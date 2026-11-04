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
    /// Time of the latest human message, used for sidebar recency.
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
    pub subtask: Option<ClaudeSubtask>,
    pub question_answers: HashMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeSubtask {
    pub id: String,
    pub status: String,
}

impl ClaudeSubtask {
    pub fn running(&self) -> bool {
        matches!(
            self.status.as_str(),
            "running" | "async_launched" | "in_progress" | "waiting"
        )
    }
}

pub fn claude_async_subtask(value: &Value, block: &Value) -> Option<ClaudeSubtask> {
    let result = &value["toolUseResult"];
    let output = content_text(&block["content"]);
    let structured = result["isAsync"] == true || result["status"] == "async_launched";
    if !structured && !output.starts_with("Async agent launched successfully.") {
        return None;
    }
    let id = result["agentId"].as_str().or_else(|| {
        output
            .lines()
            .find_map(|line| line.strip_prefix("agentId: ")?.split_whitespace().next())
    })?;
    // IDs become filenames when checking the child transcript.
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return None;
    }
    Some(ClaudeSubtask {
        id: id.into(),
        status: "running".into(),
    })
}

pub fn claude_task_event(value: &Value) -> Option<Message> {
    if value["type"] != "system"
        || value["subtype"] != "task_notification"
        || !value["parent_tool_use_id"].is_null()
    {
        return None;
    }
    let id = value["task_id"].as_str()?;
    let status = value["status"].as_str()?;
    if !matches!(
        status,
        "completed" | "failed" | "killed" | "cancelled" | "stopped"
    ) {
        return None;
    }
    Some(Message {
        role: "activity".into(),
        timestamp: string(value, "timestamp"),
        tool_id: format!("task:{id}"),
        tool_name: format!("Background task · {status}"),
        tool_input: string(value, "output_file"),
        text: string(value, "summary"),
        tool_finished: true,
        tool_error: matches!(status, "failed" | "killed"),
        subtask: Some(ClaudeSubtask {
            id: id.into(),
            status: status.into(),
        }),
        ..Default::default()
    })
}

#[derive(Clone, Debug, Default)]
pub struct ClaudeSubtasks {
    pub tasks: HashMap<String, ClaudeSubtask>,
    offset: u64,
    child_versions: HashMap<String, (u64, SystemTime)>,
}

impl ClaudeSubtasks {
    pub fn pending(&self) -> usize {
        self.tasks.values().filter(|task| task.running()).count()
    }

    pub fn observe(&mut self, message: &Message) {
        let Some(task) = &message.subtask else { return };
        if message.role == "tool_result" {
            // A repeated launch must not overwrite an already received completion.
            self.tasks
                .entry(message.tool_id.clone())
                .or_insert_with(|| task.clone());
        } else if message.role == "activity" && message.tool_id.starts_with("task:") {
            self.child_versions.remove(&task.id);
            for existing in self
                .tasks
                .values_mut()
                .filter(|existing| existing.id == task.id)
            {
                *existing = task.clone();
            }
        }
    }

    pub fn apply(&self, message: &mut Message) {
        if is_claude_subtask(message)
            && let Some(task) = self.tasks.get(&message.tool_id)
        {
            message.subtask = Some(task.clone());
            message.tool_finished = true;
            message.tool_error = matches!(task.status.as_str(), "failed" | "killed");
        }
    }

    fn refresh_children(&mut self, chat: &Chat) -> Result<()> {
        for task in self.tasks.values_mut().filter(|task| task.running()) {
            let path = chat
                .path
                .with_extension("")
                .join("subagents")
                .join(format!("agent-{}.jsonl", task.id));
            let mut file = match File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            let version = (metadata.len(), metadata.modified()?);
            if self.child_versions.get(&task.id) == Some(&version) {
                continue;
            }
            let offset = metadata.len().saturating_sub(256 * 1024);
            file.seek(SeekFrom::Start(offset))?;
            let mut reader = BufReader::new(file.take(256 * 1024));
            if offset > 0 {
                reader.skip_until(b'\n')?;
            }
            let mut completed = false;
            let mut handback = None;
            let mut report_delivered = false;
            for line in reader.split(b'\n') {
                let line = line?;
                let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                // Ignore request snapshots and metadata. Only an actual child turn ending
                // or the parent's task notification can complete a background agent.
                if value["type"] == "assistant" {
                    completed = matches!(
                        value["message"]["stop_reason"].as_str(),
                        Some("end_turn" | "stop_sequence")
                    );
                    for block in value["message"]["content"].as_array().into_iter().flatten() {
                        if block["type"] == "tool_use" && block["name"] == "SubagentHandback" {
                            handback = block["id"].as_str().map(str::to_owned);
                        }
                    }
                } else if value["type"] == "user" || value["type"] == "api-request" {
                    completed = false;
                    for block in value["message"]["content"].as_array().into_iter().flatten() {
                        if block["type"] == "tool_result"
                            && block["tool_use_id"]
                                .as_str()
                                .zip(handback.as_deref())
                                .is_some_and(|(id, handback)| id == handback)
                            && block["is_error"] != true
                            && serde_json::from_str::<Value>(&content_text(&block["content"]))
                                .ok()
                                .is_some_and(|result| result["success"] == true)
                        {
                            report_delivered = true;
                        }
                    }
                }
            }
            if report_delivered || (completed && task.status != "waiting") {
                task.status = "completed".into();
            }
            self.child_versions.insert(task.id.clone(), version);
        }
        Ok(())
    }

    fn refresh(&mut self, chat: &Chat) -> Result<()> {
        if chat.provider != Provider::Claude {
            return Ok(());
        }
        let mut file = File::open(&chat.path)?;
        let length = file.metadata()?.len();
        if length < self.offset {
            *self = Self::default();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file.take(length - self.offset));
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = reader.read_until(b'\n', &mut line)?;
            if count == 0 || line.last() != Some(&b'\n') {
                break;
            }
            self.offset += count as u64;
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if !matches!(value["type"].as_str(), Some("user" | "system"))
                || value["isSidechain"] == true
                || !value["parent_tool_use_id"].is_null()
            {
                continue;
            }
            for message in records(Provider::Claude, &value) {
                self.observe(&message);
            }
        }
        self.refresh_children(chat)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageBodyPart {
    Text(String),
    File(String),
}

pub fn message_body_parts(text: &str) -> Vec<MessageBodyPart> {
    let mut parts = Vec::new();
    let mut offset = 0;
    let mut text_start = 0;
    while offset < text.len() {
        let remaining = &text[offset..];
        if remaining.starts_with('`') || remaining.starts_with("~~~") {
            let marker = remaining.chars().next().unwrap_or('`');
            let count = remaining
                .chars()
                .take_while(|character| *character == marker)
                .count();
            let delimiter = marker.to_string().repeat(count);
            offset += count;
            if let Some(end) = text[offset..].find(&delimiter) {
                offset += end + count;
            } else {
                break;
            }
            continue;
        }
        let prefix = ["::codex-file-citation{", ":codex-file-citation{"]
            .into_iter()
            .find(|prefix| remaining.starts_with(prefix));
        if let Some(prefix) = prefix
            && let Some((length, path)) = parse_file_citation(&remaining[prefix.len()..])
        {
            let prose = &text[text_start..offset];
            if !prose.trim().is_empty() {
                parts.push(MessageBodyPart::Text(prose.trim_matches('\n').to_owned()));
            }
            parts.push(MessageBodyPart::File(path));
            offset += prefix.len() + length;
            text_start = offset;
            continue;
        }
        offset += remaining.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    let prose = &text[text_start..];
    if !prose.trim().is_empty() {
        parts.push(MessageBodyPart::Text(prose.trim_matches('\n').to_owned()));
    }
    parts
}

fn parse_file_citation(input: &str) -> Option<(usize, String)> {
    let mut offset = 0;
    let mut path = None;
    loop {
        let remaining = input.get(offset..)?;
        offset += remaining.len() - remaining.trim_start().len();
        if input.get(offset..)?.starts_with('}') {
            let path: String = path?;
            let bytes = path.as_bytes();
            let absolute = path.starts_with('/')
                || path.starts_with("\\\\")
                || (bytes.first().is_some_and(u8::is_ascii_alphabetic)
                    && bytes.get(1) == Some(&b':')
                    && bytes
                        .get(2)
                        .is_some_and(|byte| matches!(byte, b'/' | b'\\')));
            return absolute.then_some((offset + 1, path));
        }
        let key_start = offset;
        while input
            .as_bytes()
            .get(offset)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            offset += 1;
        }
        if offset == key_start {
            return None;
        }
        let key = &input[key_start..offset];
        let remaining = input.get(offset..)?;
        offset += remaining.len() - remaining.trim_start().len();
        if input.as_bytes().get(offset) != Some(&b'=') {
            return None;
        }
        offset += 1;
        let remaining = input.get(offset..)?;
        offset += remaining.len() - remaining.trim_start().len();
        let quote = input.get(offset..)?.chars().next()?;
        let closing = match quote {
            '"' => '"',
            '\'' => '\'',
            '“' => '”',
            _ => return None,
        };
        offset += quote.len_utf8();
        let start = offset;
        loop {
            let character = input.get(offset..)?.chars().next()?;
            if character == closing {
                break;
            }
            if character == '\\' {
                let next = input.get(offset + 1..)?.chars().next()?;
                if next == closing || next == '\\' {
                    offset += 1 + next.len_utf8();
                    continue;
                }
            }
            offset += character.len_utf8();
        }
        let value = &input[start..offset];
        if key == "path" {
            path = Some(if quote == '"' {
                serde_json::from_str::<String>(&format!("\"{value}\""))
                    .ok()
                    .filter(|path| !path.chars().any(char::is_control))
                    .filter(|path| !value.starts_with("\\\\") || path.starts_with("\\\\"))
                    .unwrap_or_else(|| value.to_owned())
            } else {
                value.to_owned()
            });
        }
        offset += closing.len_utf8();
    }
}

#[cfg(test)]
mod file_citation_tests {
    use super::{
        Message, MessageBodyPart::*, inline_display_text, message_body_parts,
        normalize_protocol_message,
    };

    #[test]
    fn inline_transport_is_readable_without_document_scaffolding() {
        let payload = serde_json::json!({"request":"Fix bounds", "file":"src/a.rs", "document":"private unsaved source", "instructions":"transport only"});
        let message = normalize_protocol_message(Message {
            role: "user".into(),
            text: format!("[Zed inline request]\n{payload}"),
            ..Default::default()
        });
        assert_eq!(message.text, "Fix bounds\n\nsrc/a.rs");
        let response = normalize_protocol_message(Message {
            role: "assistant".into(),
            text: r#"{"explanation":"Fixed bounds","replacement_text":"safe_call();"}"#.into(),
            ..Default::default()
        });
        assert_eq!(response.text, "Fixed bounds\n\n```\nsafe_call();\n```");
        let ordinary = "{\"explanation\":\"ordinary data\"}";
        assert!(inline_display_text("assistant", ordinary).is_none());
    }

    #[test]
    fn file_cards_preserve_message_order_and_unicode_paths() {
        let parts = message_body_parts(
            "Turkish:\n:codex-file-citation{path=\"C:/My Files/guide TR.pdf\" purpose=\"output\"}\nEnglish:\n::codex-file-citation{purpose=\"output\" path=\"C:/My Files/guide EN.pdf\"}\nDone.",
        );
        assert_eq!(
            parts,
            vec![
                Text("Turkish:".into()),
                File("C:/My Files/guide TR.pdf".into()),
                Text("English:".into()),
                File("C:/My Files/guide EN.pdf".into()),
                Text("Done.".into())
            ]
        );
        assert_eq!(
            message_body_parts(":codex-file-citation{path=\"/tmp/çalışma {final}.pdf\"}"),
            vec![File("/tmp/çalışma {final}.pdf".into())]
        );
    }

    #[test]
    fn file_cards_accept_windows_paths_and_typographic_quotes() {
        for source in [
            r#":codex-file-citation{path="C:\temp\test.pdf"}"#,
            r#":codex-file-citation{path="C:\\temp\\test.pdf"}"#,
        ] {
            assert_eq!(
                message_body_parts(source),
                vec![File(r"C:\temp\test.pdf".into())]
            );
        }
        assert_eq!(
            message_body_parts(":codex-file-citation{path=“C:/test.pdf”}"),
            vec![File("C:/test.pdf".into())]
        );
        assert_eq!(
            message_body_parts(r#":codex-file-citation{path="\\server\share\test.pdf"}"#),
            vec![File(r"\\server\share\test.pdf".into())]
        );
    }

    #[test]
    fn file_cards_leave_examples_invalid_and_partial_directives_unchanged() {
        for source in [
            "`::codex-file-citation{path=\"C:/test.pdf\"}`",
            "```text\n::codex-file-citation{path=\"C:/test.pdf\"}\n```",
            "~~~text\n::codex-file-citation{path=\"C:/test.pdf\"}\n~~~",
            ":codex-file-citation{path=\"C:/test.pdf\"",
            ":codex-file-citation{path=\"https://example.com/test.pdf\"}",
            ":codex-file-citation{purpose=\"output\"}",
            ":codex-file-citation{path=\"relative/test.pdf\"}",
        ] {
            assert_eq!(message_body_parts(source), vec![Text(source.into())]);
        }
    }
}

pub fn is_question_tool(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or(name),
        "request_user_input_async" | "request_user_input" | "AskUserQuestion"
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStep {
    pub text: String,
    pub status: String,
}

fn is_plan_tool(name: &str) -> bool {
    let name = name.rsplit('.').next().unwrap_or(name);
    name == "TodoWrite"
        || name == "plan"
        || name == "update_plan"
        || name.ends_with("__update_plan")
}

pub fn plan_steps(message: &Message) -> Vec<PlanStep> {
    if !is_plan_tool(&message.tool_name) {
        return Vec::new();
    }
    let Ok(input) = serde_json::from_str::<Value>(&message.tool_input) else {
        return Vec::new();
    };
    input
        .get("items")
        .or_else(|| input.get("plan"))
        .or_else(|| input.get("todos"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|step| {
            let text = step
                .get("content")
                .or_else(|| step.get("step"))?
                .as_str()?
                .trim();
            if text.is_empty() {
                return None;
            }
            Some(PlanStep {
                text: text.into(),
                status: match step["status"].as_str().unwrap_or("pending") {
                    "inProgress" => "in_progress",
                    status => status,
                }
                .into(),
            })
        })
        .collect()
}

pub fn normalize_plan(message: &mut Message) {
    if !plan_steps(message).is_empty() {
        message.role = "plan".into();
    }
}

pub fn apply_plan_update(message: &mut Message, previous: &[PlanStep]) -> Vec<PlanStep> {
    let full = plan_steps(message);
    if !full.is_empty() {
        message.role = "plan".into();
        return full;
    }
    if !is_plan_tool(&message.tool_name) || previous.is_empty() {
        return Vec::new();
    }
    let Ok(input) = serde_json::from_str::<Value>(&message.tool_input) else {
        return Vec::new();
    };
    let mut steps = previous.to_vec();
    let mut changed = false;
    if input["advance"] == true {
        if let Some(current) = steps.iter().position(|step| step.status == "in_progress") {
            steps[current].status = "completed".into();
            if let Some(next) = steps
                .iter_mut()
                .skip(current + 1)
                .find(|step| step.status == "pending")
            {
                next.status = "in_progress".into();
            }
            changed = true;
        }
    }
    let done = input["done"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![input["done"].clone()]);
    for index in done
        .iter()
        .filter_map(Value::as_u64)
        .filter_map(|index| index.checked_sub(1))
        .filter_map(|index| usize::try_from(index).ok())
    {
        if let Some(step) = steps.get_mut(index) {
            step.status = "completed".into();
            changed = true;
        }
    }
    if let Some(index) = input["start"]
        .as_u64()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| usize::try_from(index).ok())
    {
        if let Some(step) = steps.get_mut(index) {
            step.status = "in_progress".into();
            changed = true;
        }
    }
    if !changed {
        return Vec::new();
    }
    message.role = "plan".into();
    message.tool_input = serde_json::json!({"items":steps.iter().map(|step| serde_json::json!({"content":step.text,"status":step.status})).collect::<Vec<_>>(),"update":input}).to_string();
    steps
}

pub fn work_summary(messages: &[&Message]) -> String {
    let mut commands = 0;
    let mut agents = 0;
    let mut other = 0;
    let mut failed = 0;
    for message in messages.iter().filter(|message| message.role == "activity") {
        failed += usize::from(message.tool_error);
        if tool_label(message) == "Run command" {
            commands += 1;
        } else if matches!(
            message.tool_name.rsplit('.').next(),
            Some("Agent" | "Task" | "spawn_agent")
        ) {
            agents += 1;
        } else {
            other += 1;
        }
    }
    let mut parts = Vec::new();
    for (count, singular, plural) in [
        (commands, "command", "commands"),
        (agents, "agent", "agents"),
        (other, "step", "steps"),
    ] {
        if count > 0 {
            parts.push(format!(
                "{count} {}",
                if count == 1 { singular } else { plural }
            ));
        }
    }
    if parts.is_empty() && messages.iter().any(|message| message.role == "tool_image") {
        parts.push("Images".into());
    }
    let mut summary = parts.join(" · ");
    if failed > 0 {
        summary.push_str(&format!(" · {failed} failed"));
    }
    summary
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationPart {
    Message(usize),
    Work(std::ops::Range<usize>),
}

pub fn conversation_parts(messages: &[&Message]) -> Vec<ConversationPart> {
    let mut parts = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role == "internal" {
            continue;
        }
        if message.role == "plan"
            && messages[index + 1..]
                .iter()
                .take_while(|message| message.role != "user")
                .any(|message| message.role == "plan")
        {
            continue;
        }
        let work = (message.role == "activity" && !is_claude_subtask(message))
            || message.role == "tool_image";
        if work {
            if let Some(ConversationPart::Work(range)) = parts.last_mut()
                && range.end == index
            {
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

pub fn plan_is_running(messages: &[&Message], index: usize, running: bool) -> bool {
    running
        && !messages.iter().skip(index + 1).any(|message| {
            message.role == "user"
                || (message.role == "assistant" && message.phase == "final_answer")
        })
}

pub fn is_claude_subtask(message: &Message) -> bool {
    message.role == "activity" && matches!(message.tool_name.as_str(), "Agent" | "Task")
}

pub fn is_subtask_chat(chat: &Chat) -> bool {
    chat.provider == Provider::Claude
        && chat.internal
        && chat
            .path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "subagents")
}

/// Resolve using Claude's own tool-use metadata, never a title or an arbitrary path in tool output.
pub fn claude_subtask_chat(parent: &Chat, tool_id: &str) -> Result<Chat> {
    anyhow::ensure!(
        parent.provider == Provider::Claude && !tool_id.is_empty(),
        "No Claude subtask selected"
    );
    let directory = parent.path.with_extension("").join("subagents");
    for entry in fs::read_dir(&directory)
        .with_context(|| "Subtask history has not been saved yet. Try again shortly.")?
    {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".meta.json") else {
            continue;
        };
        let Ok(data) = fs::read(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<Value>(&data) else {
            continue;
        };
        if meta["toolUseId"] != tool_id {
            continue;
        }
        let path = directory.join(format!("{stem}.jsonl"));
        let modified = fs::metadata(&path)
            .with_context(|| "Subtask history is not available yet")?
            .modified()?;
        return Ok(Chat {
            provider: Provider::Claude,
            id: format!("{}:{stem}", parent.id),
            title: meta["description"]
                .as_str()
                .unwrap_or("Claude subtask")
                .into(),
            cwd: parent.cwd.clone(),
            path,
            modified,
            archived: false,
            internal: true,
        });
    }
    anyhow::bail!("Claude has not saved a linked conversation for this subtask yet.")
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
        if !fenced
            && (line.trim().starts_with("<pasted_content id=")
                || line.trim().starts_with("</pasted_content"))
            && line.trim().ends_with('>')
        {
            continue;
        }
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
    pub pending_subtasks: usize,
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
            if value["isSidechain"] == true || !value["parent_tool_use_id"].is_null() {
                return;
            }
            if value["parent_tool_use_id"].is_null() && value["isSidechain"] != true {
                let effort = value["perTurnEffort"]
                    .as_str()
                    .or_else(|| value["effort"].as_str())
                    .or_else(|| value["params"]["output_config"]["effort"].as_str());
                if let Some(effort) = effort {
                    self.effort = Some(effort.into());
                }
                if value["type"] == "api-request" {
                    if let Some(model) = value["params"]["model"].as_str() {
                        if self.model.as_deref() != Some(model) {
                            self.context_window = None;
                        }
                        self.model = Some(model.into());
                    }
                }
            }
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
    pub quick_chat: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_source: Option<Chat>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChatPaneLayout {
    Group(usize),
    Split {
        vertical: bool,
        ratio: f32,
        first: Box<ChatPaneLayout>,
        second: Box<ChatPaneLayout>,
    },
}

impl ChatPaneLayout {
    pub fn from_weights(weights: &[f32], vertical: bool) -> Self {
        fn build(weights: &[f32], index: usize, vertical: bool) -> ChatPaneLayout {
            if weights.len() <= 1 {
                return ChatPaneLayout::Group(index);
            }
            ChatPaneLayout::Split {
                vertical,
                ratio: weights[0] / weights.iter().sum::<f32>(),
                first: Box::new(ChatPaneLayout::Group(index)),
                second: Box::new(build(&weights[1..], index + 1, vertical)),
            }
        }
        build(weights, 0, vertical)
    }

    pub fn valid_for(&self, count: usize) -> bool {
        fn collect(node: &ChatPaneLayout, groups: &mut Vec<usize>, depth: usize) -> bool {
            if depth > 64 {
                return false;
            }
            match node {
                ChatPaneLayout::Group(group) => groups.push(*group),
                ChatPaneLayout::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    if !ratio.is_finite() || *ratio <= 0. || *ratio >= 1. {
                        return false;
                    }
                    if !collect(first, groups, depth + 1) || !collect(second, groups, depth + 1) {
                        return false;
                    }
                }
            }
            true
        }
        let mut groups = Vec::new();
        if !collect(self, &mut groups, 0) {
            return false;
        }
        groups.sort_unstable();
        groups == (0..count).collect::<Vec<_>>()
    }

    pub fn split(&mut self, group: usize, added: usize, vertical: bool) -> bool {
        match self {
            Self::Group(index) if *index == group => {
                *self = Self::Split {
                    vertical,
                    ratio: 0.5,
                    first: Box::new(Self::Group(group)),
                    second: Box::new(Self::Group(added)),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.split(group, added, vertical) || second.split(group, added, vertical)
            }
            _ => false,
        }
    }

    pub fn remove(self, removed: usize) -> Option<Self> {
        match self {
            Self::Group(index) => {
                (index != removed).then(|| Self::Group(index - usize::from(index > removed)))
            }
            Self::Split {
                vertical,
                ratio,
                first,
                second,
            } => match (first.remove(removed), second.remove(removed)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    vertical,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (first, second) => first.or(second),
            },
        }
    }

    pub fn set_ratio(&mut self, path: &[bool], value: f32) {
        if let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        {
            if let Some((right, rest)) = path.split_first() {
                if *right { second } else { first }.set_ratio(rest, value);
            } else {
                *ratio = value;
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralWorkspace {
    pub layout: Option<ChatPaneLayout>,
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
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChatStatus {
    Pinned,
    InputRequired,
    InProgress,
    Unread,
    Ready,
}

impl ChatStatus {
    pub fn from_state(pinned: bool, needs_input: bool, running: bool, unread: bool) -> Self {
        if pinned {
            Self::Pinned
        } else if needs_input {
            Self::InputRequired
        } else if running {
            Self::InProgress
        } else if unread {
            Self::Unread
        } else {
            Self::Ready
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Pinned => "Pinned",
            Self::InputRequired => "Input required",
            Self::InProgress => "In progress",
            Self::Unread => "Unread",
            Self::Ready => "Ready",
        }
    }
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

    pub fn reference(&self, title: &str) -> String {
        format!(
            "Chat: {title}\nProvider: {}\nChat ID: {}\nSession ID: {}\nDirectory: {}",
            self.provider.name(),
            self.key(),
            self.id,
            self.cwd
        )
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
        "<external_codex_apps_open_page>",
        "<codex_internal_context ",
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

struct ProtocolEnvelope {
    fields: HashMap<String, String>,
    complete: bool,
}

fn protocol_envelope(text: &str) -> Option<ProtocolEnvelope> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack: Vec<String> = Vec::new();
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut roots = 0;
    let mut complete = false;
    loop {
        let text = match reader.read_event() {
            Ok(Event::Start(tag)) => {
                if stack.is_empty() {
                    roots += 1;
                    if roots > 1 {
                        return None;
                    }
                }
                if stack.len() >= 64 {
                    return None;
                }
                stack.push(String::from_utf8_lossy(tag.name().as_ref()).into_owned());
                None
            }
            Ok(Event::Empty(_)) => {
                if stack.is_empty() {
                    return None;
                }
                None
            }
            Ok(Event::End(_)) => {
                stack.pop()?;
                complete = stack.is_empty();
                None
            }
            Ok(Event::Text(value)) => Some(value.decode().ok()?.into_owned()),
            Ok(Event::CData(value)) => Some(value.decode().ok()?.into_owned()),
            Ok(Event::GeneralRef(value)) => {
                let reference = format!("&{};", value.decode().ok()?);
                Some(quick_xml::escape::unescape(&reference).ok()?.into_owned())
            }
            Ok(Event::Comment(_) | Event::Decl(_)) => None,
            Ok(Event::Eof) => break,
            // Never resolve entities or interpret declarations from a transcript.
            Ok(Event::DocType(_) | Event::PI(_)) | Err(_) => return None,
        };
        if let Some(text) = text {
            if stack.is_empty() && !text.trim().is_empty() {
                return None;
            }
            for field in stack.iter().skip(1) {
                fields.entry(field.clone()).or_default().push_str(&text);
            }
        }
    }
    for value in fields.values_mut() {
        *value = value.trim().to_owned();
    }
    Some(ProtocolEnvelope { fields, complete })
}

fn inline_display_text(role: &str, text: &str) -> Option<String> {
    if role == "user" {
        let request: Value =
            serde_json::from_str(text.trim().strip_prefix("[Zed inline request]\n")?).ok()?;
        let prompt = request["request"].as_str()?;
        let file = request["file"].as_str()?;
        return Some(format!("{prompt}\n\n{file}"));
    }
    if role != "assistant" {
        return None;
    }
    let text = text.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|text| text.strip_suffix("```"))
        .unwrap_or(text)
        .trim();
    let answer: Value = serde_json::from_str(text).ok()?;
    let fields = answer.as_object()?;
    if fields.len() != 2 || !fields.contains_key("replacement_text") {
        return None;
    }
    let explanation = answer["explanation"].as_str()?;
    match &answer["replacement_text"] {
        Value::Null => Some(explanation.into()),
        Value::String(replacement) => {
            // Generated code may itself contain Markdown fences.
            let longest = replacement
                .lines()
                .map(|line| line.chars().take_while(|c| *c == '`').count())
                .max()
                .unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            Some(format!("{explanation}\n\n{fence}\n{replacement}\n{fence}"))
        }
        _ => None,
    }
}

fn claude_peer_message(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    let text = text
        .strip_prefix("Another Claude session sent a message:")
        .unwrap_or(text)
        .trim();
    let mut reader = quick_xml::Reader::from_str(text);
    let quick_xml::events::Event::Start(tag) = reader.read_event().ok()? else {
        return None;
    };
    if tag.name().as_ref() != b"agent-message" {
        return None;
    }
    let sender = tag.attributes().find_map(|attribute| {
        let attribute = attribute.ok()?;
        (attribute.key.as_ref() == b"from")
            .then(|| {
                attribute
                    .decode_and_unescape_value(reader.decoder())
                    .ok()
                    .map(|value| value.into_owned())
            })
            .flatten()
    })?;
    if sender.trim().is_empty() {
        return None;
    }
    let body = text
        .get(reader.buffer_position() as usize..)?
        .trim_end()
        .strip_suffix("</agent-message>")?
        .trim();
    Some((sender, body.to_owned()))
}

pub fn normalize_protocol_message(mut message: Message) -> Message {
    if let Some(text) = inline_display_text(&message.role, &message.text) {
        message.text = text;
        return message;
    }

    if !matches!(message.role.as_str(), "user" | "assistant") {
        return message;
    }
    if message.role == "user"
        && let Some((sender, body)) = claude_peer_message(&message.text)
    {
        message.role = "agent_message".into();
        message.tool_name = sender;
        message.text = body;
        return message;
    }
    let text = message.text.trim();
    let Some(rest) = text.strip_prefix('<') else {
        return message;
    };
    let root = rest
        .split(|character: char| character.is_whitespace() || matches!(character, '>' | '/'))
        .next()
        .unwrap_or_default();
    // Context and UI acknowledgements were never conversational messages. Keep
    // that rule identical for history imports and streaming display updates.
    if message.role == "user" && is_context(text) && root != "codex_internal_context" {
        message.role = "internal".into();
        return message;
    }
    let known = matches!(
        root,
        "heartbeat"
            | "task-notification"
            | "codex_internal_context"
            | "system-reminder"
            | "environment_context"
            | "permissions"
            | "skills_instructions"
            | "recommended_plugins"
            | "send_user_message_question_reply"
            | "external_codex_apps_open_page"
    );
    // Only classify transport envelopes, not XML/HTML examples, source code or prose.
    let family = root.ends_with("_notification")
        || root.ends_with("-notification")
        || root.ends_with("_event")
        || root.ends_with("-event")
        || root.ends_with("_context")
        || root.ends_with("-context")
        || root.ends_with("_reminder")
        || root.ends_with("-reminder");
    let envelope = protocol_envelope(text);
    let machine_fields = envelope.as_ref().is_some_and(|envelope| {
        [
            "automation_id",
            "task-id",
            "task_id",
            "thread_id",
            "tool-use-id",
            "tool_use_id",
            "session_id",
        ]
        .iter()
        .any(|field| envelope.fields.contains_key(*field))
    });
    let control_fields = envelope.as_ref().is_some_and(|envelope| {
        [
            "decision",
            "status",
            "instructions",
            "current_time_iso",
            "event",
        ]
        .iter()
        .any(|field| envelope.fields.contains_key(*field))
    });
    if !known
        && (!machine_fields
            || !(family || control_fields)
            || !envelope.as_ref().is_some_and(|envelope| envelope.complete))
    {
        return message;
    }
    // A wrapper embedded in normal prose is still normal content.
    if text.contains(&format!("</{root}>")) && !text.ends_with(&format!("</{root}>")) {
        return message;
    }
    let field = |name: &str| {
        envelope
            .as_ref()
            .and_then(|envelope| envelope.fields.get(name))
            .cloned()
            .unwrap_or_default()
    };
    let original = message.text.clone();
    if root == "heartbeat" && message.role == "assistant" {
        let report = field("message");
        if field("decision").eq_ignore_ascii_case("NOTIFY") && !report.is_empty() {
            message.text = report;
            return message;
        }
    }
    if root == "task-notification" {
        if let Some(mut notification) = task_notification(text, &message.timestamp) {
            notification.tool_input = original;
            return notification;
        }
    }
    message.tool_name = match root {
        "heartbeat" if message.role == "assistant" => "Scheduled check completed",
        "heartbeat" => "Scheduled check",
        "codex_internal_context" if !field("objective").is_empty() => "Continue goal",
        "task-notification" => "Background task update",
        _ => "System update",
    }
    .into();
    message.text = ["message", "summary", "event", "objective", "status"]
        .iter()
        .map(|name| field(name))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    message.tool_input = original;
    message.role = "activity".into();
    message.tool_finished = true;
    message.tool_error = matches!(field("status").as_str(), "failed" | "error");
    message
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
    let mut status = field("status");
    if status == "completed"
        && (field("note").contains("background work of its own still running")
            || field("result").contains("waiting on its own background work"))
    {
        status = "waiting".into();
    }
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
        subtask: matches!(
            status.as_str(),
            "completed" | "failed" | "killed" | "cancelled" | "stopped" | "waiting"
        )
        .then(|| ClaudeSubtask {
            id,
            status: status.clone(),
        }),
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

pub fn claude_skill_message(value: &Value) -> Option<Message> {
    if value["type"] != "user" || value["isMeta"] != true {
        return None;
    }
    let text = content_text(&value["message"]["content"]);
    if !text
        .trim_start()
        .starts_with("Base directory for this skill:")
    {
        return None;
    }
    let name = text
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .unwrap_or("Skill loaded");
    Some(Message {
        role: "activity".into(),
        tool_name: "Loaded skill".into(),
        text: name.to_owned(),
        tool_input: text,
        tool_finished: true,
        timestamp: string(value, "timestamp"),
        ..Default::default()
    })
}

fn visible_message(provider: Provider, value: &Value) -> Option<Message> {
    if provider == Provider::Claude
        && let Some(message) = claude_skill_message(value)
    {
        return Some(message);
    }
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
    let normalized = normalize_protocol_message(Message {
        role: role.clone(),
        text: text.clone(),
        timestamp: string(value, "timestamp"),
        phase: string(message, "phase"),
        ..Default::default()
    });
    if normalized.role == "internal" {
        return None;
    }
    if normalized.role != role || normalized.text != text {
        return Some(normalized);
    }
    if provider == Provider::Claude
        && value["isMeta"] == true
        && (text.trim_start().starts_with("[Image: original ")
            || claude_image_source_metadata(&text))
    {
        return None;
    }
    if provider == Provider::Claude && role == "user" {
        if value["isMeta"] == true && text.trim_start().starts_with("Goal check-in:") {
            return Some(Message {
                role: "activity".into(),
                tool_name: "Goal check-in".into(),
                tool_input: "Background task status".into(),
                text,
                timestamp: string(value, "timestamp"),
                tool_finished: true,
                ..Default::default()
            });
        }
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

// Claude writes this companion record after the actual image attachment. It is
// transport metadata, not another user message or a second copy of the image.
fn claude_image_source_metadata(text: &str) -> bool {
    let mut found = false;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Some(path) = line
            .strip_prefix("[Image: source: ")
            .and_then(|value| value.strip_suffix(']'))
        else {
            return false;
        };
        let windows_absolute = path.as_bytes().get(1) == Some(&b':')
            && matches!(path.as_bytes().get(2), Some(b'\\' | b'/'));
        if !(path.starts_with('/') || Path::new(path).is_absolute() || windows_absolute) {
            return false;
        }
        found = true;
    }
    found
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

#[derive(Clone)]
struct RecencyCacheEntry {
    length: u64,
    modified: SystemTime,
    last_user: Option<SystemTime>,
}

fn human_message_time(provider: Provider, value: &Value) -> Option<SystemTime> {
    if value["isMeta"] == true || value["isSidechain"] == true {
        return None;
    }
    let role = match provider {
        Provider::Codex => &value["payload"]["role"],
        Provider::Claude => &value["message"]["role"],
    };
    if role != "user" {
        return None;
    }
    let message = visible_message(provider, value)?;
    if message.role != "user" || super::chat_bridge::linked_message(&message.text).is_some() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(&message.timestamp)
        .ok()
        .map(SystemTime::from)
}

fn last_human_message_time(path: &Path, provider: Provider) -> Result<Option<SystemTime>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, RecencyCacheEntry>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let length = metadata.len();
    let modified = metadata.modified()?;
    let complete = if length > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut tail = [0];
        file.read_exact(&mut tail)?;
        tail[0] == b'\n'
    } else {
        true
    };
    let previous = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Chat recency cache unavailable"))?
        .get(path)
        .cloned();
    if let Some(previous) = &previous {
        if previous.length == length && previous.modified == modified {
            return Ok(previous.last_user);
        }
    }
    // Scan only newly appended bytes after the first lookup. AI output must not
    // require rereading a long conversation every time the sidebar polls it.
    let previous = previous.filter(|entry| entry.length < length);
    let lower = previous
        .as_ref()
        .map_or(0, |entry| entry.length.saturating_sub(1));
    let mut end = length;
    let mut prefix = Vec::new();
    let mut latest = None;
    while end > lower && latest.is_none() {
        let start = end.saturating_sub(256 * 1024).max(lower);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact(&mut bytes)?;
        bytes.extend_from_slice(&prefix);
        let boundary = if start > 0 {
            bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|index| index + 1)
        } else {
            Some(0)
        };
        if let Some(boundary) = boundary {
            for line in bytes[boundary..].rsplit(|byte| *byte == b'\n') {
                if let Ok(value) = serde_json::from_slice::<Value>(line) {
                    if let Some(time) = human_message_time(provider, &value) {
                        latest = Some(time);
                        break;
                    }
                }
            }
            prefix = bytes[..boundary].to_vec();
        } else {
            prefix = bytes;
        }
        end = start;
    }
    let last_user = latest.or_else(|| previous.and_then(|entry| entry.last_user));
    if complete {
        cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Chat recency cache unavailable"))?
            .insert(
                path.to_owned(),
                RecencyCacheEntry {
                    length,
                    modified,
                    last_user,
                },
            );
    }
    Ok(last_user)
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
        path: path.clone(),
        modified: last_human_message_time(&path, provider)?
            .unwrap_or_else(|| file_metadata.created().unwrap_or(SystemTime::UNIX_EPOCH)),
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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ChatSchedule {
    pub id: String,
    pub name: String,
    pub status: String,
    pub rrule: String,
    pub target_thread_id: Option<String>,
    #[serde(default)]
    pub prompt: String,
    #[serde(skip)]
    pub path: PathBuf,
}

impl ChatSchedule {
    pub fn active(&self) -> bool {
        self.status.eq_ignore_ascii_case("ACTIVE")
    }

    pub fn cadence(&self) -> String {
        let rule = self.rrule.strip_prefix("RRULE:").unwrap_or(&self.rrule);
        let fields = rule
            .split(';')
            .filter_map(|field| field.split_once('='))
            .collect::<HashMap<_, _>>();
        if fields
            .keys()
            .any(|key| !matches!(*key, "FREQ" | "INTERVAL" | "BYHOUR" | "BYMINUTE"))
        {
            return self.rrule.clone();
        }
        let frequency = fields.get("FREQ").copied();
        let unit = match frequency {
            Some("MINUTELY") => "minute",
            Some("HOURLY") => "hour",
            Some("DAILY") => "day",
            Some("WEEKLY") => "week",
            _ => return self.rrule.clone(),
        };
        let interval = fields.get("INTERVAL").copied().unwrap_or("1");
        let mut cadence = match interval.parse::<u32>() {
            Ok(1) => format!("Every {unit}"),
            Ok(count) if count > 1 => format!("Every {count} {unit}s"),
            _ => return self.rrule.clone(),
        };
        if fields.contains_key("BYHOUR") || fields.contains_key("BYMINUTE") {
            if frequency != Some("DAILY") {
                return self.rrule.clone();
            }
            let hour = fields
                .get("BYHOUR")
                .and_then(|hour| hour.parse::<u32>().ok());
            let minute = fields
                .get("BYMINUTE")
                .copied()
                .unwrap_or("0")
                .parse::<u32>()
                .ok();
            match (hour, minute) {
                (Some(hour), Some(minute)) if hour < 24 && minute < 60 => {
                    cadence.push_str(&format!(" at {hour:02}:{minute:02}"));
                }
                _ => return self.rrule.clone(),
            }
        }
        cadence
    }
}

fn schedules_in(directory: &Path) -> Result<HashMap<String, Vec<ChatSchedule>>> {
    let mut schedules: HashMap<String, Vec<ChatSchedule>> = HashMap::new();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(schedules),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("automation.toml");
        let read = || -> Result<ChatSchedule> {
            let mut schedule: ChatSchedule = toml::from_str(&fs::read_to_string(&path)?)?;
            schedule.path = path.clone();
            Ok(schedule)
        };
        match read() {
            Ok(schedule) => {
                if let Some(thread) = schedule
                    .target_thread_id
                    .as_ref()
                    .filter(|id| !id.is_empty())
                {
                    schedules
                        .entry(format!("Codex:{thread}"))
                        .or_default()
                        .push(schedule);
                }
            }
            Err(error) => log::warn!("Cannot read schedule {}: {error}", path.display()),
        }
    }
    for entries in schedules.values_mut() {
        entries.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    }
    Ok(schedules)
}

pub fn local_schedules() -> Result<HashMap<String, Vec<ChatSchedule>>> {
    let codex = match std::env::var_os("CODEX_HOME") {
        Some(directory) => PathBuf::from(directory),
        None => PathBuf::from(
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .context("Home directory is unavailable")?,
        )
        .join(".codex"),
    };
    schedules_in(&codex.join("automations"))
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
    if provider == Provider::Claude
        && (value["isSidechain"] == true || !value["parent_tool_use_id"].is_null())
    {
        return Vec::new();
    }
    if provider == Provider::Claude
        && let Some(message) = claude_task_event(value)
    {
        return vec![message];
    }
    let payload = if provider == Provider::Codex {
        &value["payload"]
    } else {
        &value["message"]
    };
    if payload["role"] == "user" {
        let text = content_text(&payload["content"]);
        if let Some(reply) = text
            .trim()
            .strip_prefix("<send_user_message_question_reply>")
        {
            let reply = reply
                .split("</send_user_message_question_reply>")
                .next()
                .unwrap_or_default();
            if let Ok(replies) = serde_json::from_str::<Vec<Value>>(reply) {
                return replies
                    .into_iter()
                    .filter_map(|reply| {
                        let identity =
                            serde_json::from_str::<Vec<Value>>(reply["questionItemId"].as_str()?)
                                .ok()?;
                        Some(Message {
                            role: "question_reply".into(),
                            tool_id: identity.get(1)?.as_str()?.into(),
                            tool_input: identity.get(2)?.as_u64()?.to_string(),
                            text: reply["answer"].as_str().unwrap_or_default().into(),
                            ..Default::default()
                        })
                    })
                    .collect();
            }
        }
    }
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
                role: if is_question_tool(&string(block, "name")) {
                    "question"
                } else {
                    "activity"
                }
                .into(),
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
                subtask: (provider == Provider::Claude)
                    .then(|| claude_async_subtask(value, block))
                    .flatten(),
                attachments: if inline_image {
                    Vec::new()
                } else {
                    attachments.clone()
                },
                ..Default::default()
            });
            if inline_image {
                records.push(Message {
                    role: "tool_image".into(),
                    timestamp: timestamp.clone(),
                    attachments,
                    ..Default::default()
                });
            }
        }
    }
    for message in &mut records {
        normalize_plan(message);
    }
    records
}

#[derive(Clone, Debug)]
pub struct ChatActivity {
    pub modified: SystemTime,
    pub last_user_message_at: Option<SystemTime>,
    pub running: bool,
    pub reply: Option<String>,
    pub pending_questions: HashSet<String>,
    pub subtasks: ClaudeSubtasks,
    turn_running: bool,
}

impl ChatActivity {
    pub fn completed_since(&self, previous: Option<&Self>) -> bool {
        !self.running
            && self.reply.is_some()
            && previous.is_some_and(|old| old.running && old.reply != self.reply)
    }
}

fn codex_turn_running(value: &Value) -> Option<bool> {
    if value["type"] == "event_msg" {
        match value["payload"]["type"].as_str() {
            Some("task_started" | "turn_started") => return Some(true),
            Some("task_complete" | "task_completed" | "turn_aborted") => return Some(false),
            _ => {}
        }
    }
    if value["type"] == "response_item"
        && value["payload"]["type"] == "message"
        && value["payload"]["role"] == "assistant"
        && value["payload"]["phase"] == "final_answer"
    {
        return Some(false);
    }
    None
}

pub fn chat_activity(chat: &Chat, previous: Option<&ChatActivity>) -> Result<ChatActivity> {
    let mut file = File::open(&chat.path)?;
    let metadata = file.metadata()?;
    let modified = metadata.modified()?;
    if let Some(previous) = previous.filter(|previous| previous.modified == modified) {
        let mut previous = previous.clone();
        previous.subtasks.refresh(chat)?;
        if modified
            .elapsed()
            .is_ok_and(|age| age > std::time::Duration::from_secs(90))
        {
            previous.turn_running = false;
        }
        previous.running = previous.turn_running || previous.subtasks.pending() > 0;
        return Ok(previous);
    }
    let mut activity = ChatActivity {
        modified,
        last_user_message_at: last_human_message_time(&chat.path, chat.provider)?,
        running: previous.is_some_and(|previous| previous.turn_running),
        reply: previous.and_then(|previous| previous.reply.clone()),
        pending_questions: previous
            .map(|previous| previous.pending_questions.clone())
            .unwrap_or_default(),
        subtasks: previous
            .map(|previous| previous.subtasks.clone())
            .unwrap_or_default(),
        turn_running: false,
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
        for message in records(chat.provider, &value) {
            match message.role.as_str() {
                "question" if !message.tool_id.is_empty() => {
                    activity.pending_questions.insert(message.tool_id);
                }
                "question_reply" => {
                    activity.pending_questions.remove(&message.tool_id);
                }
                "tool_result" if message.text.trim() != "{\"accepted\":true}" => {
                    let accepted = serde_json::from_str::<Value>(&message.text)
                        .ok()
                        .is_some_and(|value| value["accepted"] == true);
                    if !accepted {
                        activity.pending_questions.remove(&message.tool_id);
                    }
                }
                "user" => activity.pending_questions.clear(),
                _ => {}
            }
        }
        let mut reply = false;
        match chat.provider {
            Provider::Codex => {
                if let Some(running) = codex_turn_running(&value) {
                    activity.running = running;
                    reply = !running && value["payload"]["type"] != "turn_aborted";
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
    activity.turn_running = activity.running;
    activity.subtasks.refresh(chat)?;
    activity.running |= activity.subtasks.pending() > 0;
    Ok(activity)
}

pub fn transcript(chat: &Chat, limit: usize) -> Result<Transcript> {
    let file =
        File::open(&chat.path).with_context(|| format!("Cannot open {}", chat.path.display()))?;
    let length = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(length));
    let mut line = Vec::new();
    let mut messages: VecDeque<Message> = VecDeque::new();
    let mut plan = Vec::new();
    let mut transcript = Transcript::default();
    let mut subtasks = ClaudeSubtasks::default();
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
        let mut value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                transcript.skipped += 1;
                continue;
            }
        };
        // Sidechain messages are excluded from the parent, but are the content of an
        // explicitly opened subtask. Do not merge them into ordinary conversations.
        if is_subtask_chat(chat) {
            value["isSidechain"] = Value::Bool(false);
            value["parent_tool_use_id"] = Value::Null;
        }
        transcript.metrics.observe(chat.provider, &value);
        if chat.provider == Provider::Codex
            && let Some(running) = codex_turn_running(&value)
        {
            transcript.running = running;
        }
        if chat.provider == Provider::Claude && value["isSidechain"] != true {
            if matches!(value["type"].as_str(), Some("user" | "api-request")) {
                transcript.running = true;
            }
            if matches!(
                value["message"]["stop_reason"].as_str(),
                Some("end_turn" | "stop_sequence")
            ) || (value["type"] == "system" && value["subtype"] == "turn_duration")
            {
                transcript.running = false;
            }
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
            for mut message in parsed {
                if chat.provider == Provider::Claude {
                    subtasks.observe(&message);
                }
                let updated_plan = apply_plan_update(&mut message, &plan);
                if !updated_plan.is_empty() {
                    plan = updated_plan;
                }
                if message.role == "question_reply" {
                    if let Some(call) = messages
                        .iter_mut()
                        .rev()
                        .find(|call| call.role == "question" && call.tool_id == message.tool_id)
                    {
                        call.question_answers
                            .insert(message.tool_input, message.text);
                    }
                    continue;
                }
                if message.role == "tool_result" {
                    if let Some(call) = messages.iter_mut().rev().find(|call| {
                        matches!(call.role.as_str(), "activity" | "question" | "plan")
                            && call.tool_id == message.tool_id
                    }) {
                        call.text = message.text;
                        call.tool_finished = true;
                        call.tool_error = message.tool_error;
                        call.subtask = message.subtask;
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
    subtasks.refresh_children(chat)?;
    for message in &mut messages {
        subtasks.apply(message);
    }
    transcript.pending_subtasks = subtasks.pending();
    transcript.running |= transcript.pending_subtasks > 0;
    transcript.messages = messages.into();
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_image_source_companions_are_not_duplicate_user_bubbles() {
        let marker = r"[Image: source: C:\Users\example\Temp\claude\images\10.png]";
        let mut value = json!({
            "type": "user", "isMeta": true,
            "message": {"role": "user", "content": [{"type": "text", "text": marker}]}
        });
        assert!(visible_message(Provider::Claude, &value).is_none());
        value["isMeta"] = json!(false);
        assert_eq!(
            visible_message(Provider::Claude, &value)
                .expect("real user text")
                .text,
            marker
        );
        value["message"]["content"] = json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aW1hZ2U="}},
            {"type": "text", "text": "Please inspect this screenshot"}
        ]);
        let message = visible_message(Provider::Claude, &value).expect("image message");
        assert_eq!(message.text, "Please inspect this screenshot");
        assert_eq!(message.attachments, ["data:image/png;base64,aW1hZ2U="]);
        assert!(!claude_image_source_metadata(
            "Explain [Image: source: C:/image.png]"
        ));
        assert!(!claude_image_source_metadata(
            "[Image: source: relative.png]"
        ));
        assert!(!claude_image_source_metadata(""));
        assert!(claude_image_source_metadata(
            "[Image: source: /tmp/image.png]\n[Image: source: C:/other.png]"
        ));
    }

    #[test]
    fn nested_chat_layout_preserves_independent_splits_and_survives_closing() {
        let mut layout = ChatPaneLayout::Group(0);
        assert!(layout.split(0, 1, true));
        assert!(layout.split(0, 2, false));
        layout.set_ratio(&[false], 0.65);
        assert!(layout.valid_for(3));
        let restored: ChatPaneLayout =
            serde_json::from_str(&serde_json::to_string(&layout).expect("serialize layout"))
                .expect("restore layout");
        assert_eq!(restored, layout);
        if let ChatPaneLayout::Split {
            vertical,
            ratio,
            first,
            second,
        } = &restored
        {
            assert!(*vertical);
            assert_eq!(*ratio, 0.5);
            assert_eq!(**second, ChatPaneLayout::Group(1));
            assert!(matches!(
                **first,
                ChatPaneLayout::Split {
                    vertical: false,
                    ratio: 0.65,
                    ..
                }
            ));
        } else {
            panic!("expected split");
        }
        let remaining = restored.remove(0).expect("remaining panes");
        assert!(remaining.valid_for(2));
        assert_eq!(remaining.remove(0), Some(ChatPaneLayout::Group(0)));
        assert_eq!(ChatPaneLayout::Group(0).remove(0), None);
    }

    #[test]
    fn chat_layout_validates_saved_groups_and_legacy_weights() {
        let layout = ChatPaneLayout::from_weights(&[2., 1., 1.], false);
        assert!(layout.valid_for(3));
        assert!(!layout.valid_for(2));
        let mut invalid = layout.clone();
        invalid.set_ratio(&[], f32::NAN);
        assert!(!invalid.valid_for(3));
        let legacy: GeneralWorkspace =
            serde_json::from_str(r#"{"vertical":true}"#).expect("legacy");
        assert!(legacy.layout.is_none());
    }

    #[test]
    fn peer_messages_preserve_body_without_displaying_protocol_wrapper() {
        let body = "Review **Workers**.\n```rust\nlet value: Vec<T> = Vec::new();\n```";
        for prefix in ["", "Another Claude session sent a message:\n"] {
            let message = normalize_protocol_message(Message {
                role: "user".into(),
                text: format!(
                    "{prefix}<agent-message from=\"agent&amp;one\">\n{body}\n</agent-message>"
                ),
                ..Default::default()
            });
            assert_eq!(message.role, "agent_message");
            assert_eq!(message.tool_name, "agent&one");
            assert_eq!(message.text, body);
            assert_eq!(normalize_protocol_message(message.clone()), message);
        }
        for text in [
            "Explain <agent-message from=\"a\">text</agent-message>",
            "<agent-message from=\"a\">unfinished",
        ] {
            let message = normalize_protocol_message(Message {
                role: "user".into(),
                text: text.into(),
                ..Default::default()
            });
            assert_eq!(message.text, text);
        }
    }

    #[test]
    fn copied_chat_reference_uses_provider_qualified_routing_key() {
        for provider in [Provider::Codex, Provider::Claude] {
            let chat = Chat {
                provider,
                id: "same-id".into(),
                title: "Old title".into(),
                cwd: "C:/Projects/sevk".into(),
                path: PathBuf::new(),
                modified: SystemTime::UNIX_EPOCH,
                archived: false,
                internal: false,
            };
            let reference = chat.reference("Renamed chat");
            assert!(reference.contains("Chat: Renamed chat\n"));
            assert!(reference.contains(&format!("Chat ID: {}\n", chat.key())));
            assert!(reference.contains("Directory: C:/Projects/sevk"));
        }
    }

    fn async_agent_rows() -> Vec<Value> {
        vec![
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Agent","input":{"description":"Review Rust"}}],"stop_reason":"tool_use"}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"Async agent launched successfully.\nagentId: child-1 (internal ID)"}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"child-1"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Waiting for the agent."}],"stop_reason":"end_turn"}}),
            json!({"type":"system","subtype":"turn_duration"}),
        ]
    }

    #[test]
    fn async_agent_outlives_parent_turn_and_visible_history_page() {
        let fixture = Fixture::new();
        let path = fixture.write("async.jsonl", &async_agent_rows());
        let chat = metadata(path.clone(), Provider::Claude, false, &HashMap::new()).expect("chat");
        let history = transcript(&chat, 200).expect("history");
        let call = history
            .messages
            .iter()
            .find(|message| is_claude_subtask(message))
            .expect("agent");
        assert!(call.tool_finished, "launch call has returned");
        assert!(call.subtask.as_ref().expect("async task").running());
        assert!(history.running);
        assert_eq!(
            transcript(&chat, 1).expect("small page").pending_subtasks,
            1
        );
        let old = SystemTime::now() - std::time::Duration::from_secs(600);
        File::options()
            .write(true)
            .open(path)
            .expect("file")
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("mtime");
        let activity = chat_activity(&chat, None).expect("activity");
        assert!(
            activity.running,
            "parent inactivity does not complete a background task"
        );
        assert!(
            chat_activity(&chat, Some(&activity))
                .expect("cached activity")
                .running
        );
    }

    #[test]
    fn async_agent_completion_is_detected_without_parent_file_changes() {
        let fixture = Fixture::new();
        let path = fixture.write("parent.jsonl", &async_agent_rows());
        let chat = metadata(path, Provider::Claude, false, &HashMap::new()).expect("chat");
        fixture.write(
            "parent/subagents/agent-child-1.jsonl",
            &[json!({"type":"assistant","isSidechain":true,"message":{"stop_reason":"tool_use"}})],
        );
        let before = chat_activity(&chat, None).expect("running");
        fixture.write("parent/subagents/agent-child-1.jsonl", &[json!({"type":"assistant","isSidechain":true,"message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}})]);
        let after = chat_activity(&chat, Some(&before)).expect("finished");
        assert_eq!(before.modified, after.modified);
        assert!(before.running);
        assert!(!after.running);
        let history = transcript(&chat, 200).expect("history");
        assert!(!history.running);
        assert_eq!(history.pending_subtasks, 0);
        let call = history
            .messages
            .iter()
            .find(|message| is_claude_subtask(message))
            .expect("agent");
        assert_eq!(call.subtask.as_ref().expect("task").status, "completed");
    }

    #[test]
    fn async_notifications_finish_only_the_matching_agent_and_survive_replayed_launches() {
        for status in ["completed", "failed", "killed", "cancelled", "stopped"] {
            let mut tracker = ClaudeSubtasks::default();
            let launch = Message {
                role: "tool_result".into(),
                tool_id: "tool-1".into(),
                subtask: Some(ClaudeSubtask {
                    id: "child-1".into(),
                    status: "running".into(),
                }),
                ..Default::default()
            };
            tracker.observe(&launch);
            tracker.observe(&Message {
                tool_id: "tool-2".into(),
                subtask: Some(ClaudeSubtask {
                    id: "child-2".into(),
                    status: "running".into(),
                }),
                ..launch.clone()
            });
            let notification = task_notification(&format!("<task-notification><task-id>child-1</task-id><status>{status}</status><summary>Agent report</summary></task-notification>"), "").expect("notification");
            tracker.observe(&notification);
            tracker.observe(&launch);
            assert_eq!(tracker.pending(), 1);
            assert_eq!(tracker.tasks["tool-1"].status, status);
            assert!(tracker.tasks["tool-2"].running());
        }
    }

    #[test]
    fn synchronous_agent_result_and_unrelated_monitor_are_not_pending_agents() {
        let fixture = Fixture::new();
        let mut rows = async_agent_rows();
        rows[1] = json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"Finished review"}]}});
        let path = fixture.write("sync.jsonl", &rows);
        let chat = metadata(path, Provider::Claude, false, &HashMap::new()).expect("chat");
        let history = transcript(&chat, 200).expect("history");
        assert!(!history.running);
        assert_eq!(history.pending_subtasks, 0);
        assert!(history.messages[0].tool_finished);
        assert!(history.messages[0].subtask.is_none());
        let event = task_notification("<task-notification><task-id>child-1</task-id><event>Build still running</event></task-notification>", "").expect("event");
        assert!(event.subtask.is_none());
    }

    #[test]
    fn async_launch_fallback_does_not_require_run_in_background_input() {
        let result = json!({"content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a123 (internal ID)\nThe agent is working in the background."}]});
        assert_eq!(
            claude_async_subtask(&Value::Null, &result)
                .expect("fallback")
                .id,
            "a123"
        );
        assert!(
            claude_async_subtask(
                &json!({"toolUseResult":{"isAsync":true,"agentId":"../bad"}}),
                &result
            )
            .is_none()
        );
    }

    #[test]
    fn async_tracker_reads_launches_outside_tail_and_retries_partial_notifications() {
        use std::io::Write;
        let fixture = Fixture::new();
        let mut rows = async_agent_rows();
        rows.insert(
            2,
            json!({"type":"api-request-blob","payload":"x".repeat(300 * 1024)}),
        );
        let path = fixture.write("parent.jsonl", &rows);
        let chat = metadata(path.clone(), Provider::Claude, false, &HashMap::new()).expect("chat");
        let before = chat_activity(&chat, None).expect("cold start");
        assert_eq!(before.subtasks.pending(), 1);
        let notification = json!({"type":"system","subtype":"task_notification","task_id":"child-1","status":"completed","summary":"Done"}).to_string();
        let mut file = File::options().append(true).open(path).expect("file");
        file.write_all(notification.as_bytes())
            .expect("partial record");
        let partial = chat_activity(&chat, Some(&before)).expect("partial");
        assert_eq!(partial.subtasks.pending(), 1);
        file.write_all(b"\n").expect("finish record");
        let complete = chat_activity(&chat, Some(&partial)).expect("complete");
        assert_eq!(complete.subtasks.pending(), 0);
        assert!(!complete.running);
        assert_eq!(transcript(&chat, 200).expect("history").pending_subtasks, 0);
    }

    #[test]
    fn interim_completed_notification_waits_for_successful_subagent_handback() {
        let fixture = Fixture::new();
        let mut rows = async_agent_rows();
        rows.push(json!({"type":"user","message":{"role":"user","content":"<task-notification><task-id>child-1</task-id><status>completed</status><summary>Agent is waiting on its own background work</summary><note>This agent stopped with background work of its own still running.</note><result>This agent has not reported yet: it is waiting on its own background work and will deliver its report through SubagentHandback.</result></task-notification>"}}));
        rows.push(json!({"type":"system","subtype":"turn_duration"}));
        let path = fixture.write("parent.jsonl", &rows);
        let chat = metadata(path, Provider::Claude, false, &HashMap::new()).expect("chat");
        fixture.write("parent/subagents/agent-child-1.jsonl", &[json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Waiting for compilation"}]}})]);
        let before = chat_activity(&chat, None).expect("waiting");
        assert!(before.running);
        assert_eq!(before.subtasks.tasks["tool-1"].status, "waiting");
        assert_eq!(
            transcript(&chat, 200)
                .expect("waiting transcript")
                .pending_subtasks,
            1
        );
        let handback = json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"handback","name":"SubagentHandback","input":{}}]}});
        fixture.write("parent/subagents/agent-child-1.jsonl", &[handback.clone(), json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"handback","is_error":true,"content":"Failed"}]}})]);
        let failed = chat_activity(&chat, Some(&before)).expect("failed handback");
        assert!(failed.running, "failed delivery does not complete the task");
        fixture.write("parent/subagents/agent-child-1.jsonl", &[handback, json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"handback","content":[{"type":"text","text":"{\"success\":true,\"message\":\"Report delivered to your caller.\"}"}]}]}})]);
        let after = chat_activity(&chat, Some(&failed)).expect("delivered report");
        assert!(!after.running);
        assert_eq!(after.subtasks.pending(), 0);
        assert_eq!(
            transcript(&chat, 200)
                .expect("complete transcript")
                .pending_subtasks,
            0
        );
    }

    #[test]
    fn plan_animation_stops_at_turn_boundary_without_completing_tasks() {
        let plan = Message {
            role: "plan".into(),
            ..Default::default()
        };
        let commentary = Message {
            role: "assistant".into(),
            phase: "commentary".into(),
            ..Default::default()
        };
        let answer = Message {
            role: "assistant".into(),
            phase: "final_answer".into(),
            ..Default::default()
        };
        let user = Message {
            role: "user".into(),
            ..Default::default()
        };
        assert!(plan_is_running(&[&plan, &commentary], 0, true));
        assert!(!plan_is_running(&[&plan, &commentary], 0, false));
        assert!(!plan_is_running(&[&plan, &answer], 0, true));
        assert!(!plan_is_running(&[&plan, &user], 0, true));
    }

    #[test]
    fn injected_claude_skill_is_activity_but_user_quoted_skill_is_preserved() {
        let text = "Base directory for this skill: C:/skills/browser\n\n# Built-in browser\n\nInstructions remain available in details.";
        let mut record = json!({"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":text}]}});
        let message = visible_message(Provider::Claude, &record).expect("activity");
        assert_eq!(message.role, "activity");
        assert_eq!(message.tool_name, "Loaded skill");
        assert_eq!(message.text, "Built-in browser");
        assert_eq!(message.tool_input, text);
        assert!(message.tool_finished);
        record["isMeta"] = json!(false);
        let message = visible_message(Provider::Claude, &record).expect("user quote");
        assert_eq!(message.role, "user");
        assert_eq!(message.text, text);
    }

    #[test]
    fn schedules_match_explicit_thread_and_refresh_status_and_removal() {
        let fixture = Fixture::new();
        let directory = fixture.0.join("automations");
        fs::create_dir_all(directory.join("first")).expect("directory");
        let path = directory.join("first/automation.toml");
        let config = r#"id = "first"
name = "R2 batch takibi"
status = "ACTIVE"
rrule = "FREQ=MINUTELY;INTERVAL=30"
target_thread_id = "target"
prompt = "Check progress"
"#;
        fs::write(&path, config).expect("config");
        fs::create_dir_all(directory.join("unbound")).expect("directory");
        fs::write(
            directory.join("unbound/automation.toml"),
            config.replace("target_thread_id = \"target\"\n", ""),
        )
        .expect("config");
        let first = schedules_in(&directory).expect("schedules");
        assert_eq!(first.len(), 1);
        assert_eq!(first["Codex:target"].len(), 1);
        let schedule = &first["Codex:target"][0];
        assert!(schedule.active());
        assert_eq!(schedule.cadence(), "Every 30 minutes");
        assert!(!first.contains_key("Claude:target"));
        fs::write(
            &path,
            config
                .replace("ACTIVE", "PAUSED")
                .replace("FREQ=MINUTELY;INTERVAL=30", "FREQ=DAILY;BYHOUR=9"),
        )
        .expect("pause");
        let paused = schedules_in(&directory).expect("schedules");
        assert!(!paused["Codex:target"][0].active());
        assert_eq!(paused["Codex:target"][0].cadence(), "Every day at 09:00");
        fs::remove_file(path).expect("remove");
        assert!(schedules_in(&directory).expect("schedules").is_empty());
    }

    #[test]
    fn recency_does_not_skip_a_partially_written_user_record() {
        let fixture = Fixture::new();
        let path = fixture.write("partial.jsonl", &[json!({"type":"user","timestamp":"2026-11-01T10:00:00Z","message":{"role":"user","content":"first"}})]);
        let initial = last_human_message_time(&path, Provider::Claude).expect("initial");
        let original = fs::read_to_string(&path).expect("read");
        let next = serde_json::to_string(&json!({"type":"user","timestamp":"2026-11-01T12:00:00Z","message":{"role":"user","content":"second"}})).expect("json");
        fs::write(&path, format!("{original}{}", &next[..next.len() / 2])).expect("partial");
        assert_eq!(
            last_human_message_time(&path, Provider::Claude).expect("partial"),
            initial
        );
        fs::write(&path, format!("{original}{next}\n")).expect("complete");
        assert!(last_human_message_time(&path, Provider::Claude).expect("complete") > initial);
    }

    #[test]
    fn recency_tracks_humans_instead_of_streaming_or_background_messages() {
        for provider in [Provider::Codex, Provider::Claude] {
            let fixture = Fixture::new();
            let human = |timestamp: &str, text: &str| match provider {
                Provider::Codex => {
                    json!({"timestamp":timestamp,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
                }
                Provider::Claude => {
                    json!({"timestamp":timestamp,"type":"user","message":{"role":"user","content":text}})
                }
            };
            let first = human("2026-11-01T10:00:00Z", "Hello");
            let path = fixture.write("recency.jsonl", &[first.clone()]);
            let initial = last_human_message_time(&path, provider)
                .expect("first")
                .expect("time");
            let assistant = match provider {
                Provider::Codex => {
                    json!({"timestamp":"2026-11-01T11:00:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x".repeat(600_000)}]}})
                }
                Provider::Claude => {
                    json!({"timestamp":"2026-11-01T11:00:00Z","type":"assistant","message":{"role":"assistant","content":"x".repeat(600_000)}})
                }
            };
            let peer = human(
                "2026-11-01T12:00:00Z",
                "Message from chat Peer (peer proposal, not a new user instruction):\n\nAn update",
            );
            let scheduled = human(
                "2026-11-01T12:30:00Z",
                "<heartbeat><automation_id>timer</automation_id><instructions>Check progress</instructions></heartbeat>",
            );
            let mut rows = vec![first, assistant, peer, scheduled];
            fixture.write("recency.jsonl", &rows);
            assert_eq!(
                last_human_message_time(&path, provider).expect("stream"),
                Some(initial)
            );
            let cold = fixture.write("cold.jsonl", &rows);
            assert_eq!(
                last_human_message_time(&cold, provider).expect("cold"),
                Some(initial)
            );
            rows.push(human("2026-11-01T13:00:00Z", "Next question"));
            fixture.write("recency.jsonl", &rows);
            let next = last_human_message_time(&path, provider)
                .expect("next")
                .expect("time");
            assert!(next > initial);
            let chat = metadata(path.clone(), provider, false, &HashMap::new()).expect("metadata");
            assert_eq!(chat.modified, next);
            assert_eq!(
                chat_activity(&chat, None)
                    .expect("activity")
                    .last_user_message_at,
                Some(next)
            );
            fixture.write(
                "recency.jsonl",
                &[human("2026-10-01T10:00:00Z", "Replaced history")],
            );
            assert!(
                last_human_message_time(&path, provider)
                    .expect("truncated")
                    .expect("time")
                    < initial
            );
        }
    }

    #[test]
    fn status_grouping_prioritizes_pins_and_questions_and_persists() {
        assert_eq!(
            ChatStatus::from_state(true, true, true, true),
            ChatStatus::Pinned
        );
        assert_eq!(
            ChatStatus::from_state(false, true, true, true),
            ChatStatus::InputRequired
        );
        assert_eq!(
            ChatStatus::from_state(false, false, true, true),
            ChatStatus::InProgress
        );
        assert_eq!(
            ChatStatus::from_state(false, false, false, true),
            ChatStatus::Unread
        );
        assert_eq!(
            ChatStatus::from_state(false, false, false, false),
            ChatStatus::Ready
        );
        let mut library = Library::default();
        library.chat_grouping = ChatGrouping::Status;
        let restored: Library =
            serde_json::from_str(&serde_json::to_string(&library).expect("serialize"))
                .expect("restore");
        assert_eq!(restored.chat_grouping, ChatGrouping::Status);
    }

    #[test]
    fn sidebar_tracks_async_questions_until_the_user_answers() {
        let fixture = Fixture::new();
        let mut rows = vec![
            json!({"type":"response_item","payload":{"type":"function_call","name":"functions.request_user_input_async","call_id":"q","arguments":"{\"questions\":[{\"title\":\"Choose\",\"options\":[\"A\",\"B\"]}]}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"q","output":"{\"accepted\":true}"}}),
        ];
        let path = fixture.write("questions.jsonl", &rows);
        let chat = Chat {
            provider: Provider::Codex,
            id: "q-chat".into(),
            title: String::new(),
            cwd: String::new(),
            path,
            modified: SystemTime::now(),
            archived: false,
            internal: false,
        };
        assert!(
            chat_activity(&chat, None)
                .expect("waiting")
                .pending_questions
                .contains("q")
        );
        rows.push(json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<send_user_message_question_reply>\n[{\"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"q\\\",0]\",\"answer\":\"A\"}]\n</send_user_message_question_reply>"}]}}));
        fixture.write("questions.jsonl", &rows);
        assert!(
            chat_activity(&chat, None)
                .expect("answered")
                .pending_questions
                .is_empty()
        );
    }

    #[test]
    fn codex_goal_continuation_is_status_without_internal_instructions() {
        let value = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal.\n<objective>Review the errors</objective>\n</codex_internal_context>"}]}});
        let message = visible_message(Provider::Codex, &value).expect("goal status");
        assert_eq!(message.role, "activity");
        assert_eq!(message.tool_name, "Continue goal");
        assert_eq!(message.text, "Review the errors");
        assert!(!message.text.contains("codex_internal_context"));
        assert!(!is_context(
            "Here is an example: <codex_internal_context source=\"goal\">"
        ));
    }

    #[test]
    fn automatic_goal_check_in_is_activity_but_real_user_text_is_preserved() {
        let mut value = json!({"type":"user","isMeta":true,"message":{"role":"user","content":"Goal check-in: work is still running"}});
        let automatic = visible_message(Provider::Claude, &value).expect("status");
        assert_eq!(automatic.role, "activity");
        assert_eq!(automatic.tool_name, "Goal check-in");
        assert!(automatic.tool_finished);
        value["isMeta"] = Value::Bool(false);
        assert_eq!(
            visible_message(Provider::Claude, &value)
                .expect("user")
                .role,
            "user"
        );
    }

    #[test]
    fn claude_effort_and_turn_start_are_read_from_external_history() {
        let fixture = Fixture::new();
        let path = fixture.write("effort.jsonl", &[
            json!({"type":"api-request","params":{"model":"claude-model","output_config":{"effort":"high"}}}),
        ]);
        let chat = Chat {
            provider: Provider::Claude,
            id: "parent".into(),
            title: String::new(),
            cwd: String::new(),
            path,
            modified: SystemTime::now(),
            archived: false,
            internal: false,
        };
        let history = transcript(&chat, 20).expect("history");
        assert!(history.running);
        assert_eq!(history.metrics.effort.as_deref(), Some("high"));
        assert_eq!(history.metrics.model.as_deref(), Some("claude-model"));
        let mut metrics = history.metrics;
        metrics.observe(
            Provider::Claude,
            &json!({"type":"assistant","effort":"low","isSidechain":true}),
        );
        assert_eq!(metrics.effort.as_deref(), Some("high"));
        metrics.observe(
            Provider::Claude,
            &json!({"type":"assistant","perTurnEffort":"medium"}),
        );
        assert_eq!(metrics.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn subtask_opens_only_the_history_linked_to_its_tool_use() {
        let fixture = Fixture::new();
        let path = fixture.write("parent.jsonl", &[]);
        let parent = Chat {
            provider: Provider::Claude,
            id: "parent".into(),
            title: String::new(),
            cwd: "C:/work".into(),
            path,
            modified: SystemTime::now(),
            archived: false,
            internal: false,
        };
        let directory = parent.path.with_extension("").join("subagents");
        fs::create_dir_all(&directory).expect("directory");
        fs::write(
            directory.join("agent-child.meta.json"),
            json!({"toolUseId":"tool-1","description":"Inspect docs"}).to_string(),
        )
        .expect("metadata");
        let row = json!({"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"Child answer"}],"stop_reason":"end_turn"}});
        fs::write(directory.join("agent-child.jsonl"), format!("{row}\n")).expect("child");
        let child = claude_subtask_chat(&parent, "tool-1").expect("linked child");
        assert!(is_subtask_chat(&child));
        assert_eq!(child.title, "Inspect docs");
        assert_eq!(child.cwd, parent.cwd);
        assert_eq!(
            transcript(&child, 20).expect("transcript").messages[0].text,
            "Child answer"
        );
        assert!(visible_message(Provider::Claude, &row).is_none());
        assert!(claude_subtask_chat(&parent, "another-tool").is_err());
        let task = Message {
            role: "activity".into(),
            tool_name: "Agent".into(),
            ..Default::default()
        };
        assert_eq!(
            conversation_parts(&[&task]),
            vec![ConversationPart::Message(0)]
        );
    }

    #[test]
    fn pasted_envelopes_preserve_content_and_code_examples() {
        assert_eq!(
            clean_user_text(
                "Please review\n<pasted_content id=\"x\">\n[Source](https://example.com)\n</pasted_content id=\"x\">\nThanks"
            ),
            "Please review\n[Source](https://example.com)\nThanks"
        );
        let example = "```xml\n<pasted_content id=\"x\">\nexample\n</pasted_content>\n```";
        assert_eq!(clean_user_text(example), example);
    }

    #[test]
    fn claude_plan_and_progress_survive_shared_api_message_ids() {
        let fixture = Fixture::new();
        let rows = [
            json!({"type":"assistant","uuid":"one","message":{"id":"same","role":"assistant","content":[{"type":"tool_use","id":"plan","name":"mcp__ccd_turn__update_plan","input":{"items":[{"content":"Check infrastructure","status":"in_progress"}]}}]}}),
            json!({"type":"user","uuid":"two","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"plan","content":"OK"}]}}),
            json!({"type":"assistant","uuid":"three","message":{"id":"same","role":"assistant","content":[{"type":"text","text":"The profile is live."}]}}),
        ];
        let path = fixture.write("plan.jsonl", &rows);
        let chat = metadata(path, Provider::Claude, false, &HashMap::new()).expect("chat");
        let transcript = transcript(&chat, 200).expect("transcript");
        assert_eq!(transcript.messages.len(), 2);
        assert_eq!(transcript.messages[0].role, "plan");
        assert_eq!(plan_steps(&transcript.messages[0])[0].status, "in_progress");
        assert_eq!(transcript.messages[1].text, "The profile is live.");
    }

    #[test]
    fn plans_support_both_providers_and_only_latest_update_per_turn_is_visible() {
        let mut first = Message {
            tool_name: "TodoWrite".into(),
            tool_input: json!({"todos":[{"content":"Read","status":"pending"}]}).to_string(),
            ..Default::default()
        };
        normalize_plan(&mut first);
        let mut second = Message {
            tool_name: "functions.update_plan".into(),
            tool_input: json!({"plan":[{"step":"Read","status":"completed"}]}).to_string(),
            ..Default::default()
        };
        normalize_plan(&mut second);
        let user = Message {
            role: "user".into(),
            ..Default::default()
        };
        assert_eq!(
            conversation_parts(&[&first, &second, &user, &first]),
            vec![
                ConversationPart::Message(1),
                ConversationPart::Message(2),
                ConversationPart::Message(3)
            ]
        );
        assert_eq!(plan_steps(&second)[0].status, "completed");
    }

    #[test]
    fn work_summary_describes_commands_agents_and_failures() {
        let command = Message {
            role: "activity".into(),
            tool_name: "Bash".into(),
            tool_error: true,
            ..Default::default()
        };
        let agent = Message {
            role: "activity".into(),
            tool_name: "Agent".into(),
            ..Default::default()
        };
        assert_eq!(
            work_summary(&[&command, &agent]),
            "1 command · 1 agent · 1 failed"
        );
    }

    #[test]
    fn incremental_claude_plan_updates_advance_and_complete_numbered_steps() {
        let previous = (0..5)
            .map(|index| PlanStep {
                text: format!("Task {index}"),
                status: if index == 0 { "in_progress" } else { "pending" }.into(),
            })
            .collect::<Vec<_>>();
        let mut advance = Message {
            tool_name: "mcp__ccd_turn__update_plan".into(),
            tool_input: r#"{"advance":true}"#.into(),
            ..Default::default()
        };
        let advanced = apply_plan_update(&mut advance, &previous);
        assert_eq!(advanced[0].status, "completed");
        assert_eq!(advanced[1].status, "in_progress");
        let mut update = Message {
            tool_input: r#"{"done":[2,3,4],"start":5}"#.into(),
            ..advance.clone()
        };
        let updated = apply_plan_update(&mut update, &advanced);
        assert!(updated[..4].iter().all(|step| step.status == "completed"));
        assert_eq!(updated[4].status, "in_progress");
        assert_eq!(plan_steps(&update), updated);
        let mut invalid = Message {
            tool_input: r#"{"done":[0,99],"start":0}"#.into(),
            ..advance
        };
        assert!(apply_plan_update(&mut invalid, &updated).is_empty());
    }

    #[test]
    fn protocol_heartbeat_reports_are_prose_in_history_and_live_updates() {
        let xml = "<heartbeat><automation_id>timer</automation_id><decision>NOTIFY</decision><message>Progress &amp; details: <![CDATA[83% < 100%]]></message></heartbeat>";
        let original = Message {
            role: "assistant".into(),
            text: xml.into(),
            timestamp: "2026-11-03T10:00:00Z".into(),
            ..Default::default()
        };
        let live = normalize_protocol_message(original.clone());
        assert_eq!(live.role, "assistant");
        assert_eq!(live.text, "Progress & details: 83% < 100%");
        assert_eq!(normalize_protocol_message(live.clone()).text, live.text);
        for provider in [Provider::Codex, Provider::Claude] {
            let row = match provider {
                Provider::Codex => {
                    json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":xml}})
                }
                Provider::Claude => {
                    json!({"type":"assistant","message":{"role":"assistant","content":xml}})
                }
            };
            let history = visible_message(provider, &row).expect("report");
            assert_eq!(history.text, live.text);
            assert_eq!(history.role, live.role);
        }
    }

    #[test]
    fn protocol_scheduled_instructions_and_quiet_results_are_activity() {
        for (role, xml) in [
            (
                "user",
                "<heartbeat><automation_id>timer</automation_id><current_time_iso>2026-11-03T10:00:00Z</current_time_iso><instructions>Inspect progress</instructions></heartbeat>",
            ),
            (
                "assistant",
                "<heartbeat><automation_id>timer</automation_id><decision>QUIET</decision></heartbeat>",
            ),
        ] {
            let normalized = normalize_protocol_message(Message {
                role: role.into(),
                text: xml.into(),
                ..Default::default()
            });
            assert_eq!(normalized.role, "activity");
            assert!(normalized.tool_finished);
            assert!(!normalized.text.contains("<"));
            assert_eq!(normalized.tool_input, xml);
            assert!(matches!(
                conversation_parts(&[&normalized])[0],
                ConversationPart::Work(_)
            ));
        }
    }

    #[test]
    fn protocol_future_envelopes_use_structure_instead_of_a_tag_allowlist() {
        for root in ["worker_notification", "job-event", "new_transport_wrapper"] {
            let xml = format!(
                "<{root}><session_id>session</session_id><status>failed</status><summary>Job failed</summary></{root}>"
            );
            let normalized = normalize_protocol_message(Message {
                role: "user".into(),
                text: xml.clone(),
                ..Default::default()
            });
            assert_eq!(normalized.role, "activity", "{root}");
            assert_eq!(normalized.text, "Job failed");
            assert!(normalized.tool_error);
            assert_eq!(normalized.tool_input, xml);
        }
    }

    #[test]
    fn protocol_code_examples_and_mixed_prose_remain_unchanged() {
        for text in [
            "```xml\n<heartbeat><decision>NOTIFY</decision></heartbeat>\n```",
            "<div><p>Hello</p></div>",
            "<config><message>Keep this XML</message></config>",
            "<job_event><message>A source-code example without transport IDs</message></job_event>",
            "Here is XML: <heartbeat><decision>NOTIFY</decision></heartbeat>",
            "<heartbeat><message>Example</message></heartbeat>\nPlease explain this XML.",
            "<config><thread_id>123</thread_id><name>Source configuration</name></config>",
        ] {
            let normalized = normalize_protocol_message(Message {
                role: "user".into(),
                text: text.into(),
                ..Default::default()
            });
            assert_eq!(normalized.role, "user", "{text}");
            assert_eq!(normalized.text, text);
        }
    }

    #[test]
    fn protocol_streaming_and_broken_envelopes_do_not_leak_into_bubbles() {
        for xml in [
            "<heartbeat><automation_id>timer</automation_id><instructions>Still arriving",
            "<heartbeat><automation_id>timer</broken></heartbeat>",
            "<task-notification><task-id>job</task-id><status>",
        ] {
            let normalized = normalize_protocol_message(Message {
                role: "user".into(),
                text: xml.into(),
                ..Default::default()
            });
            assert_eq!(normalized.role, "activity");
            assert_eq!(normalized.tool_input, xml);
        }
        let partial = normalize_protocol_message(Message {
            role: "assistant".into(),
            text: "<heartbeat><decision>NOTIFY</decision><message>Loading report".into(),
            ..Default::default()
        });
        assert_eq!(partial.role, "assistant");
        assert_eq!(partial.text, "Loading report");
    }

    #[test]
    fn protocol_internal_messages_do_not_create_visible_rows_or_merge_across_them() {
        let internal = normalize_protocol_message(Message {
            role: "user".into(),
            text: "<recommended_plugins>internal</recommended_plugins>".into(),
            ..Default::default()
        });
        assert_eq!(internal.role, "internal");
        let work = Message {
            role: "activity".into(),
            ..Default::default()
        };
        assert_eq!(
            conversation_parts(&[&work, &internal, &work]),
            vec![ConversationPart::Work(0..1), ConversationPart::Work(2..3)]
        );
        assert!(
            protocol_envelope(
                "<!DOCTYPE example [<!ENTITY x SYSTEM 'file:///secret'>]><example>&x;</example>"
            )
            .is_none()
        );
    }

    #[test]
    fn async_questions_are_cards_and_acknowledgement_is_not_an_answer() {
        let fixture = Fixture::new();
        let rows = vec![
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"question","name":"functions.request_user_input_async","arguments":json!({"questions":[{"title":"Theme?","options":["Dark","Light"]}]}).to_string()}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"question","output":"{\"accepted\":true}"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"<external_codex_apps_open_page>{\"page_id\":null}</external_codex_apps_open_page>"}}),
        ];
        let path = fixture.write("questions.jsonl", &rows);
        let chat = metadata(path, Provider::Codex, false, &HashMap::new()).expect("chat");
        let pending = transcript(&chat, 200).expect("pending");
        assert_eq!(pending.messages.len(), 1);
        assert_eq!(pending.messages[0].role, "question");
        assert!(pending.messages[0].question_answers.is_empty());
        let mut answered = rows;
        answered.push(json!({"type":"response_item","payload":{"type":"message","role":"user","content":format!("<send_user_message_question_reply>{}</send_user_message_question_reply>", json!([{"questionItemId":json!(["request_user_input_async","question",0]).to_string(),"answer":"Dark"}]))}}));
        fixture.write("questions.jsonl", &answered);
        let answered = transcript(&chat, 200).expect("answered");
        assert_eq!(answered.messages.len(), 1);
        assert_eq!(
            answered.messages[0]
                .question_answers
                .get("0")
                .map(String::as_str),
            Some("Dark")
        );
    }

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
                fork_source: None,
                quick_chat: true,
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
        assert!(library.chats[&chat.key()].quick_chat);
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
            static NEXT_FIXTURE: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "zed-chat-history-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos(),
                NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    fn codex_final_answer_clears_activity_and_transcript_without_completion_event() {
        let fixture = Fixture::new();
        let path = fixture.write("final.jsonl", &[
            json!({"type":"session_meta","payload":{"id":"s","cwd":"C:/test"}}),
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Done"}]}}),
        ]);
        let chat = metadata(path, Provider::Codex, false, &HashMap::new()).expect("chat");
        assert!(!chat_activity(&chat, None).unwrap().running);
        assert!(!transcript(&chat, 100).unwrap().running);
        assert_eq!(
            codex_turn_running(
                &json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary"}})
            ),
            None
        );
        assert_eq!(
            codex_turn_running(&json!({"type":"event_msg","payload":{"type":"task_started"}})),
            Some(true)
        );
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
            last_user_message_at: None,
            running: true,
            reply: Some("old".into()),
            pending_questions: HashSet::new(),
            subtasks: ClaudeSubtasks::default(),
            turn_running: true,
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
    fn claude_tool_images_are_grouped_with_activity_and_companion_notes_are_hidden() {
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
        assert_eq!(transcript.messages[1].role, "tool_image");
        assert_eq!(
            conversation_parts(&transcript.messages.iter().collect::<Vec<_>>()),
            vec![ConversationPart::Work(0..2)]
        );
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
            layout: None,
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
