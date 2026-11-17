use super::chat_history_store::{Chat, RemoteMessage, RemoteTool, RemoteTurn};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

#[derive(Debug)]
struct DesktopUnavailable;

impl std::fmt::Display for DesktopUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Open Codex on this PC to connect")
    }
}

impl std::error::Error for DesktopUnavailable {}

pub fn connection_notice(error: &anyhow::Error) -> &'static str {
    if error.downcast_ref::<DesktopUnavailable>().is_some() {
        "Open Codex on this PC to connect."
    } else {
        "Remote chat unavailable. The other device may be offline."
    }
}

struct Client<S> {
    stream: S,
    id: String,
    owner: String,
    host: String,
    thread: String,
}

async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    stream.write_u32_le(bytes.len().try_into()?).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<Value> {
    let length = stream.read_u32_le().await? as usize;
    // Desktop snapshots contain the entire thread, including inline images. Parse
    // large frames progressively without reserving the untrusted length up front.
    if length > 1024 * 1024 {
        let (mut writer, reader) = tokio::io::duplex(256 * 1024);
        let parser = tokio::task::spawn_blocking(move || {
            let reader = tokio_util::io::SyncIoBridge::new(reader);
            serde_json::from_reader(std::io::BufReader::with_capacity(128 * 1024, reader))
        });
        let mut frame = tokio::io::BufReader::with_capacity(128 * 1024, stream.take(length as u64));
        let copied = tokio::io::copy_buf(&mut frame, &mut writer).await;
        drop(writer);
        let parsed = parser.await??;
        ensure!(copied? == length as u64, "Incomplete Codex desktop frame");
        return Ok(parsed);
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    async fn request(&mut self, method: &str, base_version: u8, params: Value) -> Result<Value> {
        let id = Uuid::new_v4().to_string();
        // Codex selects the next protocol version when a follower request carries
        // a hostId. Discovery carries its host inside params and uses its base version.
        let targeted = !self.owner.is_empty();
        let remote = targeted && self.host != "local";
        let version = base_version + u8::from(remote && method.starts_with("thread-follower-"));
        let mut message = json!({"type":"request","requestId":id,"sourceClientId":self.id,
            "method":method,"version":version,"params":params,"timeoutMs":12000});
        if targeted {
            message["targetClientId"] = json!(self.owner);
            if remote {
                message["hostId"] = json!(self.host);
            }
        }
        write_frame(&mut self.stream, &message).await?;
        loop {
            let message = self.next().await?;
            if message["type"] != "response" || message["requestId"] != id {
                continue;
            }
            if message["resultType"] == "error" && message["error"] == "no-client-found" && targeted
            {
                bail!(
                    "Codex could not route this request to the remote chat. Reopen the chat and try again."
                );
            }
            ensure!(
                message["resultType"] == "success",
                "{}",
                message["error"]
                    .as_str()
                    .unwrap_or("Codex rejected the request")
            );
            ensure!(
                message["method"] == method,
                "Unexpected Codex desktop reply"
            );
            if !self.owner.is_empty() {
                ensure!(
                    message["handledByClientId"] == self.owner,
                    "Codex conversation owner changed"
                );
            }
            return Ok(message);
        }
    }

    async fn next(&mut self) -> Result<Value> {
        loop {
            let message = read_frame(&mut self.stream).await?;
            if message["type"] == "client-discovery-request" {
                write_frame(
                    &mut self.stream,
                    &json!({"type":"client-discovery-response",
                    "requestId":message["requestId"],"response":{"canHandle":false}}),
                )
                .await?;
            } else {
                return Ok(message);
            }
        }
    }

    async fn init(stream: S, chat: &Chat) -> Result<Self> {
        let (host, thread) = if chat.is_remote_codex() {
            chat.id.split_once('/').context("Invalid remote chat ID")?
        } else {
            ("local", chat.id.as_str())
        };
        ensure!(
            chat.provider == super::chat_history_store::Provider::Codex
                && Uuid::parse_str(thread).is_ok(),
            "Invalid Codex conversation"
        );
        let mut client = Self {
            stream,
            id: "uninitialized".into(),
            owner: String::new(),
            host: host.into(),
            thread: thread.into(),
        };
        let reply = client
            .request("initialize", 0, json!({"clientType":"zed"}))
            .await?;
        client.id = reply["result"]["clientId"]
            .as_str()
            .context("Codex client ID missing")?
            .into();
        let reply = client
            .request(
                "thread-owner-discovery",
                1,
                json!({"hostId":host,"conversationId":thread}),
            )
            .await
            .context("Open this conversation in Codex Desktop on this PC, then try again")?;
        client.owner = reply["handledByClientId"]
            .as_str()
            .context("Codex conversation owner missing")?
            .into();
        Ok(client)
    }

    async fn send_to_current_turn(&mut self, input: Value, message_id: Uuid) -> Result<()> {
        self.follow().await?;
        let change = self.next_change().await?;
        ensure!(
            change["type"] == "snapshot",
            "Codex did not send a conversation snapshot"
        );
        let state = &change["conversationState"];
        ensure!(
            state["id"] == self.thread && state["hostId"] == self.host,
            "Unexpected Codex conversation"
        );
        let update = snapshot_turns(state, 0)?;
        ensure!(
            !update.running || update.active_turn.is_some(),
            "Codex is busy but has no steerable turn. Try again when it is ready."
        );
        self.send_input(input, message_id, update.running).await
    }

    async fn send_input(&mut self, input: Value, message_id: Uuid, steer: bool) -> Result<()> {
        if steer {
            self.request(
                "thread-follower-steer-turn",
                1,
                json!({
                    "conversationId":self.thread, "input":input, "clientUserMessageId":message_id
                }),
            )
            .await?;
            return Ok(());
        }
        let thread = self.thread.clone();
        self.request("thread-follower-start-turn",2,json!({"conversationId":thread,
            "turnStart":{"request":{"threadId":thread,"clientUserMessageId":message_id,"input":input},
            "context":{"inheritThreadSettings":true}}})).await?;
        Ok(())
    }

    async fn set_model_options(
        &mut self,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<()> {
        let mut settings = serde_json::Map::new();
        if let Some(model) = model {
            settings.insert("model".into(), json!(model));
        }
        if let Some(effort) = effort {
            settings.insert("effort".into(), json!(effort));
        }
        ensure!(!settings.is_empty(), "Choose a model or effort first");
        let reply = self
            .request(
                "thread-follower-update-thread-settings",
                2,
                json!({"conversationId":self.thread,"threadSettings":settings}),
            )
            .await?;
        ensure!(
            reply["result"]["applied"] == true,
            "Codex did not apply the model settings"
        );
        Ok(())
    }

    async fn follow(&mut self) -> Result<()> {
        write_frame(&mut self.stream,&json!({"type":"broadcast","method":"thread-stream-following-changed","sourceClientId":self.id,
            "targetClientIds":[self.owner],"version":1,"params":{"conversationId":self.thread,"hostId":self.host,"following":true}})).await?;
        Ok(())
    }

    async fn next_change(&mut self) -> Result<Value> {
        loop {
            let mut message = self.next().await?;
            let p = &message["params"];
            if message["type"] == "broadcast"
                && message["sourceClientId"] == self.owner
                && message["method"] == "thread-stream-state-changed"
                && p["hostId"] == self.host
                && p["conversationId"] == self.thread
            {
                return Ok(message["params"]["change"].take());
            }
        }
    }
}

async fn connect(chat: &Chat) -> Result<Client<tokio::net::windows::named_pipe::NamedPipeClient>> {
    let pipe = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(r"\\.\pipe\codex-ipc")
        .context(DesktopUnavailable)?;
    tokio::time::timeout(Duration::from_secs(8), Client::init(pipe, chat))
        .await
        .context("Codex Desktop did not respond")?
}

fn remote_input(prompt: &str, attachments: &[std::path::PathBuf]) -> Result<Value> {
    use base64::Engine as _;
    let mut input = Vec::new();
    if !prompt.is_empty() {
        input.push(json!({"type":"text","text":prompt,"text_elements":[]}));
    }
    let mut total: u64 = 0;
    for path in attachments {
        let mime = super::chat_runtime::image_mime(path)
            .context("Remote chats support image attachments")?;
        total = total.saturating_add(std::fs::metadata(path)?.len());
        ensure!(total <= 20 * 1024 * 1024, "Attached images exceed 20 MB");
        let encoded = base64::engine::general_purpose::STANDARD.encode(std::fs::read(path)?);
        input.push(json!({"type":"image","url":format!("data:{mime};base64,{encoded}")}));
    }
    ensure!(!input.is_empty(), "Empty remote message");
    Ok(Value::Array(input))
}

pub async fn set_model_options(
    chat: &Chat,
    model: Option<String>,
    effort: Option<String>,
) -> Result<()> {
    let mut client = connect(chat).await?;
    tokio::time::timeout(
        Duration::from_secs(15),
        client.set_model_options(model, effort),
    )
    .await
    .context("Timed out waiting for remote model settings")?
}

pub async fn interrupt(chat: &Chat, turn: Uuid) -> Result<()> {
    let mut client = connect(chat).await?;
    let params = json!({"conversationId":client.thread,"mode":"user-stop","expectedTurnId":turn});
    let reply = tokio::time::timeout(
        Duration::from_secs(15),
        client.request("thread-follower-interrupt-turn", 4, params),
    )
    .await
    .context("Timed out waiting for the remote turn to stop")??;
    ensure!(
        reply["result"]["ok"] == true,
        "Codex could not stop this turn"
    );
    Ok(())
}

pub async fn send(
    chat: &Chat,
    prompt: &str,
    attachments: Vec<std::path::PathBuf>,
    message_id: Uuid,
    steer: bool,
) -> Result<()> {
    let prompt = prompt.to_owned();
    let input = tokio::task::spawn_blocking(move || remote_input(&prompt, &attachments)).await??;
    let mut client = connect(chat).await?;
    tokio::time::timeout(
        Duration::from_secs(18),
        client.send_input(input, message_id, steer),
    )
    .await
    .context("Delivery unconfirmed. Check the original chat before retrying")??;
    Ok(())
}

pub async fn send_to_local_owner(chat: &Chat, input: Value, message_id: Uuid) -> Result<()> {
    ensure!(
        !chat.is_remote_codex(),
        "Expected a local Codex conversation"
    );
    let mut client = connect(chat).await?;
    tokio::time::timeout(
        Duration::from_secs(20),
        client.send_to_current_turn(input, message_id),
    )
    .await
    .context("Delivery unconfirmed. Check the original chat before retrying")??;
    Ok(())
}

pub struct Update {
    pub turns: Vec<RemoteTurn>,
    pub running: bool,
    pub activity: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub active_turn: Option<Uuid>,
}

pub async fn follow(
    chat: &Chat,
    updates: tokio::sync::watch::Sender<Option<std::sync::Arc<Update>>>,
) -> Result<()> {
    let mut client = connect(chat).await?;
    client.follow().await?;
    let owner = client.owner.clone();
    let since = super::chat_history_store::remote_export_time_ms(&chat.path)?;
    let mut state = Value::Null;
    let mut revision = None;
    loop {
        let change = if revision.is_none() {
            tokio::time::timeout(Duration::from_secs(90), client.next_change())
                .await
                .context("Codex did not send an initial snapshot")??
        } else {
            // Keep the same reader across idle checks, even if it has received
            // part of a frame. Resubscribing transfers the full history again.
            let change = client.next_change();
            tokio::pin!(change);
            loop {
                tokio::select! {
                    result = &mut change => break result?,
                    _ = updates.closed() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        ensure!(connect(chat).await?.owner == owner, "Codex conversation owner changed");
                    }
                }
            }
        };
        if !apply_change(&mut state, &mut revision, change)? {
            continue;
        }
        ensure!(
            state["id"] == client.thread && state["hostId"] == client.host,
            "Unexpected remote conversation"
        );
        if updates
            .send(Some(std::sync::Arc::new(snapshot_turns(&state, since)?)))
            .is_err()
        {
            return Ok(());
        }
    }
}

fn apply_change(state: &mut Value, revision: &mut Option<u64>, mut change: Value) -> Result<bool> {
    let next = change["revision"]
        .as_u64()
        .context("Missing Codex revision")?;
    if revision.is_some_and(|current| next <= current) {
        return Ok(false);
    }
    match change["type"].as_str() {
        Some("snapshot") => *state = change["conversationState"].take(),
        Some("patches") => {
            ensure!(
                revision.is_some() && *revision == change["baseRevision"].as_u64(),
                "Codex stream needs a fresh snapshot"
            );
            for patch in change["patches"]
                .as_array_mut()
                .context("Missing Codex patches")?
            {
                let value = patch["value"].take();
                let path = patch["path"].as_array().context("Invalid patch path")?;
                let op = patch["op"].as_str().context("Missing patch operation")?;
                let Some((last, parents)) = path.split_last() else {
                    ensure!(op == "replace", "Invalid root patch");
                    *state = value;
                    continue;
                };
                let mut parent = &mut *state;
                for key in parents {
                    parent = if let Some(name) = key.as_str() {
                        parent.get_mut(name)
                    } else {
                        key.as_u64().and_then(|n| parent.get_mut(n as usize))
                    }
                    .context("Patch parent missing")?;
                }
                if let (Some(map), Some(name)) = (parent.as_object_mut(), last.as_str()) {
                    match op {
                        "add" | "replace" => {
                            map.insert(name.into(), value);
                        }
                        "remove" => {
                            map.remove(name);
                        }
                        _ => bail!("Unsupported Codex object operation"),
                    }
                } else {
                    let list = parent.as_array_mut().context("Invalid patch target")?;
                    let i = last.as_u64().context("Invalid patch index")? as usize;
                    match op {
                        "add" if i <= list.len() => list.insert(i, value),
                        "replace" if i < list.len() => list[i] = value,
                        "remove" if i < list.len() => {
                            list.remove(i);
                        }
                        _ => bail!("Invalid Codex list operation"),
                    }
                }
            }
        }
        _ => bail!("Unknown Codex stream update"),
    }
    *revision = Some(next);
    Ok(true)
}

fn snapshot_turns(state: &Value, since: u64) -> Result<Update> {
    let mut turns = Vec::new();
    let mut running = false;
    let mut activity = None;
    let mut active_turn = None;
    let mut latest = None;
    let legacy = state["turns"].as_array();
    let paginated = state["turnHistory"]["history"]["entitiesByKey"].as_object();
    if legacy.is_none() && paginated.is_none() {
        bail!("Unsupported Codex conversation format");
    }
    for turn in legacy
        .into_iter()
        .flatten()
        .chain(paginated.into_iter().flat_map(|m| m.values()))
    {
        let Some(id) = turn["turnId"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            continue;
        };
        let started_at = turn["turnStartedAtMs"].as_u64().unwrap_or_default();
        let active = turn["status"] == "inProgress";
        if latest.is_none_or(|time| started_at >= time) {
            running = active;
            latest = Some(started_at);
            let items = turn["items"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            activity = active.then(|| live_activity(items)).flatten();
            active_turn = active.then_some(id);
        }
        if !active && started_at.saturating_add(turn["durationMs"].as_u64().unwrap_or(0)) < since {
            continue;
        }
        let mut messages = Vec::new();
        for item in turn["items"].as_array().into_iter().flatten() {
            if let Some(tool) = remote_tool(item) {
                messages.push(RemoteMessage {
                    role: "activity".into(),
                    text: String::new(),
                    phase: String::new(),
                    images: Vec::new(),
                    tool: Some(tool),
                });
                continue;
            }
            let (role, text) = match item["type"].as_str() {
                Some("userMessage") => ("user", input_text(&item["content"])),
                Some("steeringUserMessage") => ("user", input_text(&item["input"])),
                Some("agentMessage") => (
                    "assistant",
                    item["text"].as_str().unwrap_or_default().to_owned(),
                ),
                _ => continue,
            };
            let content = if item["type"] == "steeringUserMessage" {
                &item["input"]
            } else {
                &item["content"]
            };
            let images = input_images(content);
            if !text.is_empty() || !images.is_empty() {
                messages.push(RemoteMessage {
                    tool: None,
                    images,
                    role: role.into(),
                    text,
                    phase: item["phase"].as_str().unwrap_or_default().into(),
                })
            }
        }
        if !messages.iter().any(|m| m.role == "user") {
            let text = input_text(&turn["params"]["input"]);
            let images = input_images(&turn["params"]["input"]);
            if !text.is_empty() || !images.is_empty() {
                messages.insert(
                    0,
                    RemoteMessage {
                        tool: None,
                        images,
                        role: "user".into(),
                        text,
                        phase: String::new(),
                    },
                );
            }
        }
        if !messages.is_empty() {
            turns.push(RemoteTurn {
                id,
                started_at,
                completed: !active,
                messages,
            });
        }
    }
    match state["threadRuntimeStatus"]["type"].as_str() {
        Some("active") => running = true,
        Some("idle") => {
            running = false;
            active_turn = None;
            activity = None;
        }
        _ => {}
    }
    turns.sort_by_key(|t| (t.started_at, t.id));
    turns.dedup_by_key(|t| t.id);
    Ok(Update {
        turns,
        running,
        activity,
        active_turn,
        model: state["latestCollaborationMode"]["settings"]["model"]
            .as_str()
            .or_else(|| state["latestThreadSettings"]["model"].as_str())
            .or_else(|| state["latestModel"].as_str())
            .map(str::to_owned),
        effort: state["latestReasoningEffort"]
            .as_str()
            .or_else(|| state["latestThreadSettings"]["effort"].as_str())
            .or_else(|| state["latestCollaborationMode"]["settings"]["reasoning_effort"].as_str())
            .map(str::to_owned),
    })
}

fn remote_tool(item: &Value) -> Option<RemoteTool> {
    let (name, input) = match item["type"].as_str()? {
        "commandExecution" => (
            "exec_command",
            item["command"].as_str().unwrap_or_default().to_owned(),
        ),
        "fileChange" => (
            "Edit",
            item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|change| change["path"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "webSearch" => (
            "Search",
            item["query"].as_str().unwrap_or_default().to_owned(),
        ),
        "mcpToolCall" => (item["tool"].as_str()?, String::new()),
        _ => return None,
    };
    Some(RemoteTool {
        id: item["id"].as_str()?.into(),
        name: name.into(),
        input,
        finished: item["status"] != "inProgress",
        failed: item["status"] == "failed"
            || item["exitCode"].as_i64().is_some_and(|code| code != 0),
    })
}

fn live_activity(items: &[Value]) -> Option<String> {
    for item in items.iter().rev() {
        match item["type"].as_str() {
            Some("reasoning") => {
                if let Some(summary) = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .rev()
                    .filter_map(Value::as_str)
                    .find(|s| !s.trim().is_empty())
                {
                    return Some(summary.to_owned());
                }
            }
            Some("commandExecution" | "fileChange" | "mcpToolCall")
                if item["status"] == "inProgress" =>
            {
                return Some(
                    match item["type"].as_str() {
                        Some("commandExecution") => "Running a command",
                        Some("fileChange") => "Editing files",
                        _ => "Using a tool",
                    }
                    .into(),
                );
            }
            Some("agentMessage" | "userMessage" | "steeringUserMessage") => break,
            _ => {}
        }
    }
    None
}

fn input_images(content: &Value) -> Vec<String> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "image" || block["type"] == "input_image")
        .filter_map(|block| {
            block["url"]
                .as_str()
                .or_else(|| block["image_url"].as_str())
        })
        .filter(|url| url.starts_with("data:image/") && url.len() <= 28 * 1024 * 1024)
        .map(str::to_owned)
        .collect()
}

fn input_text(input: &Value) -> String {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "text")
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zed::chat_history_store::Provider;
    use std::time::SystemTime;

    #[test]
    fn image_only_remote_turn_survives_snapshot_and_transcript() {
        let id = Uuid::new_v4();
        let image = "data:image/png;base64,iVBORw0KGgo=";
        let mut state = json!({"turns":[{"turnId":id,"turnStartedAtMs":1000,
            "status":"completed","params":{"input":[{"type":"image","url":image}]},
            "items":[{"type":"agentMessage","text":"Image received"}]}]});
        let dir = tempfile::tempdir().unwrap();
        let mut target = chat("remote-control:test", &id.to_string());
        target.path = dir.path().join("export.jsonl");
        std::fs::write(
            &target.path,
            json!({"host_id":"remote-control:test","thread_id":id,"updated_at":0,"title":"test","cwd":"/remote"}).to_string(),
        )
        .unwrap();
        for with_text in [false, true] {
            if with_text {
                state["turns"][0]["params"]["input"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"text","text":"Look at this"}));
            }
            let update = snapshot_turns(&state, 0).unwrap();
            assert_eq!(update.turns[0].messages[0].role, "user");
            assert_eq!(update.turns[0].messages[0].images, [image]);
            super::super::chat_history_store::save_remote_turns(&target, &update.turns).unwrap();
            let transcript = super::super::chat_history_store::transcript(&target, 20).unwrap();
            assert_eq!(transcript.total, 2);
            assert_eq!(transcript.messages[0].attachments.len(), 1);
            assert_eq!(
                transcript.messages[0].text,
                if with_text { "Look at this" } else { "" }
            );
        }
    }

    #[test]
    fn old_unfinished_remote_turn_does_not_keep_completed_chat_working() {
        let old = Uuid::new_v4();
        let latest = Uuid::new_v4();
        let mut state = json!({"turns":[
            {"turnId":latest,"turnStartedAtMs":2000,"status":"completed","items":[]},
            {"turnId":old,"turnStartedAtMs":1000,"status":"inProgress","items":[]}
        ]});
        let update = snapshot_turns(&state, 0).unwrap();
        assert!(!update.running);
        assert_eq!(update.active_turn, None);
        state["threadRuntimeStatus"] = json!({"type":"active"});
        assert!(snapshot_turns(&state, 0).unwrap().running);
        state["turns"][0]["status"] = json!("inProgress");
        assert_eq!(snapshot_turns(&state, 0).unwrap().active_turn, Some(latest));
        state["threadRuntimeStatus"] = json!({"type":"idle"});
        let update = snapshot_turns(&state, 0).unwrap();
        assert!(!update.running);
        assert_eq!(update.active_turn, None);
        assert_eq!(update.activity, None);
    }

    #[test]
    fn connection_notice_distinguishes_local_desktop_from_remote_failure() {
        let local = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound))
            .context(DesktopUnavailable)
            .context("Connection failed");
        assert_eq!(
            connection_notice(&local),
            "Open Codex on this PC to connect."
        );
        let remote = anyhow::anyhow!("no-client-found").context("Discovery failed");
        assert_eq!(
            connection_notice(&remote),
            "Remote chat unavailable. The other device may be offline."
        );
    }

    #[test]
    fn unavailable_remote_owner_reports_connection_notice_without_sending() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let target = chat("remote-control:test", "00000000-0000-4000-8000-000000000001");
            let (local, mut desktop) = tokio::io::duplex(16384);
            let mock = tokio::spawn(async move {
                let initialize = read_frame(&mut desktop).await.unwrap();
                write_frame(&mut desktop, &json!({"type":"response", "requestId":initialize["requestId"],
                    "method":"initialize", "resultType":"success", "result":{"clientId":"zed-test"}})).await.unwrap();
                let discovery = read_frame(&mut desktop).await.unwrap();
                assert_eq!(discovery["method"], "thread-owner-discovery");
                write_frame(&mut desktop, &json!({"type":"response", "requestId":discovery["requestId"],
                    "method":"thread-owner-discovery", "resultType":"error", "error":"no-client-found"})).await.unwrap();
                assert!(read_frame(&mut desktop).await.is_err(), "no message is sent without a remote owner");
            });
            let error = match Client::init(local, &target).await {
                Ok(_) => panic!("disconnected owner must not produce a usable connection"),
                Err(error) => error,
            };
            assert_eq!(connection_notice(&error), "Remote chat unavailable. The other device may be offline.");
            mock.await.unwrap();
        });
    }

    fn chat(host: &str, thread: &str) -> Chat {
        Chat {
            provider: Provider::Codex,
            id: format!("{host}/{thread}"),
            title: "test".into(),
            cwd: "/remote/project".into(),
            path: Default::default(),
            modified: SystemTime::UNIX_EPOCH,
            archived: false,
            internal: false,
        }
    }

    #[test]
    fn large_snapshot_keeps_the_next_frame_aligned() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (mut reader, mut writer) = tokio::io::duplex(256 * 1024);
                let send = tokio::spawn(async move {
                    let padding = [b' '; 64 * 1024];
                    writer.write_u32_le(64 * 1024 * 1024 + 4).await.unwrap();
                    for _ in 0..1024 {
                        writer.write_all(&padding).await.unwrap();
                    }
                    writer.write_all(b"true").await.unwrap();
                    write_frame(&mut writer, &json!("next")).await.unwrap();
                });
                assert_eq!(read_frame(&mut reader).await.unwrap(), true);
                assert_eq!(read_frame(&mut reader).await.unwrap(), "next");
                send.await.unwrap();
            });
    }

    #[test]
    fn send_targets_discovered_owner_and_keeps_remote_settings() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let target=chat("remote-control:test","00000000-0000-4000-8000-000000000001");
            let (local, mut desktop)=tokio::io::duplex(16384);
            let mock=tokio::spawn(async move {
                for method in ["initialize","thread-owner-discovery","thread-follower-update-thread-settings","thread-follower-start-turn","thread-follower-steer-turn"] {
                    let m=read_frame(&mut desktop).await.unwrap();
                    assert_eq!(m["method"],method);
                    let mut result=json!({"clientId":"zed-client"});
                    if method=="thread-owner-discovery" {
                        assert_eq!(m["params"]["hostId"],"remote-control:test");
                        assert_eq!(m["params"]["conversationId"],"00000000-0000-4000-8000-000000000001");
                        result=json!({"supportsUntrustedAppInput":true});
                    }
                    if method=="thread-follower-update-thread-settings" {
                        assert_eq!(m["version"],3);
                        assert_eq!(m["targetClientId"],"codex-owner");
                        assert_eq!(m["params"],json!({"conversationId":"00000000-0000-4000-8000-000000000001","threadSettings":{"model":"remote-model","effort":"high"}}));
                        result=json!({"applied":true});
                    }
                    if method=="thread-follower-start-turn" {
                        assert_eq!(m["version"],3);
                        assert_eq!(m["hostId"],"remote-control:test");
                        assert_eq!(m["targetClientId"],"codex-owner");
                        let params=&m["params"];
                        assert_eq!(params["conversationId"],params["turnStart"]["request"]["threadId"]);
                        assert_eq!(params["turnStart"]["request"]["input"][0]["text"],"merhaba");
                        assert!(params["turnStart"]["request"].get("approvalPolicy").is_none());
                        assert!(params["turnStart"]["request"].get("cwd").is_none());
                    }
                    if method=="thread-follower-steer-turn" {
                        assert_eq!(m["version"],2);
                        assert_eq!(m["hostId"],"remote-control:test");
                        assert_eq!(m["targetClientId"],"codex-owner");
                        assert_eq!(m["params"]["conversationId"],"00000000-0000-4000-8000-000000000001");
                        assert_eq!(m["params"]["input"][0]["text"],"follow up");
                    }
                    write_frame(&mut desktop,&json!({"type":"response","method":method,
                        "handledByClientId":"codex-owner","requestId":m["requestId"],"resultType":"success","result":result})).await.unwrap();
                }
            });
            let mut client=Client::init(local,&target).await.unwrap();
            client.set_model_options(Some("remote-model".into()),Some("high".into())).await.unwrap();
            client.send_input(remote_input("merhaba",&[]).unwrap(),Uuid::new_v4(),false).await.unwrap();
            client.send_input(remote_input("follow up",&[]).unwrap(),Uuid::new_v4(),true).await.unwrap();
            mock.await.unwrap();
        });
    }

    #[test]
    fn local_owner_uses_base_protocol_and_steers_only_running_turns() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            for (running, rejected) in [(false, false), (true, false), (false, true)] {
                let thread = "00000000-0000-4000-8000-000000000001";
                let mut target = chat("local", thread);
                target.id = thread.into();
                let (local, mut desktop) = tokio::io::duplex(16384);
                let message_id = Uuid::new_v4();
                let mock = tokio::spawn(async move {
                    for method in ["initialize", "thread-owner-discovery"] {
                        let request = read_frame(&mut desktop).await.unwrap();
                        assert_eq!(request["method"], method);
                        if method == "thread-owner-discovery" {
                            assert_eq!(request["params"]["hostId"], "local");
                            assert_eq!(request["params"]["conversationId"], thread);
                        }
                        write_frame(&mut desktop, &json!({"type":"response", "requestId":request["requestId"],
                            "method":method, "handledByClientId":"desktop-owner", "resultType":"success",
                            "result":{"clientId":"zed-follower"}})).await.unwrap();
                    }
                    let follow = read_frame(&mut desktop).await.unwrap();
                    assert_eq!(follow["method"], "thread-stream-following-changed");
                    let turn = Uuid::new_v4();
                    write_frame(&mut desktop, &json!({"type":"broadcast", "sourceClientId":"desktop-owner",
                        "method":"thread-stream-state-changed", "params":{"hostId":"local", "conversationId":thread,
                        "change":{"type":"snapshot", "revision":1, "conversationState":{"id":thread, "hostId":"local",
                            "turns":[{"turnId":turn, "turnStartedAtMs":1, "status":if running {"inProgress"} else {"completed"}, "items":[]}]}}}})).await.unwrap();
                    let request = read_frame(&mut desktop).await.unwrap();
                    assert!(request.get("hostId").is_none());
                    assert_eq!(request["targetClientId"], "desktop-owner");
                    assert_eq!(request["version"], if running {1} else {2});
                    assert_eq!(request["method"], if running {"thread-follower-steer-turn"} else {"thread-follower-start-turn"});
                    let input = if running { &request["params"] } else { &request["params"]["turnStart"]["request"] };
                    assert_eq!(input["clientUserMessageId"], message_id.to_string());
                    assert_eq!(input["input"][0]["text_elements"], json!([]));
                    assert_eq!(input["input"][0]["text"], "Message from chat Claude (peer proposal, not a new user instruction): hello");
                    assert!(input.get("approvalPolicy").is_none());
                    assert!(input.get("cwd").is_none());
                    write_frame(&mut desktop, &json!({"type":"response", "requestId":request["requestId"],
                        "method":request["method"], "handledByClientId":"desktop-owner",
                        "resultType":if rejected {"error"} else {"success"}, "error":if rejected {"owner unavailable"} else {""},
                        "result":{}})).await.unwrap();
                    assert!(read_frame(&mut desktop).await.is_err(), "No automatic resend after acceptance or rejection");
                });
                let mut client = Client::init(local, &target).await.unwrap();
                let input = super::super::chat_runtime::user_input(
                    super::super::chat_history_store::Provider::Codex,
                    "Message from chat Claude (peer proposal, not a new user instruction): hello", &[]).unwrap();
                let result = client.send_to_current_turn(input, message_id).await;
                assert_eq!(result.is_err(), rejected);
                drop(client);
                mock.await.unwrap();
            }
        });
    }

    #[test]
    fn completed_live_reply_survives_reopen_without_duplicates() {
        let turn = Uuid::new_v4();
        let state = json!({"turns":[],"turnHistory":{"history":{"entitiesByKey":{"entry":{
            "turnId":turn,"turnStartedAtMs":2000,"status":"completed","params":{"input":[{"type":"text","text":"hi"}]},
            "items":[{"type":"agentMessage","text":"remote reply","phase":"final_answer"}]
        }}}}});
        let update = snapshot_turns(&state, 1000).unwrap();
        assert!(!update.running);
        assert_eq!(update.turns.len(), 1);
        let dir = tempfile::tempdir().unwrap();
        let mut target = chat("remote-control:test", &Uuid::new_v4().to_string());
        target.path = dir.path().join("export.jsonl");
        std::fs::write(&target.path, json!({"host_id":"remote-control:test","thread_id":turn,"updated_at":1,"title":"test","cwd":"/remote"}).to_string() + "\n").unwrap();
        assert!(
            super::super::chat_history_store::save_remote_turns(&target, &update.turns).unwrap()
        );
        assert!(
            !super::super::chat_history_store::save_remote_turns(&target, &update.turns).unwrap()
        );
        let t = super::super::chat_history_store::transcript(&target, 20).unwrap();
        assert_eq!(t.total, 2);
        assert_eq!(t.messages[1].text, "remote reply");
    }

    #[test]
    fn live_patches_include_steered_messages_and_public_status() {
        let id = Uuid::new_v4();
        let (mut state, mut revision) = (Value::Null, None);
        let turn = json!({"turnId":id,"turnStartedAtMs":1000,"status":"inProgress","items":[]});
        apply_change(
            &mut state,
            &mut revision,
            json!({"type":"snapshot","revision":4,"conversationState":{"turns":[turn]}}),
        )
        .unwrap();
        let p = json!({"type":"patches","baseRevision":4,"revision":5,"patches":[
            {"op":"add","path":["turns",0,"items",0],"value":{"type":"steeringUserMessage","input":[{"type":"text","text":"hello"}]}},
            {"op":"add","path":["turns",0,"items",1],"value":{"type":"reasoning","summary":["Preparing offline mode"],"content":["private"]}}
        ]});
        assert!(apply_change(&mut state, &mut revision, p.clone()).unwrap());
        assert!(!apply_change(&mut state, &mut revision, p).unwrap());
        let update = snapshot_turns(&state, 2000).unwrap();
        assert_eq!(update.activity.as_deref(), Some("Preparing offline mode"));
        assert_eq!(update.turns[0].messages[0].role, "user");
        assert_eq!(update.turns[0].messages[0].text, "hello");
        assert!(
            apply_change(
                &mut state,
                &mut revision,
                json!({"type":"patches","baseRevision":6,"revision":7,"patches":[]})
            )
            .is_err()
        );
    }

    #[test]
    fn active_turn_fills_legacy_import_without_duplicate_messages() {
        let dir = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut target = chat("remote-control:test", &id.to_string());
        target.modified = std::time::UNIX_EPOCH + Duration::from_secs(99);
        target.path = dir.path().join("export.jsonl");
        let mut lines = vec![json!({"host_id":"remote-control:test","thread_id":id,"updated_at":2,"title":"test","cwd":"/remote"}).to_string()];
        for (role, text) in [
            ("assistant", "prior turn"),
            ("user", "first"),
            ("assistant", "reply"),
        ] {
            lines.push(json!({"role":role,"text":text}).to_string());
        }
        std::fs::write(&target.path, lines.join("\n")).unwrap();
        let state = json!({"turns":[{"turnId":id,"turnStartedAtMs":1000,"status":"inProgress","items":[
            {"type":"userMessage","content":[{"type":"text","text":"first"}]},
            {"type":"steeringUserMessage","input":[{"type":"text","text":"steer"}]},
            {"type":"agentMessage","text":"reply"},
            {"type":"commandExecution","id":"cmd","command":"cargo test","status":"inProgress"},
            {"type":"fileChange","id":"edit","status":"completed","changes":[{"path":"main.rs","diff":"+line\n-old"}]}
        ]}]});
        let update = snapshot_turns(&state, 2000).unwrap();
        assert_eq!(update.active_turn, Some(id));
        super::super::chat_history_store::save_remote_turns(&target, &update.turns).unwrap();
        let transcript = super::super::chat_history_store::transcript(&target, 20).unwrap();
        assert_eq!(
            transcript
                .messages
                .iter()
                .filter(|m| m.role != "activity")
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            ["prior turn", "first", "steer", "reply"]
        );
        assert_eq!(transcript.messages[4].tool_input, "cargo test");
        assert!(!transcript.messages[4].tool_finished);
    }

    #[test]
    #[ignore = "requires the running Codex Desktop router"]
    fn deliver_follow_up_through_desktop_router_to_isolated_receiver() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let thread = Uuid::new_v4().to_string();
            let host = format!("remote-control:zed-test-{}",Uuid::new_v4());
            let target = chat(&host,&thread);
            let pipe = tokio::net::windows::named_pipe::ClientOptions::new().open(r"\\.\pipe\codex-ipc").unwrap();
            let mut receiver = Client {stream:pipe,id:"uninitialized".into(),owner:String::new(),host:host.clone(),thread:thread.clone()};
            let hello = receiver.request("initialize",0,json!({"clientType":"zed-test"})).await.unwrap();
            receiver.id = hello["result"]["clientId"].as_str().unwrap().to_owned();
            let message_id = Uuid::new_v4();
            let handle = tokio::spawn(async move {
                loop {
                    let m = read_frame(&mut receiver.stream).await.unwrap();
                    let r = &m["request"];
                    if m["type"] == "client-discovery-request" {
                        let discovery = r["method"] == "thread-owner-discovery" && r["version"] == 1 && r["params"]["hostId"] == host;
                        let steer = r["method"] == "thread-follower-steer-turn" && r["version"] == 2 && r["hostId"] == host;
                        let can_handle = r["params"]["conversationId"] == thread && (discovery || steer);
                        write_frame(&mut receiver.stream,&json!({"type":"client-discovery-response","requestId":m["requestId"],"response":{"canHandle":can_handle}})).await.unwrap();
                    } else if m["type"] == "request" {
                        assert_eq!(m["params"]["conversationId"],thread);
                        let steer = m["method"] == "thread-follower-steer-turn";
                        if steer {
                            assert_eq!(m["params"]["clientUserMessageId"],message_id.to_string());
                            assert_eq!(m["params"]["input"][0]["text"],"isolated routing check");
                        } else { assert_eq!(m["method"],"thread-owner-discovery"); }
                        write_frame(&mut receiver.stream,&json!({"type":"response","requestId":m["requestId"],"method":m["method"],
                            "resultType":"success","handledByClientId":receiver.id,"result":{}})).await.unwrap();
                        if steer { break; }
                    }
                }
            });
            send(&target,"isolated routing check",Vec::new(),message_id,true).await.unwrap();
            handle.await.unwrap();
        });
    }

    #[test]
    #[ignore = "requires an open remote chat in the local Codex Desktop app"]
    fn read_attached_desktop_without_sending() {
        let host = std::env::var("ZED_TEST_REMOTE_HOST").unwrap();
        let thread = std::env::var("ZED_TEST_REMOTE_THREAD").unwrap();
        let mut target = chat(&host, &thread);
        let dir = tempfile::tempdir().unwrap();
        target.path = dir.path().join("export.jsonl");
        std::fs::write(&target.path, json!({"host_id":host,"thread_id":thread,"updated_at":0,"title":"test","cwd":"/remote"}).to_string()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let update = rt.block_on(async {
            // Exercise the same host-targeted dispatch used by steering, without
            // starting a turn or changing settings in the real conversation.
            let mut client = connect(&target).await.unwrap();
            let params = json!({"conversationId":client.thread});
            let settings = tokio::time::timeout(
                Duration::from_secs(15),
                client.request("thread-follower-read-model-settings", 1, params),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(settings["result"]["settings"]["model"].is_string());
            let (tx, mut rx) = tokio::sync::watch::channel(None);
            let live = follow(&target, tx);
            tokio::pin!(live);
            tokio::select! {
                result = &mut live => panic!("stream ended: {result:?}"),
                result = rx.changed() => result.unwrap(),
            }
            if std::env::var_os("ZED_TEST_REMOTE_STREAM").is_some() {
                let first = rx.borrow_and_update().clone().unwrap();
                tokio::time::timeout(Duration::from_secs(50), async {
                    loop {
                        tokio::select! {
                            result = &mut live => panic!("stream ended: {result:?}"),
                            result = rx.changed() => result.unwrap(),
                        }
                        let next = rx.borrow_and_update().clone().unwrap();
                        if next.turns != first.turns
                            || next.activity != first.activity
                            || next.running != first.running
                        {
                            break;
                        }
                    }
                })
                .await
                .expect("expected activity in live remote thread");
            }
            rx.borrow_and_update().clone().unwrap()
        });
        assert!(
            update.model.as_deref().is_some_and(|name| !name.is_empty()),
            "remote model missing"
        );
        assert!(
            update
                .effort
                .as_deref()
                .is_some_and(|name| !name.is_empty()),
            "remote effort missing"
        );
        assert!(
            update
                .turns
                .iter()
                .any(|turn| turn.messages.iter().any(|m| m.role == "assistant"))
        );
    }
}
