use anyhow::Result;
use gpui::{App, Global, Task, WeakEntity, Window};
use std::{
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
};
use workspace::Workspace;

pub use crate::context::LoadedContext as InlineContext;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalEditorProvider {
    #[default]
    Codex,
    Claude,
}

#[derive(Clone, Debug)]
pub struct LocalChatTarget {
    pub key: String,
    pub title: String,
    pub project: String,
    pub provider: LocalEditorProvider,
}

pub fn filter_chat_targets(chats: Vec<LocalChatTarget>, query: &str) -> Vec<LocalChatTarget> {
    let query = query.to_lowercase();
    let words = query.split_whitespace().collect::<Vec<_>>();
    let mut chats = chats
        .into_iter()
        .filter(|chat| {
            let text =
                format!("{} {} {:?}", chat.title, chat.project, chat.provider).to_lowercase();
            words.iter().all(|word| text.contains(word))
        })
        .collect::<Vec<_>>();
    // Keep recent conversations first among equally relevant matches.
    chats.sort_by_key(|chat| !chat.title.to_lowercase().starts_with(query.trim()));
    chats.truncate(50);
    chats
}

pub struct InlineRequest {
    pub target_chat: Option<LocalChatTarget>,
    pub session_id: uuid::Uuid,
    pub provider: LocalEditorProvider,
    pub prompt: String,
    pub document: String,
    pub document_path: Option<std::path::PathBuf>,
    pub selected_text: String,
    pub selection_start: usize,
    pub selection_end: usize,
    pub edit: bool,
    pub context: Task<Option<InlineContext>>,
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Debug, serde::Deserialize)]
pub struct InlineResponse {
    pub explanation: String,
    pub replacement_text: Option<String>,
}

/// The editor owns the preview; the application owns persistent chat sessions.
#[derive(Clone)]
pub struct LocalEditorAgent {
    pub list_chats: Rc<
        dyn Fn(WeakEntity<Workspace>, &mut Window, &mut App) -> Task<Result<Vec<LocalChatTarget>>>,
    >,
    pub run: Rc<
        dyn Fn(
            InlineRequest,
            WeakEntity<Workspace>,
            &mut Window,
            &mut App,
        ) -> Task<Result<InlineResponse>>,
    >,
    pub add_selection: Rc<dyn Fn(String, WeakEntity<Workspace>, &mut Window, &mut App)>,
}

impl Global for LocalEditorAgent {}

pub fn enabled(cx: &App) -> bool {
    use settings::Settings;
    agent_settings::AgentSettings::get_global(cx).use_local_chats_for_editor
        && cx.has_global::<LocalEditorAgent>()
}

const PREFERRED_PROVIDER_KEY: &str = "local_inline_preferred_provider";

struct PreferredProvider(LocalEditorProvider);
impl Global for PreferredProvider {}

pub fn preferred_provider(cx: &mut App) -> LocalEditorProvider {
    use util::ResultExt;
    if !cx.has_global::<PreferredProvider>() {
        let saved = db::kvp::KeyValueStore::global(cx)
            .read_kvp(PREFERRED_PROVIDER_KEY)
            .log_err()
            .flatten();
        let provider = match saved.as_deref() {
            Some("claude") => LocalEditorProvider::Claude,
            _ => LocalEditorProvider::Codex,
        };
        cx.set_global(PreferredProvider(provider));
    }
    cx.global::<PreferredProvider>().0
}

pub fn remember_provider(provider: LocalEditorProvider, cx: &mut App) {
    cx.set_global(PreferredProvider(provider));
    let value = match provider {
        LocalEditorProvider::Codex => "codex",
        LocalEditorProvider::Claude => "claude",
    };
    let database = db::kvp::KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        database
            .write_kvp(PREFERRED_PROVIDER_KEY.into(), value.into())
            .await
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(
        key: &str,
        title: &str,
        project: &str,
        provider: LocalEditorProvider,
    ) -> LocalChatTarget {
        LocalChatTarget {
            key: key.into(),
            title: title.into(),
            project: project.into(),
            provider,
        }
    }

    #[test]
    fn filters_chat_title_project_and_provider_together() {
        let chats = vec![
            chat("1", "AWS maliyet", "sevk", LocalEditorProvider::Claude),
            chat("2", "AWS maliyet", "leartes", LocalEditorProvider::Claude),
            chat("3", "AWS maliyet", "sevk", LocalEditorProvider::Codex),
        ];
        let matches = filter_chat_targets(chats, " SEVK aws CLAUDE ");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].key, "1");
    }

    #[test]
    fn prefix_matches_come_first_without_shuffling_recent_ties() {
        let chats = vec![
            chat("1", "Review sevk", "sevk", LocalEditorProvider::Codex),
            chat("2", "Sevk AWS", "sevk", LocalEditorProvider::Claude),
            chat("3", "Sevk docs", "sevk", LocalEditorProvider::Codex),
        ];
        let matches = filter_chat_targets(chats.clone(), "sevk");
        assert_eq!(
            matches
                .iter()
                .map(|chat| chat.key.as_str())
                .collect::<Vec<_>>(),
            ["2", "3", "1"]
        );
        assert_eq!(filter_chat_targets(chats, "missing").len(), 0);
    }
}
