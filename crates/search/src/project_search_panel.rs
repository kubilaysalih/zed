use crate::{SearchOption, SearchOptions};
use editor::{Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};
use file_icons::FileIcons;
use futures::StreamExt;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle, StyledText,
    Task, UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use language::Buffer;
use project::{Project, search::SearchResult};
use std::{ops::Range, time::Duration};
use text::{Anchor, Point};
use ui::{ScrollAxes, Scrollbars, SpinnerLabel, TintColor, Tooltip, WithScrollbar, prelude::*};
use util::{
    ResultExt,
    paths::{PathMatcher, PathStyle},
};
use workspace::{
    DeploySearch, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

pub fn install(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<ProjectSearchPanel>(cx).is_some() {
        return;
    }
    let project = workspace.project().clone();
    let workspace_handle = cx.weak_entity();
    let panel = cx.new(|cx| ProjectSearchPanel::new(project, workspace_handle, window, cx));
    workspace.add_panel(panel, window, cx);
}

pub fn deploy(
    workspace: &mut Workspace,
    action: &DeploySearch,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    install(workspace, window, cx);
    if let Some(panel) = workspace.focus_panel::<ProjectSearchPanel>(window, cx) {
        panel.update(cx, |panel, cx| {
            if let Some(query) = &action.query {
                panel
                    .query
                    .update(cx, |editor, cx| editor.set_text(query.clone(), window, cx));
            }
            if let Some(include) = &action.included_files {
                panel.include.update(cx, |editor, cx| {
                    editor.set_text(include.clone(), window, cx)
                });
            }
            if let Some(exclude) = &action.excluded_files {
                panel.exclude.update(cx, |editor, cx| {
                    editor.set_text(exclude.clone(), window, cx)
                });
            }
            for (option, enabled) in [
                (SearchOptions::REGEX, action.regex),
                (SearchOptions::CASE_SENSITIVE, action.case_sensitive),
                (SearchOptions::WHOLE_WORD, action.whole_word),
                (SearchOptions::INCLUDE_IGNORED, action.include_ignored),
            ] {
                if let Some(enabled) = enabled {
                    panel.options.set(option, enabled);
                }
            }
            panel.filters |= action.included_files.is_some() || action.excluded_files.is_some();
            panel.schedule(cx);
        });
    }
}

#[derive(Clone)]
struct ResultRow {
    buffer: Entity<Buffer>,
    range: Range<Anchor>,
    label: String,
    heading: bool,
    directory: String,
    match_count: usize,
    line: u32,
    highlight: Range<usize>,
}

pub struct ProjectSearchPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    query: Entity<Editor>,
    include: Entity<Editor>,
    exclude: Entity<Editor>,
    options: SearchOptions,
    filters: bool,
    rows: Vec<ResultRow>,
    scroll_handle: UniformListScrollHandle,
    count: usize,
    running: bool,
    limited: bool,
    error: Option<String>,
    task: Task<()>,
}

impl ProjectSearchPanel {
    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut input = |placeholder| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        let query = input("Search all files…");
        let include = input("e.g. src/**, tests/**");
        let exclude = input("e.g. **/vendor/**");
        for editor in [&query, &include, &exclude] {
            cx.subscribe(editor, |this, _, event, cx| {
                if matches!(event, EditorEvent::Edited { .. }) {
                    this.schedule(cx);
                }
            })
            .detach();
        }
        Self {
            workspace,
            project,
            query,
            include,
            exclude,
            options: SearchOptions::NONE,
            filters: false,
            rows: Vec::new(),
            scroll_handle: UniformListScrollHandle::new(),
            count: 0,
            running: false,
            limited: false,
            error: None,
            task: Task::ready(()),
        }
    }

    fn schedule(&mut self, cx: &mut Context<Self>) {
        self.task = Task::ready(());
        self.rows.clear();
        self.count = 0;
        self.error = None;
        self.limited = false;
        let text = self.query.read(cx).text(cx);
        if text.is_empty() {
            self.running = false;
            cx.notify();
            return;
        }
        let query = (|| {
            let patterns = |editor: &Entity<Editor>| {
                PathMatcher::new(
                    crate::project_search::split_glob_patterns(&editor.read(cx).text(cx))
                        .into_iter()
                        .map(str::trim)
                        .filter(|pattern| !pattern.is_empty()),
                    PathStyle::local(),
                )
            };
            self.options.build_query(
                text,
                patterns(&self.include)?,
                patterns(&self.exclude)?,
                self.project.read(cx).visible_worktrees(cx).count() > 1,
                None,
            )
        })();
        let query = match query {
            Ok(query) => query,
            Err(error) => {
                self.error = Some(error.to_string());
                self.running = false;
                cx.notify();
                return;
            }
        };
        self.running = true;
        self.task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let Some(search) = this
                .update(cx, |this, cx| {
                    this.project
                        .update(cx, |project, cx| project.search(query, cx))
                })
                .ok()
            else {
                return;
            };
            let mut chunks = std::pin::pin!(search.rx.clone().ready_chunks(128));
            while let Some(results) = chunks.next().await {
                if this
                    .update(cx, |this, cx| {
                        for result in results {
                            match result {
                                SearchResult::Buffer { buffer, ranges } => {
                                    let snapshot = buffer.read(cx).snapshot();
                                    if let Some(first) = ranges.first() {
                                        let label = snapshot
                                            .file()
                                            .map(|file| file.path().as_unix_str().to_owned())
                                            .unwrap_or_else(|| "Untitled".into());
                                        let (directory, filename) = label
                                            .rsplit_once('/')
                                            .map(|(directory, filename)| {
                                                (directory.to_owned(), filename.to_owned())
                                            })
                                            .unwrap_or_else(|| (String::new(), label));
                                        this.rows.push(ResultRow {
                                            buffer: buffer.clone(),
                                            range: first.clone(),
                                            label: filename,
                                            heading: true,
                                            directory,
                                            match_count: ranges.len(),
                                            line: 0,
                                            highlight: 0..0,
                                        });
                                    }
                                    for range in ranges {
                                        let point: Point =
                                            snapshot.summary_for_anchor(&range.start);
                                        let start = snapshot.clip_point(
                                            Point::new(point.row, point.column.saturating_sub(60)),
                                            text::Bias::Left,
                                        );
                                        let preview = snapshot
                                            .text_for_range(
                                                start
                                                    ..Point::new(
                                                        point.row,
                                                        snapshot.line_len(point.row),
                                                    ),
                                            )
                                            .take(240)
                                            .collect::<String>();
                                        let end: Point = snapshot.summary_for_anchor(&range.end);
                                        let trim = preview.len() - preview.trim_start().len();
                                        let label = preview.trim().to_owned();
                                        let highlight_start =
                                            (point.column - start.column) as usize;
                                        let highlight_end = if end.row == point.row {
                                            (end.column - start.column) as usize
                                        } else {
                                            preview.len()
                                        };
                                        let highlight = highlight_start
                                            .saturating_sub(trim)
                                            .min(label.len())
                                            ..highlight_end.saturating_sub(trim).min(label.len());
                                        this.rows.push(ResultRow {
                                            buffer: buffer.clone(),
                                            range,
                                            label,
                                            heading: false,
                                            directory: String::new(),
                                            match_count: 0,
                                            line: point.row + 1,
                                            highlight,
                                        });
                                        this.count += 1;
                                    }
                                }
                                SearchResult::LimitReached => this.limited = true,
                                SearchResult::Searching | SearchResult::WaitingForScan => {}
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.running = false;
                cx.notify();
            })
            .log_err();
            drop(search);
        });
        cx.notify();
    }

    fn open_result(&self, row: ResultRow, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                let editor = workspace.open_project_item::<Editor>(
                    None,
                    row.buffer.clone(),
                    true,
                    true,
                    false,
                    true,
                    window,
                    cx,
                );
                let snapshot = row.buffer.read(cx).snapshot();
                let start: Point = snapshot.summary_for_anchor(&row.range.start);
                let end: Point = snapshot.summary_for_anchor(&row.range.end);
                editor.update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([start..end]),
                    );
                });
            })
            .log_err();
    }
}

impl EventEmitter<PanelEvent> for ProjectSearchPanel {}
impl Focusable for ProjectSearchPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}
impl Panel for ProjectSearchPanel {
    fn persistent_name() -> &'static str {
        "Project Search Panel"
    }
    fn panel_key() -> &'static str {
        "ProjectSearchPanel"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Left
    }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }
    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(340.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::MagnifyingGlass)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Project Search")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(DeploySearch::default())
    }
    fn activation_priority(&self) -> u32 {
        8
    }
}

impl Render for ProjectSearchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .overflow_hidden()
            .track_focus(&self.query.focus_handle(cx))
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .p_3()
                    .gap_1()
                    .child(Label::new("Search"))
                    .child(div().flex_1())
                    .children(
                        [
                            SearchOption::CaseSensitive,
                            SearchOption::WholeWord,
                            SearchOption::Regex,
                            SearchOption::IncludeIgnored,
                        ]
                        .into_iter()
                        .map(|option| {
                            IconButton::new(option.label(), option.icon())
                                .toggle_state(self.options.contains(option.as_options()))
                                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                .tooltip(Tooltip::text(option.label()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.options.toggle(option.as_options());
                                    this.schedule(cx);
                                }))
                        }),
                    )
                    .child(
                        IconButton::new("search-filters", IconName::Filter)
                            .toggle_state(self.filters)
                            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                            .tooltip(Tooltip::text("Include / exclude files"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.filters = !this.filters;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .mx_3()
                    .p_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .child(self.query.clone()),
            )
            .when(self.filters, |this| {
                this.child(
                    v_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .children([
                            ("Files to include", self.include.clone()),
                            ("Files to exclude", self.exclude.clone()),
                        ].into_iter().map(|(label, editor)| {
                            v_flex().gap_1()
                                .child(Label::new(label).size(LabelSize::Small))
                                .child(div().p_2().border_1()
                                    .border_color(cx.theme().colors().border)
                                    .rounded_sm()
                                    .bg(cx.theme().colors().editor_background)
                                    .child(editor))
                        })),
                )
            })
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .when(self.running, |this| this.child(SpinnerLabel::new()))
                    .child(
                        Label::new(if self.query.read(cx).text(cx).is_empty() {
                            "Search all files".into()
                        } else if self.count == 0 && !self.running {
                            "No results".into()
                        } else {
                            format!(
                                "{} matches{}",
                                self.count,
                                if self.limited {
                                    " · limit reached"
                                } else {
                                    ""
                                }
                            )
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().px_3().child(Label::new(error).color(Color::Error)))
            })
            .child(
                div().id("search-results-viewport").flex_1().min_h_0().relative().overflow_hidden().child(
                uniform_list(
                    "project-search-results",
                    self.rows.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                this.rows.get(index).cloned().map(|row| {
                                    let heading = row.heading;
                                    let icon = row.buffer.read(cx).file()
                                        .and_then(|file| FileIcons::get_icon(file.path().as_std_path(), cx))
                                        .map(Icon::from_path)
                                        .unwrap_or_else(|| Icon::new(IconName::File));
                                    h_flex()
                                        .id(index)
                                        .w_full()
                                        .h(px(26.))
                                        .gap_1()
                                        .px_3()
                                        .when(!heading, |this| this.pl_6())
                                        .cursor_pointer()
                                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                                        .when(heading, |this| {
                                            this.child(
                                                icon.size(IconSize::Small),
                                            )
                                        })
                                        .when(heading, |this| this.child(
                                            h_flex().flex_1().min_w_0().gap_2().overflow_hidden().child(
                                            Label::new(row.label.clone())
                                                .size(LabelSize::Small)
                                                .single_line()
                                                .truncate())
                                            .child(Label::new(row.directory.clone()).size(LabelSize::Small).color(Color::Muted).truncate())
                                        ).child(div().px_1p5().rounded_full().bg(cx.theme().colors().element_background)
                                            .child(Label::new(row.match_count.to_string()).size(LabelSize::XSmall))))
                                        .when(!heading, |this| this
                                            .child(div().w(px(32.)).flex_shrink_0().child(Label::new(row.line.to_string()).size(LabelSize::Small).color(Color::Muted)))
                                            .child(div().flex_1().min_w_0().text_size(rems(0.875)).text_ellipsis().child(
                                                StyledText::new(row.label.clone()).with_highlights([(
                                                    row.highlight.clone(), HighlightStyle {
                                                        background_color: Some(cx.theme().colors().search_match_background),
                                                        font_weight: Some(gpui::FontWeight::BOLD),
                                                        ..Default::default()
                                                    }
                                                )])
                                            )))
                                        .tooltip(Tooltip::text(row.label.clone()))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_result(row.clone(), window, cx)
                                        }))
                                })
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_handle)
                .size_full())
                .custom_scrollbars(Scrollbars::always_visible(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.scroll_handle)
                    .with_track_along(ScrollAxes::Vertical, cx.theme().colors().panel_background), window, cx),
            )
    }
}
