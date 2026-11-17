use super::{ChatHistoryView, Provider};
use agent_ui::local_editor_agent::{LocalEditorAgent, filter_chat_targets};
use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{Context, Entity, Task, WeakEntity, Window};
use language::{Anchor, Buffer, CodeLabel, ToOffset};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
use workspace::MultiWorkspace;
use gpui::IntoElement;
use std::{ops::Range, sync::Arc};

pub(super) struct ComposerCompletions(pub(super) WeakEntity<ChatHistoryView>);

// Offsets are bytes, matching the editor buffer's anchors.
fn query(prefix: &str) -> Option<(usize, char, &str)> {
    if let Some(command) = prefix.strip_prefix('/')
        && !command.contains(['\n', '\r'])
        && !command.ends_with(char::is_whitespace)
    {
        return Some((0, '/', command));
    }
    let start = prefix.rfind('@')?;
    let needle = &prefix[start + 1..];
    if prefix[..start]
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace())
        || needle.contains(['\n', '\r', '[', ']'])
    {
        return None;
    }
    Some((start, '@', needle))
}

pub(super) fn has_query_at_cursor(editor: &Editor, cx: &gpui::App) -> bool {
    let buffer = editor.buffer().read(cx).snapshot(cx);
    let selection = editor.selections.newest_anchor();
    let end = editor::ToOffset::to_offset(&selection.head(), &buffer);
    if editor::ToOffset::to_offset(&selection.tail(), &buffer) != end {
        return false;
    }
    let prefix = buffer.text_for_range(editor::MultiBufferOffset(0)..end).collect::<String>();
    query(&prefix).is_some()
}

pub(super) struct ChatMention<'a> {
    range: Range<usize>,
    pub(super) label: &'a str,
    pub(super) key: &'a str,
}

pub(super) fn chat_mentions(text: &str) -> impl Iterator<Item = ChatMention<'_>> {
    const MARKER: &str = " [chat_id: ";
    let mut previous_end = 0;
    text.match_indices(MARKER).filter_map(move |(marker, _)| {
        let id_start = marker + MARKER.len();
        let (id, _) = text[id_start..].split_once(']')?;
        let key = id.strip_prefix("Codex:").or_else(|| id.strip_prefix("Claude:"))?;
        if key.is_empty() || key.contains(char::is_whitespace) || previous_end > marker {
            return None;
        }
        let before = &text[previous_end..marker];
        let line = before.rsplit(['\n', '\r']).next()?;
        let (at, _) = line.rmatch_indices('@').find(|(at, _)| {
            line[..*at].chars().next_back().is_none_or(char::is_whitespace)
        })?;
        let start = marker - line.len() + at;
        previous_end = id_start + id.len() + 1;
        Some(ChatMention { range: start..previous_end, label: &text[start..marker], key: id })
    })
}

pub(super) fn message_without_mentions(text: &str) -> std::borrow::Cow<'_, str> {
    let mut mentions = chat_mentions(text).peekable();
    if mentions.peek().is_none() {
        return text.into();
    }
    let mut visible = String::new();
    let mut offset = 0;
    for mention in mentions {
        visible.push_str(&text[offset..mention.range.start]);
        offset = mention.range.end;
    }
    visible.push_str(&text[offset..]);
    visible.into()
}

pub(super) fn fold_mentions(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    use editor::{MultiBufferOffset, ToOffset as _};
    let text = editor.text(cx);
    struct MentionChip;
    let kind = std::any::TypeId::of::<MentionChip>();
    let display = editor.display_snapshot(cx);
    let buffer = display.buffer_snapshot();
    let all = MultiBufferOffset(0)..buffer.len();
    let mut existing = display.folds_in_range(all.clone())
        .filter(|fold| fold.placeholder.type_tag == Some(kind))
        .peekable();
    let mentions = chat_mentions(&text).collect::<Vec<_>>();
    let had_chips = existing.peek().is_some();
    if existing.map(|fold| {
        fold.range.start.to_offset(buffer).0..fold.range.end.to_offset(buffer).0
    }).eq(mentions.iter().map(|mention| mention.range.clone())) {
        return;
    }
    if had_chips {
        editor.remove_folds_with_type(&[all], kind, false, cx);
    }
    let mut creases = Vec::new();
    for mention in mentions {
        let range = MultiBufferOffset(mention.range.start)..MultiBufferOffset(mention.range.end);
        let title: gpui::SharedString = mention.label.to_owned().into();
        let label: gpui::SharedString = util::truncate_and_trailoff(&title, 36).into();
        let placeholder = editor::FoldPlaceholder {
            render: Arc::new(move |id, _, _| {
                agent_ui::MentionCrease::new(id, ui::IconName::Chat.path(), label.clone())
                    .tooltip(title.clone())
                    .into_any_element()
            }),
            type_tag: Some(kind),
            constrain_width: false,
            merge_adjacent: false,
            ..Default::default()
        };
        creases.push(editor::display_map::Crease::simple(range, placeholder));
    }
    editor.fold_creases(creases, false, window, cx);
}

fn completion(range: std::ops::Range<Anchor>, label: String, insertion: String) -> Completion {
    Completion {
        replace_range: range,
        new_text: insertion,
        label: CodeLabel::plain(label, None),
        documentation: None,
        source: CompletionSource::Custom,
        icon_path: None,
        icon_color: None,
        match_start: None,
        snippet_deduplication_key: None,
        insert_text_mode: None,
        confirm: None,
        group: None,
    }
}

impl CompletionProvider for ComposerCompletions {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        _: CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let prefix: String = buffer
            .text_for_range(0..position.to_offset(buffer))
            .collect();
        let Some((start, kind, needle)) = query(&prefix) else {
            return Task::ready(Ok(vec![]));
        };
        let Some(view) = self.0.upgrade() else {
            return Task::ready(Ok(vec![]));
        };
        let view = view.read(cx);
        let range = buffer.anchor_before(start)..position;
        let response = |completions| {
            vec![CompletionResponse {
                completions,
                is_incomplete: true,
                display_options: CompletionDisplayOptions {
                    dynamic_width: true,
                },
            }]
        };
        if kind == '/' {
            let mut commands = vec!["/model", "/effort"];
            if !view.chat.is_remote_codex() {
                if view.chat.provider == Provider::Codex {
                    commands.extend([
                        "/goal",
                        "/goal status",
                        "/goal pause",
                        "/goal resume",
                        "/goal clear",
                    ]);
                } else {
                    commands.extend(view.slash_commands.iter().map(String::as_str));
                }
            }
            commands.sort_unstable();
            commands.dedup();
            let needle = needle.to_lowercase();
            let rows = commands
                .into_iter()
                .filter(|cmd| cmd[1..].starts_with(&needle))
                .map(|cmd| completion(range.clone(), cmd.into(), format!("{cmd} ")))
                .collect();
            return Task::ready(Ok(response(rows)));
        }
        if view.options.bridge.is_none() || !cx.has_global::<LocalEditorAgent>() {
            return Task::ready(Ok(vec![]));
        }
        let Some(multi) = window.root::<MultiWorkspace>().flatten() else {
            return Task::ready(Ok(vec![]));
        };
        let workspace = multi.read(cx).workspace().downgrade();
        let current = view.chat.key();
        let list = cx.global::<LocalEditorAgent>().list_chats.clone();
        let task = list(workspace, window, cx);
        let needle = needle.to_owned();
        cx.spawn(async move |_, _| {
            let mut targets = task.await?;
            targets.retain(|target| target.key != current);
            let rows = filter_chat_targets(targets, &needle)
                .into_iter()
                .map(|target| {
                    let label = format!(
                        "{} · {:?} · {}",
                        target.title, target.provider, target.project
                    );
                    let insertion = format!("@{} [chat_id: {}] ", target.title, target.key);
                    completion(range.clone(), label, insertion)
                })
                .collect();
            Ok(response(rows))
        })
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        _: &str,
        _: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        let buffer = buffer.read(cx);
        let prefix: String = buffer
            .text_for_range(0..position.to_offset(buffer))
            .collect();
        query(&prefix).is_some()
    }

    fn filter_completions(&self) -> bool {
        false
    }
    fn sort_completions(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::query;

    #[test]
    fn sent_mentions_preserve_targets_and_surrounding_text() {
        let text = "Şuna sor @Önceki sohbet [chat_id: Claude:peer-1] ve @Migration [chat_id: Codex:peer-2]\ncevabı karşılaştır.";
        let mentions = super::chat_mentions(text).collect::<Vec<_>>();
        assert_eq!(mentions.iter().map(|mention| (mention.label, mention.key)).collect::<Vec<_>>(),
            vec![("@Önceki sohbet", "Claude:peer-1"), ("@Migration", "Codex:peer-2")]);
        assert_eq!(super::message_without_mentions(text), "Şuna sor  ve \ncevabı karşılaştır.");
        for literal in ["name@example.com", "@unknown", "@broken [chat_id: Other:id]", "@broken [chat_id: Claude:]"] {
            assert_eq!(super::message_without_mentions(literal), literal);
        }
    }

    #[test]
    fn composer_triggers_respect_cursor_boundaries() {
        assert_eq!(
            query("şuna sor @Önceki sohbet"),
            Some((10, '@', "Önceki sohbet"))
        );
        assert_eq!(query("@"), Some((0, '@', "")));
        assert_eq!(query("/"), Some((0, '/', "")));
        assert_eq!(query("/goal p"), Some((0, '/', "goal p")));
        for literal in [
            "name@example.com",
            "a/b",
            "/model ",
            "önce\n/usage",
            "@foo [chat_id: Codex:1] ",
        ] {
            assert!(query(literal).is_none(), "{literal}");
        }
    }
}
