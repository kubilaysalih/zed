use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    Codex,
    Claude,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chat {
    pub provider: Provider,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub archived: bool,
    pub internal: bool,
}

#[derive(Default)]
pub struct Catalog {
    pub chats: Vec<Chat>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub text: String,
    pub timestamp: String,
    pub phase: String,
    pub attachments: Vec<String>,
    pub files: Vec<String>,
    pub tool_id: String,
    pub tool_name: String,
    pub tool_input: String,
    pub tool_finished: bool,
    pub tool_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationPart {
    Message(usize),
    Work(std::ops::Range<usize>),
}

pub fn conversation_parts(messages: &[&Message]) -> Vec<ConversationPart> {
    let mut parts = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let work = message.role == "activity";
        if work {
            if let Some(ConversationPart::Work(range)) = parts.last_mut() {
                range.end = index + 1;
            } else {
                parts.push(ConversationPart::Work(index..index + 1));
            }
        } else {
            parts.push(ConversationPart::Message(index));
        }
    }
    parts
}

pub fn clean_user_text(text: &str) -> String {
    let trimmed = text.trim_start();
    let envelope = trimmed.starts_with("# Files mentioned by the user:")
        || trimmed.starts_with("# Files pasted by the user:");
    let text = if envelope {
        text.split_once("\n## My request:")
            .or_else(|| text.split_once("\n# My request:"))
            .map_or(text, |(_, request)| request)
    } else {
        text
    };
    let mut result = String::new();
    let mut fenced = false;
    let mut image_tag = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = !fenced;
        }
        let mut remaining = line;
        if !fenced {
            while let Some(start) = remaining.find("<image ") {
                let Some(end) = remaining[start..].find('>').map(|end| start + end + 1) else {
                    break;
                };
                let tag = &remaining[start..end];
                if !tag.contains("path=\"") || !tag.contains("name=") {
                    break;
                }
                result.push_str(&remaining[..start]);
                remaining = &remaining[end..];
                image_tag = true;
            }
            if image_tag {
                result.push_str(&remaining.replace("</image>", ""));
            } else {
                result.push_str(remaining);
            }
        } else {
            result.push_str(remaining);
        }
        result.push('\n');
    }
    result.trim().to_owned()
}

pub fn tool_label(message: &Message) -> String {
    let name = message.tool_name.to_lowercase();
    let label = if name.contains("exec") || name.contains("terminal") || name == "bash" {
        "Run command"
    } else if name.contains("patch") || name == "edit" || name == "write" {
        "Edit files"
    } else if name == "read" || name.contains("read_file") {
        "Read file"
    } else if name.contains("search") || name == "grep" || name == "glob" {
        "Search"
    } else if name.contains("wait") || name.contains("sleep") {
        "Wait for result"
    } else {
        return message.tool_name.clone();
    };
    label.into()
}

#[derive(Default)]
pub struct Transcript {
    pub messages: Vec<Message>,
    pub total: usize,
    pub skipped: usize,
    pub running: bool,
    pub metrics: SessionMetrics,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionMetrics {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

impl SessionMetrics {
    pub fn observe(&mut self, provider: Provider, value: &Value) {
        if provider == Provider::Codex {
            if value["type"] == "turn_context" {
                let context = &value["payload"];
                self.model = context["model"].as_str().map(str::to_owned);
                self.effort = context["effort"]
                    .as_str()
                    .or(context["reasoning_effort"].as_str())
                    .map(str::to_owned);
            }
            if value["payload"]["type"] == "token_count" {
                let info = &value["payload"]["info"];
                if let Some(tokens) = info["last_token_usage"]["total_tokens"].as_u64() {
                    self.context_tokens = Some(tokens);
                    self.context_window = info["model_context_window"]
                        .as_u64()
                        .filter(|size| *size > 0);
                }
            }
            if value["method"] == "thread/tokenUsage/updated" {
                let usage = &value["params"]["tokenUsage"];
                self.context_tokens = usage["last"]["totalTokens"].as_u64();
                self.context_window = usage["modelContextWindow"]
                    .as_u64()
                    .filter(|size| *size > 0);
            }
            if let Some(model) = value["result"]["model"].as_str() {
                self.model = Some(model.to_owned());
                self.effort = value["result"]["reasoningEffort"]
                    .as_str()
                    .map(str::to_owned);
            }
        } else {
            if value["type"] == "system" && value["subtype"] == "init" {
                self.model = value["model"].as_str().map(str::to_owned);
            }
            if value["type"] == "assistant" && value["parent_tool_use_id"].is_null() {
                let message = &value["message"];
                if let Some(model) = message["model"]
                    .as_str()
                    .filter(|model| !model.starts_with('<'))
                {
                    if self.model.as_deref() != Some(model) {
                        self.context_window = None;
                    }
                    self.model = Some(model.to_owned());
                }
                let usage = &message["usage"];
                if usage["input_tokens"].is_u64() {
                    self.context_tokens = Some(
                        [
                            "input_tokens",
                            "cache_creation_input_tokens",
                            "cache_read_input_tokens",
                            "output_tokens",
                        ]
                        .iter()
                        .filter_map(|key| usage[key].as_u64())
                        .sum(),
                    );
                }
            }
            if let Some(model) = self.model.as_deref() {
                if let Some(size) = value["modelUsage"][model]["contextWindow"]
                    .as_u64()
                    .filter(|size| *size > 0)
                {
                    self.context_window = Some(size);
                }
            }
        }
    }
}

pub fn activity_summary(message: &Message) -> String {
    let input = serde_json::from_str::<Value>(&message.tool_input).unwrap_or(Value::Null);
    let detail = [
        "description",
        "title",
        "cmd",
        "command",
        "file_path",
        "path",
        "pattern",
    ]
    .iter()
    .find_map(|key| input[key].as_str())
    .unwrap_or("");
    let label = tool_label(message);
    let summary = if detail.is_empty() {
        label
    } else {
        format!("{label} · {detail}")
    };
    summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatOptions {
    pub title: Option<String>,
    pub archived: Option<bool>,
    pub pinned: bool,
    pub deleted: bool,
    pub project: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub folders: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatNotificationStyle {
    #[default]
    System,
    Zed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAutoDismiss {
    Never,
    FiveSeconds,
    #[default]
    TenSeconds,
    ThirtySeconds,
    OneMinute,
}

impl NotificationAutoDismiss {
    pub fn seconds(self) -> Option<u64> {
        match self {
            Self::Never => None,
            Self::FiveSeconds => Some(5),
            Self::TenSeconds => Some(10),
            Self::ThirtySeconds => Some(30),
            Self::OneMinute => Some(60),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Never => "Never",
            Self::FiveSeconds => "5 seconds",
            Self::TenSeconds => "10 seconds",
            Self::ThirtySeconds => "30 seconds",
            Self::OneMinute => "1 minute",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedChatTab {
    pub chat: Option<Chat>,
    pub draft: String,
    pub attachments: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedChatGroup {
    pub tabs: Vec<SavedChatTab>,
    pub active: usize,
    pub weight: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralWorkspace {
    pub active: bool,
    pub projects: Vec<Vec<PathBuf>>,
    pub groups: Vec<SavedChatGroup>,
    pub selected_group: usize,
    pub vertical: bool,
    pub zoomed: Option<usize>,
    pub sidebar_width: Option<f32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    pub chats: HashMap<String, ChatOptions>,
    pub projects: Vec<Project>,
    pub chat_in_right_panel: bool,
    pub chat_panel_position: Option<String>,
    pub notification_auto_dismiss: NotificationAutoDismiss,
    pub notification_style: ChatNotificationStyle,
    pub unread_chats: HashSet<String>,
    pub general_workspace: GeneralWorkspace,
    pub chat_grouping: ChatGrouping,
    pub project_chat_sort: HashMap<String, ChatSort>,
    pub project_chat_order: HashMap<String, Vec<String>>,
    pub workspace_chat_scopes: HashMap<String, ChatScope>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatScope {
    All,
    Workspace,
    Project(String),
}

pub fn workspace_scope_key(folders: &[PathBuf]) -> String {
    let mut folders = folders
        .iter()
        .map(|folder| {
            let path = folder.to_string_lossy().replace('\\', "/");
            let path = path.trim_end_matches('/').to_owned();
            if cfg!(windows) {
                path.to_lowercase()
            } else {
                path
            }
        })
        .collect::<Vec<_>>();
    folders.sort();
    folders.dedup();
    // Length-prefix roots so a multi-root workspace cannot alias a single folder.
    folders
        .iter()
        .map(|folder| format!("{}:{folder}", folder.len()))
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatGrouping {
    #[default]
    Recent,
    Project,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatSort {
    #[default]
    Recent,
    Custom,
}

impl Chat {
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider.name(), self.id)
    }
}

impl Library {
    pub fn chat_project(&self, chat: &Chat) -> &str {
        self.chats
            .get(&chat.key())
            .and_then(|options| options.project.as_deref())
            .filter(|id| self.projects.iter().any(|project| project.id == *id))
            .unwrap_or("")
    }

    pub fn project_sort(&self, project: &str) -> ChatSort {
        self.project_chat_sort
            .get(project)
            .copied()
            .unwrap_or_default()
    }

    pub fn sort_project_chats(&self, project: &str, chats: &mut Vec<&Chat>) {
        let custom = self.project_sort(project) == ChatSort::Custom;
        let ranks = self
            .project_chat_order
            .get(project)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect::<HashMap<_, _>>();
        chats.sort_by_cached_key(|chat| {
            (
                if custom {
                    ranks
                        .get(chat.key().as_str())
                        .copied()
                        .unwrap_or(usize::MAX)
                } else {
                    0
                },
                std::cmp::Reverse(chat.modified),
                chat.key(),
            )
        });
    }

    pub fn reorder_project_chat(
        &mut self,
        project: &str,
        source: &str,
        target: &str,
        after: bool,
        chats: &[Chat],
    ) -> bool {
        if source == target || self.project_sort(project) != ChatSort::Custom {
            return false;
        }
        let mut members = chats
            .iter()
            .filter(|chat| self.chat_project(chat) == project)
            .collect::<Vec<_>>();
        self.sort_project_chats(project, &mut members);
        let mut order = members.iter().map(|chat| chat.key()).collect::<Vec<_>>();
        if !order.iter().any(|key| key == source) || !order.iter().any(|key| key == target) {
            return false;
        }
        order.retain(|key| key != source);
        let Some(index) = order.iter().position(|key| key == target) else {
            return false;
        };
        order.insert(index + usize::from(after), source.to_owned());
        self.project_chat_order.insert(project.to_owned(), order);
        true
    }

    pub fn in_workspace(&self, chat: &Chat) -> bool {
        self.projects
            .iter()
            .any(|project| self.contains(project, chat))
    }

    pub fn title(&self, chat: &Chat) -> String {
        self.chats
            .get(&chat.key())
            .and_then(|options| options.title.clone())
            .unwrap_or_else(|| chat.title.clone())
    }
    pub fn archived(&self, chat: &Chat) -> bool {
        self.chats
            .get(&chat.key())
            .and_then(|options| options.archived)
            .unwrap_or(chat.archived)
    }
    pub fn pinned(&self, chat: &Chat) -> bool {
        self.chats
            .get(&chat.key())
            .is_some_and(|options| options.pinned)
    }
    pub fn deleted(&self, chat: &Chat) -> bool {
        self.chats
            .get(&chat.key())
            .is_some_and(|options| options.deleted)
    }
    pub fn contains(&self, project: &Project, chat: &Chat) -> bool {
        self.chats
            .get(&chat.key())
            .and_then(|options| options.project.as_ref())
            .is_some_and(|assigned| assigned == &project.id)
    }
    pub fn read(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("Cannot read chat library preferences")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("Missing library directory")?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!("chat-library-{}.tmp", std::process::id()));
        use std::io::Write;
        let mut file = File::create(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path).context("Cannot save chat library")
    }
}

pub fn path_contains(folder: &str, child: &str) -> bool {
    let normalize = |path: &str| {
        let path = path.replace('\\', "/").trim_end_matches('/').to_owned();
        if cfg!(windows) {
            path.to_lowercase()
        } else {
            path
        }
    };
    let folder = normalize(folder);
    let child = normalize(child);
    child == folder || child.starts_with(&(folder + "/"))
}

fn is_context(text: &str) -> bool {
    let text = text.trim_start();
    [
        "# AGENTS.md instructions",
        "<environment_context>",
        "<permissions instructions>",
        "<recommended_plugins>",
        "<skills_instructions>",
        "<system-reminder>",
        "<local-command-",
        "<command-name>",
        "<send_user_message_question_reply>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

pub(crate) fn image_sources(content: &Value, text: &str) -> Vec<String> {
    let mut sources = Vec::new();
    for block in content.as_array().into_iter().flatten() {
        if block["type"] == "input_image" {
            if let Some(url) = block["image_url"].as_str() {
                sources.push(url.to_owned());
            }
        } else if block["type"] == "image" && block["source"]["type"] == "base64" {
            if let (Some(mime), Some(data)) = (
                block["source"]["media_type"].as_str(),
                block["source"]["data"].as_str(),
            ) {
                if data.len() <= 24 * 1024 * 1024 {
                    sources.push(format!("data:{mime};base64,{data}"));
                }
            }
        }
    }
    // Structured image blocks and the generated XML describe the same attachments.
    // Prefer the embedded copy, which also survives temporary file cleanup.
    if !sources.is_empty() {
        let mut seen = HashSet::new();
        sources.retain(|source| seen.insert(source.clone()));
        return sources;
    }
    for tag in text.split("<image ").skip(1) {
        let tag = tag.split('>').next().unwrap_or_default();
        if let Some(path) = tag
            .split("path=\"")
            .nth(1)
            .and_then(|value| value.split('"').next())
        {
            if Path::new(path).is_absolute() {
                sources.push(path.to_owned());
            }
        }
    }
    let mut seen = HashSet::new();
    sources.retain(|source| seen.insert(source.clone()));
    sources
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text" | "input_text" | "output_text") => {
                block.get("text").and_then(Value::as_str).map(str::to_owned)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn task_notification(text: &str, timestamp: &str) -> Option<Message> {
    let text = text.trim();
    if !text.starts_with("<task-notification>") || !text.contains("</task-notification>") {
        return None;
    }
    let field = |name: &str| {
        text.split_once(&format!("<{name}>"))
            .and_then(|(_, rest)| rest.split_once(&format!("</{name}>")))
            .map(|(value, _)| value.trim().to_owned())
            .unwrap_or_default()
    };
    let id = field("task-id");
    let status = field("status");
    let event = field("event");
    let summary = field("summary");
    if id.is_empty() || (status.is_empty() && event.is_empty() && summary.is_empty()) {
        return None;
    }
    let update = status.is_empty();
    Some(Message {
        role: "activity".into(),
        timestamp: timestamp.into(),
        tool_id: format!("task:{id}"),
        tool_name: if update {
            "Background task update".into()
        } else {
            format!("Background task · {status}")
        },
        text: [summary, event]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        tool_input: field("output-file"),
        // A monitor event is a delivered notification, not a new running command.
        tool_finished: update
            || matches!(
                status.as_str(),
                "completed" | "failed" | "killed" | "cancelled" | "stopped"
            ),
        tool_error: matches!(status.as_str(), "failed" | "killed"),
        ..Default::default()
    })
}

fn visible_message(provider: Provider, value: &Value) -> Option<Message> {
    let message = match provider {
        Provider::Codex
            if value["type"] == "response_item" && value["payload"]["type"] == "message" =>
        {
            &value["payload"]
        }
        Provider::Claude
            if (value["type"] == "user" || value["type"] == "assistant")
                && value["isSidechain"] != true =>
        {
            &value["message"]
        }
        _ => return None,
    };
    let role = string(message, "role");
    if role != "user" && role != "assistant" {
        return None;
    }
    let text = content_text(&message["content"]);
    if provider == Provider::Claude
        && value["isMeta"] == true
        && text.trim_start().starts_with("[Image: original ")
    {
        return None;
    }
    if provider == Provider::Claude && role == "user" {
        if let Some(notification) = task_notification(&text, &string(value, "timestamp")) {
            return Some(notification);
        }
    }
    let attachments = image_sources(&message["content"], &text);
    let files = if role == "user"
        && (text
            .trim_start()
            .starts_with("# Files mentioned by the user:")
            || text.trim_start().starts_with("# Files pasted by the user:"))
    {
        text.split_once("\n## My request:")
            .map(|(header, _)| {
                header
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("## ")
                            .and_then(|line| line.split_once(": "))
                            .map(|(_, path)| path.trim())
                    })
                    .filter(|path| Path::new(path).is_absolute())
                    .filter(|path| {
                        !matches!(
                            Path::new(path)
                                .extension()
                                .and_then(|extension| extension.to_str())
                                .unwrap_or_default()
                                .to_ascii_lowercase()
                                .as_str(),
                            "png" | "jpg" | "jpeg" | "webp" | "gif"
                        )
                    })
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if (text.trim().is_empty() && attachments.is_empty()) || (role == "user" && is_context(&text)) {
        return None;
    }
    Some(Message {
        role,
        text: if message["role"] == "user" {
            clean_user_text(&text)
        } else {
            text
        },
        timestamp: string(value, "timestamp"),
        phase: if provider == Provider::Codex {
            string(message, "phase")
        } else if message["stop_reason"] == "end_turn" {
            "final_answer".into()
        } else {
            String::new()
        },
        attachments,
        files,
        ..Default::default()
    })
}

fn files_in(root: &Path, depth: usize, output: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("Cannot read {}", root.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            output.push(entry.path());
        } else if kind.is_dir() && depth > 0 && entry.file_name() != "subagents" {
            files_in(&entry.path(), depth - 1, output)?;
        }
    }
    Ok(())
}

fn title_excerpt(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}

fn metadata(
    path: PathBuf,
    provider: Provider,
    archived: bool,
    titles: &HashMap<String, String>,
) -> Result<Chat> {
    let mut file = File::open(&path)?;
    let file_metadata = file.metadata()?;
    let mut chat = Chat {
        provider,
        id: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        title: String::new(),
        cwd: String::new(),
        path,
        modified: file_metadata.modified()?,
        archived,
        internal: false,
    };
    let mut first_prompt = None;
    let mut has_session_metadata = false;
    let mut subagent = false;
    for is_tail in [false, true] {
        if is_tail
            && provider == Provider::Codex
            && (titles.contains_key(&chat.id) || first_prompt.is_some())
        {
            continue;
        }
        let offset = if is_tail {
            file_metadata.len().saturating_sub(64 * 1024)
        } else {
            0
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut reader =
            BufReader::new((&mut file).take(if is_tail { 64 * 1024 } else { 1024 * 1024 }));
        if offset > 0 {
            reader.skip_until(b'\n')?;
        }
        for line in reader.split(b'\n') {
            let line = line?;
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            match provider {
                Provider::Codex if value["type"] == "session_meta" && !has_session_metadata => {
                    // Forked rollouts can include inherited session metadata later in the file.
                    has_session_metadata = true;
                    let id = string(&value["payload"], "id");
                    if !id.is_empty() {
                        chat.id = id;
                    }
                    chat.cwd = string(&value["payload"], "cwd");
                    subagent = value["payload"]["source"].get("subagent").is_some()
                        || value["payload"]["thread_source"] == "subagent";
                    chat.internal = subagent || value["payload"]["source"] == "exec";
                }
                Provider::Claude => {
                    let cwd = string(&value, "cwd");
                    if !cwd.is_empty() {
                        chat.cwd = cwd;
                    }
                    let title = if value["type"] == "custom-title" {
                        string(&value, "customTitle")
                    } else if value["type"] == "ai-title" {
                        string(&value, "aiTitle")
                    } else {
                        String::new()
                    };
                    if !title.is_empty() {
                        chat.title = title;
                    }
                }
                _ => {}
            }
            if first_prompt.is_none() {
                first_prompt = visible_message(provider, &value)
                    .filter(|message| message.role == "user")
                    .map(|message| {
                        if message.text.trim().is_empty() {
                            "Image conversation".into()
                        } else {
                            title_excerpt(&message.text)
                        }
                    });
            }
            if !is_tail
                && !chat.cwd.is_empty()
                && (titles.contains_key(&chat.id) || first_prompt.is_some())
            {
                break;
            }
        }
    }
    if let Some(title) = titles.get(&chat.id) {
        chat.title = title.clone();
        // A named desktop conversation can also use the exec origin.
        if has_session_metadata && !subagent && !chat.title.is_empty() {
            chat.internal = false;
        }
    }
    if chat.title.is_empty() {
        chat.title = first_prompt.unwrap_or_else(|| "Untitled conversation".to_owned());
    }
    Ok(chat)
}

pub fn catalog(codex_home: &Path, claude_home: &Path) -> Catalog {
    let mut catalog = Catalog::default();
    let mut titles = HashMap::new();
    let index = codex_home.join("session_index.jsonl");
    if index.exists() {
        match File::open(&index) {
            Ok(file) => {
                for line in BufReader::new(file).lines() {
                    match line {
                        Ok(line) => {
                            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                                let title = string(&value, "thread_name");
                                if !title.is_empty() && !is_context(&title) {
                                    titles.insert(string(&value, "id"), title);
                                }
                            }
                        }
                        Err(error) => {
                            catalog.warnings.push(format!("Codex index: {error}"));
                            break;
                        }
                    }
                }
            }
            Err(error) => catalog.warnings.push(format!("Codex index: {error}")),
        }
    }
    for (provider, root, depth, archived) in [
        (Provider::Codex, codex_home.join("sessions"), 3, false),
        (
            Provider::Codex,
            codex_home.join("archived_sessions"),
            3,
            true,
        ),
        (Provider::Claude, claude_home.join("projects"), 1, false),
    ] {
        let mut files = Vec::new();
        if let Err(error) = files_in(&root, depth, &mut files) {
            catalog.warnings.push(error.to_string());
        }
        for path in files {
            if path
                .file_stem()
                .is_some_and(|name| name.to_string_lossy().starts_with("agent-"))
            {
                continue;
            }
            match metadata(path.clone(), provider, archived, &titles) {
                Ok(chat) => catalog.chats.push(chat),
                Err(error) => catalog
                    .warnings
                    .push(format!("{}: {error}", path.display())),
            }
        }
    }
    catalog
        .chats
        .sort_by(|left, right| right.modified.cmp(&left.modified));
    let mut seen = HashSet::new();
    catalog
        .chats
        .retain(|chat| seen.insert((chat.provider, chat.id.clone())));
    catalog
}

pub fn local_catalog() -> Result<Catalog> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .context("Home directory is unavailable")?;
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".codex"));
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".claude"));
    Ok(catalog(&codex, &claude))
}

pub fn compaction_message(finished: bool, timestamp: String) -> Message {
    Message {
        role: "compaction".into(),
        text: if finished {
            "Context compacted"
        } else {
            "Compacting context · This can take a few minutes"
        }
        .into(),
        timestamp,
        tool_finished: finished,
        ..Default::default()
    }
}

fn records(provider: Provider, value: &Value) -> Vec<Message> {
    let compacted = match provider {
        Provider::Codex => {
            value["type"] == "compacted"
                || (value["type"] == "event_msg" && value["payload"]["type"] == "context_compacted")
        }
        Provider::Claude => value["type"] == "system" && value["subtype"] == "compact_boundary",
    };
    if compacted {
        return vec![compaction_message(true, string(value, "timestamp"))];
    }
    let mut records = visible_message(provider, value)
        .into_iter()
        .collect::<Vec<_>>();
    let timestamp = string(value, "timestamp");
    let blocks: Vec<&Value> = match provider {
        Provider::Codex if value["type"] == "response_item" => vec![&value["payload"]],
        Provider::Claude if value["type"] == "assistant" || value["type"] == "user" => {
            value["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .collect()
        }
        _ => Vec::new(),
    };
    for block in blocks {
        let kind = block["type"].as_str().unwrap_or_default();
        if matches!(kind, "tool_use" | "function_call" | "custom_tool_call") {
            let input = block
                .get("arguments")
                .or_else(|| block.get("input"))
                .unwrap_or(&Value::Null);
            records.push(Message {
                role: "activity".into(),
                timestamp: timestamp.clone(),
                tool_id: if provider == Provider::Claude {
                    string(block, "id")
                } else {
                    string(block, "call_id")
                },
                tool_name: string(block, "name"),
                tool_input: input
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| serde_json::to_string_pretty(input).unwrap_or_default()),
                ..Default::default()
            });
        } else if matches!(
            kind,
            "tool_result" | "function_call_output" | "custom_tool_call_output"
        ) {
            let output = block
                .get("output")
                .or_else(|| block.get("content"))
                .unwrap_or(&Value::Null);
            let text = content_text(output);
            let attachments = image_sources(output, "");
            let inline_image = provider == Provider::Claude && !attachments.is_empty();
            records.push(Message {
                role: "tool_result".into(),
                timestamp: timestamp.clone(),
                tool_id: if provider == Provider::Claude {
                    string(block, "tool_use_id")
                } else {
                    string(block, "call_id")
                },
                text: if text.is_empty() && !attachments.is_empty() {
                    "Image result".into()
                } else if text.is_empty() {
                    output.to_string()
                } else {
                    text
                },
                tool_finished: true,
                tool_error: block["is_error"] == true,
                attachments: if inline_image {
                    Vec::new()
                } else {
                    attachments.clone()
                },
                ..Default::default()
            });
            if inline_image {
                records.push(Message {
                    role: "assistant".into(),
                    timestamp: timestamp.clone(),
                    attachments,
                    ..Default::default()
                });
            }
        }
    }
    records
}

#[derive(Clone, Debug)]
pub struct ChatActivity {
    pub modified: SystemTime,
    pub running: bool,
    pub reply: Option<String>,
}

impl ChatActivity {
    pub fn completed_since(&self, previous: Option<&Self>) -> bool {
        !self.running
            && self.reply.is_some()
            && previous.is_some_and(|old| old.running && old.reply != self.reply)
    }
}

pub fn chat_activity(chat: &Chat, previous: Option<&ChatActivity>) -> Result<ChatActivity> {
    let mut file = File::open(&chat.path)?;
    let metadata = file.metadata()?;
    let modified = metadata.modified()?;
    if let Some(previous) = previous.filter(|previous| previous.modified == modified) {
        let mut previous = previous.clone();
        if modified
            .elapsed()
            .is_ok_and(|age| age > std::time::Duration::from_secs(90))
        {
            previous.running = false;
        }
        return Ok(previous);
    }
    let mut activity = ChatActivity {
        modified,
        running: previous.is_some_and(|previous| previous.running),
        reply: previous.and_then(|previous| previous.reply.clone()),
    };
    let offset = metadata.len().saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file.take(256 * 1024));
    if offset > 0 {
        reader.skip_until(b'\n')?;
    }
    for line in reader.split(b'\n') {
        let line = line?;
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let mut reply = false;
        match chat.provider {
            Provider::Codex => {
                if value["type"] == "event_msg" {
                    match value["payload"]["type"].as_str() {
                        Some("task_started" | "turn_started") => activity.running = true,
                        Some("task_complete" | "task_completed") => {
                            activity.running = false;
                            reply = true;
                        }
                        Some("turn_aborted") => activity.running = false,
                        _ => {}
                    }
                }
                if value["type"] == "response_item"
                    && value["payload"]["type"] == "message"
                    && value["payload"]["role"] == "assistant"
                    && value["payload"]["phase"] == "final_answer"
                {
                    activity.running = false;
                    reply = true;
                }
            }
            Provider::Claude
                if value["isSidechain"] != true && value["parent_tool_use_id"].is_null() =>
            {
                if value["type"] == "user" {
                    activity.running = true;
                }
                if value["type"] == "assistant" {
                    match value["message"]["stop_reason"].as_str() {
                        Some("end_turn" | "stop_sequence") => {
                            activity.running = false;
                            reply = true;
                        }
                        Some("tool_use") => activity.running = true,
                        _ => {}
                    }
                }
                if value["type"] == "system" && value["subtype"] == "turn_duration" {
                    activity.running = false;
                }
            }
            _ => {}
        }
        if reply {
            activity.reply = Some(format!(
                "{}:{}",
                string(&value, "timestamp"),
                string(&value, "uuid")
            ));
        }
    }
    // Crashed or abandoned external sessions cannot keep a sidebar spinner alive forever.
    if modified
        .elapsed()
        .is_ok_and(|age| age > std::time::Duration::from_secs(90))
    {
        activity.running = false;
    }
    Ok(activity)
}

pub fn transcript(chat: &Chat, limit: usize) -> Result<Transcript> {
    let file =
        File::open(&chat.path).with_context(|| format!("Cannot open {}", chat.path.display()))?;
    let length = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(length));
    let mut line = Vec::new();
    let mut messages: VecDeque<Message> = VecDeque::new();
    let mut transcript = Transcript::default();
    let mut seen = HashSet::new();
    loop {
        line.clear();
        if (&mut reader)
            .take(32 * 1024 * 1024)
            .read_until(b'\n', &mut line)?
            == 0
        {
            break;
        }
        if line.len() == 32 * 1024 * 1024 && line.last() != Some(&b'\n') {
            reader.skip_until(b'\n')?;
            transcript.skipped += 1;
            continue;
        }
        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                transcript.skipped += 1;
                continue;
            }
        };
        transcript.metrics.observe(chat.provider, &value);
        match value["payload"]["type"].as_str() {
            Some("task_started" | "turn_started") => transcript.running = true,
            Some("task_complete" | "task_completed" | "turn_aborted") => transcript.running = false,
            _ => {}
        }
        if chat.provider == Provider::Claude && value["message"]["stop_reason"] == "end_turn" {
            transcript.running = false;
        }
        let parsed = records(chat.provider, &value);
        if !parsed.is_empty() {
            // Claude can repeat a transcript UUID after a resume; Codex message IDs are optional.
            let id = if chat.provider == Provider::Claude {
                string(&value, "uuid")
            } else {
                string(&value["payload"], "id")
            };
            if !id.is_empty() && !seen.insert(id) {
                continue;
            }
            for message in parsed {
                if message.role == "tool_result" {
                    if let Some(call) = messages
                        .iter_mut()
                        .rev()
                        .find(|call| call.role == "activity" && call.tool_id == message.tool_id)
                    {
                        call.text = message.text;
                        call.tool_finished = true;
                        call.tool_error = message.tool_error;
                        call.attachments.extend(message.attachments);
                    }
                    continue;
                }
                if chat.provider == Provider::Claude
                    && message.role == "activity"
                    && !message.tool_finished
                {
                    transcript.running = true;
                }
                transcript.total += 1;
                messages.push_back(message);
                if messages.len() > limit {
                    messages.pop_front();
                }
            }
        }
    }
    transcript.messages = messages.into();
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_uses_latest_request_and_includes_claude_cached_input() {
        let mut metrics = SessionMetrics::default();
        metrics.observe(
            Provider::Codex,
            &json!({"type":"turn_context","payload":{"model":"test-model","effort":"high"}}),
        );
        metrics.observe(Provider::Codex, &json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":9000000},"last_token_usage":{"total_tokens":1250},"model_context_window":10000}}}));
        assert_eq!(metrics.model.as_deref(), Some("test-model"));
        assert_eq!(metrics.effort.as_deref(), Some("high"));
        assert_eq!(metrics.context_tokens, Some(1250));
        metrics.observe(Provider::Codex, &json!({"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"totalTokens":800},"modelContextWindow":10000}}}));
        assert_eq!(metrics.context_tokens, Some(800));
        let mut claude = SessionMetrics::default();
        claude.observe(Provider::Claude, &json!({"type":"assistant","message":{"model":"claude-test","usage":{"input_tokens":2,"cache_read_input_tokens":100,"cache_creation_input_tokens":20,"output_tokens":3}}}));
        assert_eq!(claude.context_tokens, Some(125));
        assert_eq!(claude.context_window, None);
        assert_eq!(claude.effort, None);
        claude.observe(
            Provider::Claude,
            &json!({"type":"result","modelUsage":{"claude-test":{"contextWindow":1000}}}),
        );
        assert_eq!(claude.context_window, Some(1000));
    }

    #[test]
    fn activity_summary_uses_public_description_and_stays_compact() {
        let message = Message {
            tool_name: "exec".into(),
            tool_input:
                json!({"description":"Checking\nquota metadata","cmd":"secret-long-command"})
                    .to_string(),
            ..Default::default()
        };
        assert_eq!(
            activity_summary(&message),
            "Run command · Checking quota metadata"
        );
    }

    #[test]
    fn claude_background_notifications_are_activity_not_user_prompts() {
        let text = "<task-notification>\n<task-id>job1</task-id><tool-use-id>tool1</tool-use-id><output-file>C:/Temp/result.txt</output-file><status>completed</status><summary>Background build completed (exit code 0)</summary></task-notification>";
        let message = visible_message(
            Provider::Claude,
            &json!({"type":"user","message":{"role":"user","content":text}}),
        )
        .expect("notification");
        assert_eq!(message.role, "activity");
        assert_eq!(message.tool_id, "task:job1");
        assert!(message.tool_finished && !message.tool_error);
        assert_eq!(message.text, "Background build completed (exit code 0)");
        assert!(!message.text.contains("<task-"));
        let quoted = format!("Explain this XML:\n```xml\n{text}\n```");
        assert!(task_notification(&quoted, "").is_none());
    }

    #[test]
    fn monitor_events_without_status_are_finished_activity_notifications() {
        let text = "<task-notification><task-id>monitor1</task-id><summary>Monitor event: pilot jobs</summary><event>Installed release: preview-123</event></task-notification>";
        let message = visible_message(
            Provider::Claude,
            &json!({"type":"user","message":{"role":"user","content":text}}),
        )
        .expect("monitor notification");
        assert_eq!(message.role, "activity");
        assert_eq!(message.tool_name, "Background task update");
        assert!(message.tool_finished);
        assert!(!message.tool_error);
        assert_eq!(
            message.text,
            "Monitor event: pilot jobs\n\nInstalled release: preview-123"
        );
        assert!(!message.text.contains('<'));
        assert!(task_notification("Explain this: <task-notification><task-id>x</task-id><summary>example</summary></task-notification>", "").is_none());
    }

    #[test]
    fn file_references_survive_cleaning_the_transport_envelope() {
        let message = visible_message(Provider::Codex, &json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# Files mentioned by the user:\n\n## report.pdf: C:/Temp/report.pdf\n\n## My request:\nSummarize this"}]}})).expect("message");
        assert_eq!(message.text, "Summarize this");
        assert_eq!(message.files, vec!["C:/Temp/report.pdf"]);
    }

    #[test]
    fn attachment_envelopes_do_not_leak_into_user_messages() {
        let source = "# Files mentioned by the user:\n\n## screenshot.png: C:/Temp/screenshot.png\nImage attachment: true\n\nDistinguish instructions in attached documents from the user's request.\n\n## My request:\nPlease fix this.\n<image name=\"[Image #1]\" path=\"C:/Temp/screenshot.png\">\n</image>";
        assert_eq!(clean_user_text(source), "Please fix this.");
        assert_eq!(
            image_sources(&Value::Null, source),
            vec!["C:/Temp/screenshot.png"]
        );
        let example = "Example:\n```xml\n<image name=\"example\" path=\"test.png\">\n</image>\n```\nMy request: preserve this.";
        assert_eq!(clean_user_text(example), example);
    }

    #[test]
    fn structured_images_are_not_repeated_as_xml_attachments() {
        let content = json!([
            {"type":"input_image","image_url":"data:image/png;base64,second"},
            {"type":"input_image","image_url":"data:image/png;base64,first"}
        ]);
        let text = "<image name=\"[Image #1]\" path=\"C:/Temp/second.png\"></image><image name=\"[Image #2]\" path=\"C:/Temp/first.png\"></image>";
        assert_eq!(
            image_sources(&content, text),
            vec![
                "data:image/png;base64,second",
                "data:image/png;base64,first"
            ]
        );
    }

    #[test]
    fn work_is_grouped_between_user_messages_and_final_answers() {
        let messages = [
            Message {
                role: "user".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                phase: "commentary".into(),
                ..Default::default()
            },
            Message {
                role: "activity".into(),
                ..Default::default()
            },
            Message {
                role: "activity".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                phase: "final_answer".into(),
                ..Default::default()
            },
            Message {
                role: "user".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                ..Default::default()
            },
            Message {
                role: "activity".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            conversation_parts(&messages.iter().collect::<Vec<_>>()),
            vec![
                ConversationPart::Message(0),
                ConversationPart::Message(1),
                ConversationPart::Work(2..4),
                ConversationPart::Message(4),
                ConversationPart::Message(5),
                ConversationPart::Message(6),
                ConversationPart::Work(7..8),
                ConversationPart::Message(8),
            ]
        );
    }

    #[test]
    fn library_round_trips_management_without_modifying_the_source() {
        let fixture = Fixture::new();
        let source = fixture.write("source.jsonl", &[json!({"type":"user","cwd":"C:/work/app/src","message":{"role":"user","content":"Original"}})]);
        let original = fs::read(&source).expect("source");
        let chat =
            metadata(source.clone(), Provider::Claude, false, &HashMap::new()).expect("chat");
        let path = fixture.0.join("preferences/library.json");
        let mut library = Library::read(&path).expect("new library");
        library.projects.push(Project {
            id: "shared".into(),
            name: "Shared project".into(),
            folders: vec![PathBuf::from("C:/work/app")],
        });
        assert!(!library.contains(&library.projects[0], &chat));
        library.chats.insert(
            chat.key(),
            ChatOptions {
                title: Some("Renamed".into()),
                archived: Some(true),
                pinned: true,
                deleted: true,
                project: Some("other".into()),
                model: Some("model-a".into()),
                effort: Some("high".into()),
            },
        );
        library.save(&path).expect("save");
        let mut library = Library::read(&path).expect("reload");
        assert_eq!(library.title(&chat), "Renamed");
        assert_eq!(library.chats[&chat.key()].model.as_deref(), Some("model-a"));
        assert_eq!(library.chats[&chat.key()].effort.as_deref(), Some("high"));
        assert!(library.archived(&chat) && library.pinned(&chat) && library.deleted(&chat));
        assert!(!library.contains(&library.projects[0], &chat));
        let options = library.chats.get_mut(&chat.key()).expect("options");
        options.archived = Some(false);
        options.deleted = false;
        options.project = None;
        library.save(&path).expect("replace existing library");
        let library = Library::read(&path).expect("restored");
        assert!(!library.archived(&chat) && !library.deleted(&chat));
        assert!(!library.contains(&library.projects[0], &chat));
        assert_eq!(fs::read(source).expect("source unchanged"), original);
        assert!(!path_contains("C:/work/app", "C:/work/application"));
        assert!(path_contains("C:/work/app/", "C:/work/app/src"));
        fs::write(&path, "broken json").expect("corrupt preferences");
        assert!(Library::read(&path).is_err());
    }

    #[test]
    fn injected_context_and_named_subagents_do_not_become_normal_chats() {
        let fixture = Fixture::new();
        let path = fixture.write("chat.jsonl", &[
            json!({"type":"session_meta","payload":{"id":"child","cwd":"/work","source":{"subagent":{"thread_spawn":{}}}}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"<recommended_plugins>internal</recommended_plugins>"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"My real request"}}),
        ]);
        let chat = metadata(
            path,
            Provider::Codex,
            false,
            &HashMap::from([("child".into(), "Named helper".into())]),
        )
        .expect("metadata");
        assert!(chat.internal);
        let transcript = transcript(&chat, 200).expect("transcript");
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].text, "My real request");
    }

    #[test]
    fn codex_tool_results_are_attached_to_calls_and_turn_completion_is_recorded() {
        let fixture = Fixture::new();
        let path = fixture.write("tools.jsonl", &[
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"one","name":"exec_command","arguments":"{\"cmd\":\"git status\"}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"one","output":"Clean working tree"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        ]);
        let chat = metadata(path, Provider::Codex, false, &HashMap::new()).expect("chat");
        let transcript = transcript(&chat, 200).expect("transcript");
        assert!(!transcript.running);
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].tool_name, "exec_command");
        assert!(transcript.messages[0].tool_finished);
        assert_eq!(transcript.messages[0].text, "Clean working tree");
    }

    #[test]
    fn claude_tool_errors_and_images_are_preserved_without_private_thinking() {
        let fixture = Fixture::new();
        let path = fixture.write("tools.jsonl", &[
            json!({"type":"assistant","uuid":"call","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private"},{"type":"tool_use","id":"one","name":"Read","input":{"file_path":"missing"}}]}}),
            json!({"type":"user","uuid":"result","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"one","is_error":true,"content":"Not found"}]}}),
            json!({"type":"user","uuid":"image","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]}}),
            json!({"type":"assistant","uuid":"answer","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}}),
        ]);
        let chat = metadata(path, Provider::Claude, false, &HashMap::new()).expect("chat");
        let transcript = transcript(&chat, 200).expect("transcript");
        assert_eq!(transcript.messages.len(), 3);
        assert!(transcript.messages[0].tool_error && transcript.messages[0].tool_finished);
        assert_eq!(
            transcript.messages[1].attachments,
            ["data:image/png;base64,aGVsbG8="]
        );
        assert!(!transcript.running);
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "zed-chat-history-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            fs::create_dir_all(&path).expect("fixture directory");
            Self(path)
        }
        fn write(&self, name: &str, values: &[Value]) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().expect("parent")).expect("directories");
            let text = values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            fs::write(&path, text).expect("fixture file");
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove fixture");
        }
    }

    #[test]
    fn catalogs_all_projects_and_archives_deduplicates_and_ignores_subagents() {
        let fixture = Fixture::new();
        fixture.write(
            "codex/session_index.jsonl",
            &[json!({"id":"abc","thread_name":"Saved title"})],
        );
        let metadata = json!({"type":"session_meta","payload":{"id":"abc","cwd":"/project/a"}});
        fixture.write(
            "codex/sessions/2026/11/01/rollout-abc.jsonl",
            &[metadata.clone()],
        );
        fixture.write("codex/archived_sessions/rollout-abc.jsonl", &[metadata]);
        fixture.write("claude/projects/a/first.jsonl", &[json!({"type":"ai-title","aiTitle":"First"}), json!({"type":"user","cwd":"/project/a","message":{"role":"user","content":"Hello"}})]);
        fixture.write("claude/projects/b/second.jsonl", &[json!({"type":"user","cwd":"/project/b","message":{"role":"user","content":"Merhaba dünya"}})]);
        fixture.write(
            "claude/projects/b/second/subagents/agent-child.jsonl",
            &[json!({"type":"user","message":{"role":"user","content":"Hidden"}})],
        );
        let catalog = catalog(&fixture.0.join("codex"), &fixture.0.join("claude"));
        assert!(catalog.warnings.is_empty());
        assert_eq!(catalog.chats.len(), 3);
        assert!(catalog.chats.iter().any(|chat| chat.title == "Saved title"));
        assert!(
            catalog
                .chats
                .iter()
                .any(|chat| chat.title == "Merhaba dünya" && chat.cwd == "/project/b")
        );
    }

    #[test]
    fn paging_preserves_order_handles_partial_writes_and_duplicate_messages() {
        let fixture = Fixture::new();
        let one = json!({"type":"user","uuid":"one","message":{"role":"user","content":"First"}});
        let path = fixture.write("chat.jsonl", &[one.clone(), one, json!({"type":"assistant","uuid":"two","message":{"role":"assistant","content":[{"type":"text","text":"Second"}]}})]);
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("file")
            .write_all(b"{\"type\":")
            .expect("partial append");
        let chat = metadata(path, Provider::Claude, false, &HashMap::new()).expect("metadata");
        let page = transcript(&chat, 1).expect("page");
        assert_eq!(page.total, 2);
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].text, "Second");
        assert_eq!(page.skipped, 1);
        let all = transcript(&chat, 200).expect("all");
        assert_eq!(all.messages[0].text, "First");
    }

    #[test]
    fn inherited_metadata_does_not_replace_fork_identity() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "fork.jsonl",
            &[
                json!({"type":"session_meta","payload":{"id":"fork","cwd":"/new"}}),
                json!({"type":"session_meta","payload":{"id":"original","cwd":"/old"}}),
            ],
        );
        let chat = metadata(path, Provider::Codex, false, &HashMap::new()).expect("metadata");
        assert_eq!(chat.id, "fork");
        assert_eq!(chat.cwd, "/new");
    }

    #[test]
    fn codex_uses_messages_not_duplicate_events_or_private_reasoning() {
        let message = json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer"}]}});
        assert_eq!(
            visible_message(Provider::Codex, &message)
                .expect("message")
                .text,
            "Answer"
        );
        for value in [
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"Answer"}}),
            json!({"type":"response_item","payload":{"type":"reasoning","summary":[]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"system","content":"instructions"}}),
        ] {
            assert!(visible_message(Provider::Codex, &value).is_none());
        }
    }

    #[test]
    fn claude_keeps_text_and_attachment_markers_not_tool_results() {
        let value = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"Hello"},{"type":"image","source":{"data":"secret bytes"}},{"type":"tool_result","content":"tool output"}]}});
        assert_eq!(
            visible_message(Provider::Claude, &value)
                .expect("message")
                .text,
            "Hello"
        );
        let value = json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"output"}]}});
        assert!(visible_message(Provider::Claude, &value).is_none());
    }
    #[test]
    fn historical_compaction_is_a_status_row_without_internal_summary() {
        for (provider, value) in [
            (
                Provider::Codex,
                json!({"type":"compacted","timestamp":"today","payload":{"message":"private compaction summary"}}),
            ),
            (
                Provider::Claude,
                json!({"type":"system","subtype":"compact_boundary","timestamp":"today"}),
            ),
        ] {
            let rows = records(provider, &value);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].role, "compaction");
            assert_eq!(rows[0].text, "Context compacted");
            assert!(rows[0].tool_finished);
            assert!(matches!(
                conversation_parts(&[&rows[0]]).as_slice(),
                [ConversationPart::Message(0)]
            ));
        }
    }

    #[test]
    fn manually_assigned_codex_and_claude_chats_share_a_project() {
        let project = Project {
            id: "sevk".into(),
            name: "sevk".into(),
            folders: vec![],
        };
        let fixture = Fixture::new();
        let path = fixture.0.join("library.json");
        let mut library = Library::default();
        let chats = [Provider::Codex, Provider::Claude].map(|provider| Chat {
            provider,
            id: "same-id".into(),
            title: "Chat".into(),
            cwd: "C:/elsewhere".into(),
            path: PathBuf::new(),
            modified: SystemTime::now(),
            archived: false,
            internal: false,
        });
        for chat in &chats {
            library.chats.entry(chat.key()).or_default().project = Some(project.id.clone());
        }
        library.save(&path).expect("save");
        let library = Library::read(&path).expect("read");
        assert_eq!(library.chats.len(), 2);
        assert!(chats.iter().all(|chat| library.contains(&project, chat)));
    }
    #[test]
    fn activity_changes_from_running_to_reply_and_read_status_persists() {
        let fixture = Fixture::new();
        let path = fixture.write("session.jsonl", &[
            json!({"type":"session_meta","payload":{"id":"s","cwd":"C:/test"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Hi"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
        ]);
        let chat = metadata(path.clone(), Provider::Codex, false, &HashMap::new()).expect("chat");
        let running = chat_activity(&chat, None).expect("running status");
        assert!(running.running);
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(
            file,
            "{}",
            json!({"type":"event_msg","timestamp":"reply-1","payload":{"type":"task_complete"}})
        )
        .expect("write");
        let mut previous = running;
        previous.modified = SystemTime::UNIX_EPOCH;
        let finished = chat_activity(&chat, Some(&previous)).expect("finished status");
        assert!(!finished.running);
        assert_eq!(finished.reply.as_deref(), Some("reply-1:"));
        let library_path = fixture.0.join("library.json");
        let mut library = Library::default();
        library.unread_chats.insert(chat.key());
        library.save(&library_path).expect("save unread");
        let mut loaded = Library::read(&library_path).expect("reload");
        assert!(loaded.unread_chats.remove(&chat.key()));
        loaded.save(&library_path).expect("save read");
        assert!(
            Library::read(&library_path)
                .expect("reload read")
                .unread_chats
                .is_empty()
        );
    }
    #[test]
    fn completion_notification_ignores_history_repeats_and_aborted_turns() {
        let running = ChatActivity {
            modified: SystemTime::now(),
            running: true,
            reply: Some("old".into()),
        };
        let done = ChatActivity {
            running: false,
            reply: Some("new".into()),
            ..running.clone()
        };
        assert!(done.completed_since(Some(&running)));
        assert!(!done.completed_since(None));
        assert!(!done.completed_since(Some(&done)));
        assert!(!running.completed_since(Some(&running)));
        let stopped = ChatActivity {
            running: false,
            ..running.clone()
        };
        assert!(!stopped.completed_since(Some(&running)));
        let no_reply = ChatActivity {
            running: false,
            reply: None,
            ..running.clone()
        };
        assert!(!no_reply.completed_since(Some(&running)));
    }

    #[test]
    fn workspace_chat_filter_uses_manual_projects_independently_of_agent_folders() {
        let mut library = Library::default();
        library.general_workspace.projects = vec![vec![PathBuf::from("C:/work/sevk")]];
        let project = Project {
            id: "sevk".into(),
            name: "Sevk".into(),
            folders: vec![PathBuf::from("C:/work/sevk/app")],
        };
        library.projects.push(project);
        let mut chat = Chat {
            provider: Provider::Codex,
            id: "test".into(),
            title: "test".into(),
            cwd: "C:/work/sevk/app/src".into(),
            path: PathBuf::new(),
            modified: SystemTime::UNIX_EPOCH,
            archived: false,
            internal: false,
        };
        assert!(!library.in_workspace(&chat));
        chat.cwd = "C:/work/sevk-other".into();
        assert!(!library.in_workspace(&chat));
        library.chats.entry(chat.key()).or_default().project = Some("sevk".into());
        assert!(library.in_workspace(&chat));
        library.general_workspace.projects.clear();
        library.projects[0].folders.clear();
        assert!(library.in_workspace(&chat));
        library
            .chats
            .get_mut(&chat.key())
            .expect("assignment")
            .project = None;
        chat.cwd = "C:/work/sevk/app/src".into();
        assert!(!library.in_workspace(&chat));
    }

    #[test]
    fn claude_tool_images_are_visible_outside_activity_and_companion_notes_are_hidden() {
        let fixture = Fixture::new();
        let path = fixture.write("images.jsonl", &[
            json!({"type":"assistant","uuid":"call","message":{"role":"assistant","content":[{"type":"tool_use","id":"read","name":"Read","input":{}}]}}),
            json!({"type":"user","uuid":"result","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read","content":[{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"aGVsbG8="}}]}]}}),
            json!({"type":"user","uuid":"note","isMeta":true,"turnCompanion":true,"message":{"role":"user","content":"[Image: original 1450x3150, displayed at 921x2000. Multiply coordinates by 1.57 to map to original image.]"}}),
        ]);
        let chat = metadata(path, Provider::Claude, false, &HashMap::new()).expect("chat");
        let transcript = transcript(&chat, 200).expect("transcript");
        assert_eq!(transcript.messages.len(), 2);
        assert!(transcript.messages[0].tool_finished);
        assert!(transcript.messages[0].attachments.is_empty());
        assert!(!transcript.messages[0].text.contains("aGVsbG8="));
        assert_eq!(transcript.messages[1].role, "assistant");
        assert_eq!(
            transcript.messages[1].attachments,
            ["data:image/jpeg;base64,aGVsbG8="]
        );
    }

    #[test]
    fn project_order_is_manual_persistent_and_independent_of_updates_and_other_projects() {
        let fixture = Fixture::new();
        let path = fixture.0.join("library.json");
        let mut library = Library::default();
        library.projects = ["a", "b"]
            .map(|id| Project {
                id: id.into(),
                name: id.into(),
                folders: vec![],
            })
            .into();
        let mut chats = (1..=4)
            .map(|number| Chat {
                provider: Provider::Codex,
                id: number.to_string(),
                title: number.to_string(),
                cwd: "C:/same-folder".into(),
                path: PathBuf::new(),
                modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(number),
                archived: false,
                internal: false,
            })
            .collect::<Vec<_>>();
        for (index, chat) in chats.iter().enumerate() {
            library.chats.entry(chat.key()).or_default().project =
                Some(if index == 3 { "b" } else { "a" }.into());
        }
        fn ordered(library: &Library, chats: &[Chat]) -> Vec<String> {
            let mut members = chats
                .iter()
                .filter(|chat| library.chat_project(chat) == "a")
                .collect();
            library.sort_project_chats("a", &mut members);
            members.iter().map(|chat| chat.id.clone()).collect()
        }
        assert_eq!(ordered(&library, &chats), ["3", "2", "1"]);
        library.chat_grouping = ChatGrouping::Project;
        library
            .project_chat_sort
            .insert("a".into(), ChatSort::Custom);
        assert!(library.reorder_project_chat("a", &chats[0].key(), &chats[2].key(), true, &chats));
        assert_eq!(ordered(&library, &chats), ["3", "1", "2"]);
        assert!(!library.reorder_project_chat("a", &chats[0].key(), &chats[3].key(), true, &chats));
        chats[1].modified += std::time::Duration::from_secs(100);
        assert_eq!(ordered(&library, &chats), ["3", "1", "2"]);
        library.save(&path).expect("save custom order");
        let mut restored = Library::read(&path).expect("read order");
        assert_eq!(restored.chat_grouping, ChatGrouping::Project);
        assert_eq!(ordered(&restored, &chats), ["3", "1", "2"]);
        assert_eq!(restored.project_sort("b"), ChatSort::Recent);
        restored
            .project_chat_sort
            .insert("a".into(), ChatSort::Recent);
        assert_eq!(ordered(&restored, &chats), ["2", "3", "1"]);
        restored
            .project_chat_sort
            .insert("a".into(), ChatSort::Custom);
        assert_eq!(ordered(&restored, &chats), ["3", "1", "2"]);
        // Moving only the visible source and target must preserve the middle chat too.
        assert!(restored.reorder_project_chat("a", &chats[2].key(), &chats[1].key(), true, &chats));
        assert_eq!(ordered(&restored, &chats), ["1", "2", "3"]);
        assert_eq!(restored.chat_project(&chats[3]), "b");
    }

    #[test]
    fn folder_scope_and_manual_assignments_survive_general_workspace_changes() {
        let fixture = Fixture::new();
        let path = fixture.0.join("library.json");
        let folders = vec![PathBuf::from("C:/projects/sevk")];
        let key = workspace_scope_key(&folders);
        let mut library = Library::default();
        library.projects.push(Project {
            id: "manual-sevk".into(),
            name: "Sevk".into(),
            folders: vec![],
        });
        library
            .workspace_chat_scopes
            .insert(key.clone(), ChatScope::Project("manual-sevk".into()));
        for provider in [Provider::Codex, Provider::Claude] {
            library.chats.insert(
                format!("{}:session", provider.name()),
                ChatOptions {
                    project: Some("manual-sevk".into()),
                    ..Default::default()
                },
            );
        }
        library.general_workspace.projects =
            vec![folders.clone(), vec![PathBuf::from("C:/projects/leartes")]];
        library.save(&path).expect("save general mappings");
        let mut standalone = Library::read(&path).expect("open single folder");
        assert_eq!(
            standalone.workspace_chat_scopes.get(&key),
            Some(&ChatScope::Project("manual-sevk".into()))
        );
        standalone.general_workspace = GeneralWorkspace::default();
        standalone.save(&path).expect("close general workspace");
        let reopened = Library::read(&path).expect("reopen folder");
        for provider in [Provider::Codex, Provider::Claude] {
            assert_eq!(
                reopened.chats[&format!("{}:session", provider.name())]
                    .project
                    .as_deref(),
                Some("manual-sevk")
            );
        }
        assert_eq!(
            workspace_scope_key(&[PathBuf::from("C:\\projects\\sevk\\")]),
            key
        );
        assert_ne!(
            workspace_scope_key(&[folders[0].clone(), PathBuf::from("C:/projects/leartes")]),
            key
        );
        assert!(
            !reopened
                .workspace_chat_scopes
                .contains_key(&workspace_scope_key(&[PathBuf::from(
                    "C:/projects/sevk/subfolder"
                )]))
        );
    }

    #[test]
    fn general_workspace_restores_projects_chat_panes_and_unsent_drafts() {
        let fixture = Fixture::new();
        let path = fixture.0.join("library.json");
        fs::write(&path, r#"{"chats":{},"projects":[]}"#).expect("old library");
        let mut library = Library::read(&path).expect("backward compatible");
        assert!(!library.general_workspace.active);
        let chat = Chat {
            provider: Provider::Claude,
            id: "session-a".into(),
            title: "A".into(),
            cwd: "C:/projects/a".into(),
            path: PathBuf::from("C:/sessions/a.jsonl"),
            modified: SystemTime::UNIX_EPOCH,
            archived: false,
            internal: false,
        };
        library.general_workspace = GeneralWorkspace {
            active: true,
            projects: vec![
                vec![PathBuf::from("C:/projects/a")],
                vec![PathBuf::from("C:/projects/b")],
            ],
            groups: vec![
                SavedChatGroup {
                    tabs: vec![SavedChatTab {
                        chat: Some(chat.clone()),
                        draft: "not sent yet".into(),
                        attachments: vec![PathBuf::from("C:/image.png")],
                    }],
                    active: 0,
                    weight: 2.,
                },
                SavedChatGroup {
                    weight: 1.,
                    ..Default::default()
                },
            ],
            selected_group: 1,
            vertical: true,
            zoomed: Some(0),
            sidebar_width: Some(350.),
        };
        library.save(&path).expect("save layout");
        let restored = Library::read(&path)
            .expect("restore layout")
            .general_workspace;
        assert!(restored.active && restored.vertical);
        assert_eq!(restored.projects.len(), 2);
        assert_eq!(restored.groups.len(), 2);
        assert_eq!(
            restored.groups[0].tabs[0]
                .chat
                .as_ref()
                .expect("chat")
                .key(),
            chat.key()
        );
        assert_eq!(
            restored.groups[0].tabs[0].chat.as_ref().expect("chat").cwd,
            "C:/projects/a"
        );
        assert_eq!(restored.groups[0].tabs[0].draft, "not sent yet");
        assert_eq!(
            restored.groups[0].tabs[0].attachments,
            vec![PathBuf::from("C:/image.png")]
        );
        assert_eq!(restored.groups[0].weight, 2.);
        assert_eq!(restored.selected_group, 1);
        assert_eq!(restored.zoomed, Some(0));
        assert_eq!(restored.sidebar_width, Some(350.));
    }
}
