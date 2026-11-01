use super::{
    ProjectSettingsUpdateEntry, ProjectSettingsUpdateQueue, SettingsUiFile, SettingsWindow,
    all_projects,
};
use editor::{Editor, EditorEvent};
use gpui::UpdateGlobal;
use gpui::{
    Bounds, Entity, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, ReadGlobal, Rgba,
    Subscription, TaskExt, canvas, linear_color_stop, linear_gradient,
};
use settings::{LocalSettingsKind, LocalSettingsPath, SettingsStore};
use std::{cell::Cell, rc::Rc};
use ui::prelude::*;
use util::ResultExt;

#[derive(Clone, Copy, Debug)]
struct Hsv {
    hue: f32,
    saturation: f32,
    value: f32,
}

impl Hsv {
    fn from_hex(text: &str) -> Option<Self> {
        let text = text.trim().strip_prefix('#').unwrap_or(text.trim());
        if text.len() != 6 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let rgb = gpui::rgb(u32::from_str_radix(text, 16).ok()?);
        let maximum = rgb.r.max(rgb.g).max(rgb.b);
        let minimum = rgb.r.min(rgb.g).min(rgb.b);
        let delta = maximum - minimum;
        let hue = if delta == 0.0 {
            0.0
        } else if maximum == rgb.r {
            ((rgb.g - rgb.b) / delta).rem_euclid(6.0) / 6.0
        } else if maximum == rgb.g {
            ((rgb.b - rgb.r) / delta + 2.0) / 6.0
        } else {
            ((rgb.r - rgb.g) / delta + 4.0) / 6.0
        };
        Some(Self {
            hue,
            saturation: if maximum == 0.0 { 0.0 } else { delta / maximum },
            value: maximum,
        })
    }
    fn rgb(self) -> Rgba {
        let chroma = self.value * self.saturation;
        let hue = self.hue.rem_euclid(1.0) * 6.0;
        let secondary = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
        let (red, green, blue) = match hue as u32 {
            0 => (chroma, secondary, 0.0),
            1 => (secondary, chroma, 0.0),
            2 => (0.0, chroma, secondary),
            3 => (0.0, secondary, chroma),
            4 => (secondary, 0.0, chroma),
            _ => (chroma, 0.0, secondary),
        };
        let offset = self.value - chroma;
        Rgba {
            r: red + offset,
            g: green + offset,
            b: blue + offset,
            a: 1.0,
        }
    }
    fn hex(self) -> String {
        let rgb = self.rgb();
        format!(
            "#{:02X}{:02X}{:02X}",
            (rgb.r * 255.0).round() as u8,
            (rgb.g * 255.0).round() as u8,
            (rgb.b * 255.0).round() as u8
        )
    }
}

fn stored_color(file: &SettingsUiFile, cx: &App) -> Option<String> {
    SettingsStore::global(cx)
        .get_content_for_file(file.to_settings())
        .and_then(|content| content.project.folder_color.clone())
}

fn effective_color(file: &SettingsUiFile, cx: &App) -> String {
    SettingsStore::global(cx)
        .get_value_from_file(file.to_settings(), |content| {
            content.project.folder_color.as_ref()
        })
        .1
        .cloned()
        .unwrap_or_default()
}

fn preview(file: &SettingsUiFile, color: Option<String>, cx: &mut App) -> anyhow::Result<()> {
    SettingsStore::update_global(cx, |store, cx| -> anyhow::Result<()> {
        match file {
            SettingsUiFile::Project((worktree, path)) => {
                // Preserve only settings already allowed by the project trust filter.
                let mut content = store
                    .get_content_for_file(file.to_settings())
                    .cloned()
                    .unwrap_or_default();
                content.project.folder_color = color;
                let json = serde_json::to_string(&content.project)?;
                store.set_local_settings(
                    *worktree,
                    LocalSettingsPath::InWorktree(path.clone()),
                    LocalSettingsKind::Settings,
                    Some(&json),
                    cx,
                )?;
            }
            SettingsUiFile::User => {
                let mut content = store.raw_user_settings().cloned().unwrap_or_default();
                content.content.project.folder_color = color;
                let json = serde_json::to_string(&content)?;
                store.set_user_settings(&json, cx).result()?;
            }
            SettingsUiFile::Server(_) => {
                anyhow::bail!("Folder colors are not available for server settings")
            }
        }
        Ok(())
    })?;
    cx.refresh_windows();
    Ok(())
}

#[derive(Clone, Copy)]
enum Drag {
    Color,
    Hue,
}

struct FolderColorPicker {
    file: SettingsUiFile,
    original: Option<String>,
    draft: Option<String>,
    hsv: Hsv,
    editor: Entity<Editor>,
    editor_text: String,
    error: Option<String>,
    saving: bool,
    drag: Option<Drag>,
    color_bounds: Rc<Cell<Bounds<Pixels>>>,
    hue_bounds: Rc<Cell<Bounds<Pixels>>>,
    _editor_subscription: Subscription,
}

pub(super) fn render(file: SettingsUiFile, window: &mut Window, cx: &mut App) -> AnyElement {
    let key = format!("folder-color-picker-{file:?}");
    window
        .use_keyed_state(key, cx, |window, cx| {
            FolderColorPicker::new(file, window, cx)
        })
        .into_any_element()
}

impl FolderColorPicker {
    fn new(file: SettingsUiFile, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let original = stored_color(&file, cx);
        let effective = SettingsStore::global(cx)
            .get_value_from_file(file.to_settings(), |content| {
                content.project.folder_color.as_ref()
            })
            .1
            .cloned()
            .unwrap_or_default();
        let hsv = Hsv::from_hex(&effective).unwrap_or(Hsv {
            hue: 0.06,
            saturation: 0.8,
            value: 1.0,
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(effective.clone(), window, cx);
            editor.set_placeholder_text("#FF6600", window, cx);
            editor
        });
        let subscription = cx.subscribe_in(&editor, window, |this, editor, event, _, cx| {
            if !matches!(event, EditorEvent::Edited { .. }) {
                return;
            }
            let text = editor.read(cx).text(cx);
            // Programmatic updates must preserve the hue when dragging through black or white.
            if this.editor_text == text {
                this.error = None;
                cx.notify();
                return;
            }
            let color = if text.trim().is_empty() {
                Some(String::new())
            } else if let Some(hsv) = Hsv::from_hex(&text) {
                this.hsv = hsv;
                Some(hsv.hex())
            } else {
                this.error = Some("Use six HEX digits, for example #FF6600.".into());
                cx.notify();
                return;
            };
            this.editor_text = text;
            this.set_preview(color, cx);
        });
        cx.on_release(|this, cx| {
            if !this.saving {
                this.restore(cx);
            }
        })
        .detach();
        Self {
            file,
            draft: original.clone(),
            original,
            hsv,
            editor,
            editor_text: effective,
            error: None,
            saving: false,
            drag: None,
            color_bounds: Rc::new(Cell::new(Bounds::default())),
            hue_bounds: Rc::new(Cell::new(Bounds::default())),
            _editor_subscription: subscription,
        }
    }
    fn set_preview(&mut self, color: Option<String>, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.error = None;
        if self.draft != color {
            match preview(&self.file, color.clone(), cx) {
                Ok(()) => self.draft = color,
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        cx.notify();
    }
    fn restore(&mut self, cx: &mut App) {
        // Do not overwrite a newer external edit when this preview is dismissed.
        if self.draft != self.original && stored_color(&self.file, cx) == self.draft {
            preview(&self.file, self.original.clone(), cx).log_err();
        }
        self.draft = stored_color(&self.file, cx);
        self.original = self.draft.clone();
    }
    fn sync_editor(&mut self, window: &mut Window, cx: &mut App) {
        let text = self
            .draft
            .clone()
            .unwrap_or_else(|| effective_color(&self.file, cx));
        self.editor_text = text.clone();
        self.editor
            .update(cx, |editor, cx| editor.set_text(text, window, cx));
    }
    fn drag_to(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(drag) = self.drag else {
            return;
        };
        let bounds = match drag {
            Drag::Color => self.color_bounds.get(),
            Drag::Hue => self.hue_bounds.get(),
        };
        if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
            return;
        }
        let horizontal = ((position.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
        match drag {
            Drag::Color => {
                self.hsv.saturation = horizontal;
                self.hsv.value =
                    1.0 - ((position.y - bounds.origin.y) / bounds.size.height).clamp(0.0, 1.0);
            }
            Drag::Hue => self.hsv.hue = horizontal,
        }
        self.set_preview(Some(self.hsv.hex()), cx);
        self.sync_editor(window, cx);
    }
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.error.is_some() || self.saving {
            return;
        }
        let color = self.draft.clone();
        let completion = match persist(&self.file, color.clone(), window, cx) {
            Ok(completion) => completion,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        self.saving = true;
        self.editor
            .update(cx, |editor, _| editor.set_read_only(true));
        let file = self.file.clone();
        let original = self.original.clone();
        cx.spawn(async move |picker, cx| {
            let result = completion.await.unwrap_or_else(|error| Err(error.into()));
            if let Some(picker) = picker.upgrade() {
                picker.update(cx, |picker, cx| {
                    picker.saving = false;
                    picker
                        .editor
                        .update(cx, |editor, _| editor.set_read_only(false));
                    match result {
                        Ok(()) => picker.original = color,
                        Err(error) => picker.error = Some(format!("Could not save: {error}")),
                    }
                    cx.notify();
                });
            } else if let Err(error) = result {
                log::error!("Could not save folder color: {error:#}");
                cx.update(|cx| {
                    if stored_color(&file, cx) == color {
                        preview(&file, original, cx).log_err();
                    }
                });
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        cx.notify();
    }
}

fn persist(
    file: &SettingsUiFile,
    color: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<futures::channel::oneshot::Receiver<anyhow::Result<()>>> {
    match file {
        SettingsUiFile::User => Ok(SettingsStore::global(cx)
            .update_settings_file_with_completion(<dyn fs::Fs>::global(cx), move |content, _| {
                content.project.folder_color = color
            })),
        SettingsUiFile::Project((worktree_id, path)) => {
            let settings_window = window
                .root::<SettingsWindow>()
                .flatten()
                .ok_or_else(|| anyhow::anyhow!("No settings window found"))?;
            let (worktree, project) =
                all_projects(settings_window.read(cx).original_window.as_ref(), cx)
                    .find_map(|project| {
                        project
                            .read(cx)
                            .worktree_for_id(*worktree_id, cx)
                            .zip(Some(project))
                    })
                    .ok_or_else(|| anyhow::anyhow!("The project is no longer open"))?;
            Ok(ProjectSettingsUpdateQueue::enqueue_with_completion(
                cx,
                ProjectSettingsUpdateEntry {
                    worktree_id: *worktree_id,
                    rel_path: path.join(paths::local_settings_file_relative_path()).into(),
                    settings_window: settings_window.downgrade(),
                    project: project.downgrade(),
                    worktree: worktree.downgrade(),
                    update: Box::new(move |content, _| content.project.folder_color = color),
                },
            ))
        }
        SettingsUiFile::Server(_) => {
            anyhow::bail!("Folder colors are not available for server settings")
        }
    }
}

impl Render for FolderColorPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.saving
            && self.draft == self.original
            && stored_color(&self.file, cx) != self.original
        {
            self.original = stored_color(&self.file, cx);
            self.draft = self.original.clone();
            if let Some(hsv) = Hsv::from_hex(&effective_color(&self.file, cx)) {
                self.hsv = hsv;
            }
            self.sync_editor(window, cx);
        }
        let hue = Hsv {
            saturation: 1.0,
            value: 1.0,
            ..self.hsv
        }
        .rgb();
        let color_bounds = self.color_bounds.clone();
        let swatch_color = if effective_color(&self.file, cx).is_empty() {
            cx.theme().colors().title_bar_background
        } else {
            self.hsv.rgb().into()
        };
        let hue_bounds = self.hue_bounds.clone();
        let drag_listener = cx.listener(|this, event: &MouseMoveEvent, window, cx| {
            if this.drag.is_some() && event.pressed_button == Some(MouseButton::Left) {
                this.drag_to(event.position, window, cx);
                cx.stop_propagation();
            }
        });
        let release_listener = cx.listener(|this, _: &MouseUpEvent, _, _| {
            this.drag = None;
        });
        v_flex()
            .w(px(256.0))
            .flex_none()
            .gap_2()
            .child(
                div()
                    .id("color-plane")
                    .relative()
                    .w_full()
                    .h(px(144.0))
                    .cursor(gpui::CursorStyle::Crosshair)
                    .bg(linear_gradient(
                        90.0,
                        linear_color_stop(gpui::white(), 0.0),
                        linear_color_stop(hue, 1.0),
                    ))
                    .child(div().absolute().inset_0().bg(linear_gradient(
                        180.0,
                        linear_color_stop(gpui::transparent_black(), 0.0),
                        linear_color_stop(gpui::black(), 1.0),
                    )))
                    .child(
                        div()
                            .absolute()
                            .left(px(self.hsv.saturation * 256.0 - 5.0))
                            .top(px((1.0 - self.hsv.value) * 144.0 - 5.0))
                            .size(px(10.0))
                            .rounded_full()
                            .border_2()
                            .border_color(gpui::white())
                            .bg(self.hsv.rgb()),
                    )
                    .child(
                        canvas(
                            move |bounds, _, _| color_bounds.set(bounds),
                            move |_, _, window, _| {
                                window.on_mouse_event(
                                    move |event: &MouseMoveEvent, phase, window, cx| {
                                        if phase.capture() {
                                            drag_listener(event, window, cx);
                                        }
                                    },
                                );
                                window.on_mouse_event(
                                    move |event: &MouseUpEvent, phase, window, cx| {
                                        if phase.capture() {
                                            release_listener(event, window, cx);
                                        }
                                    },
                                );
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                            this.drag = Some(Drag::Color);
                            this.drag_to(event.position, window, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                h_flex()
                    .id("hue-strip")
                    .relative()
                    .w_full()
                    .h(px(18.0))
                    .cursor(gpui::CursorStyle::Crosshair)
                    .children((0..6).map(|index| {
                        div().flex_1().h_full().bg(linear_gradient(
                            90.0,
                            linear_color_stop(
                                Hsv {
                                    hue: index as f32 / 6.0,
                                    saturation: 1.0,
                                    value: 1.0,
                                }
                                .rgb(),
                                0.0,
                            ),
                            linear_color_stop(
                                Hsv {
                                    hue: (index + 1) as f32 / 6.0,
                                    saturation: 1.0,
                                    value: 1.0,
                                }
                                .rgb(),
                                1.0,
                            ),
                        ))
                    }))
                    .child(
                        div()
                            .absolute()
                            .left(px(self.hsv.hue * 250.0))
                            .top_0()
                            .w(px(6.0))
                            .h_full()
                            .border_2()
                            .border_color(gpui::white())
                            .bg(gpui::transparent_black()),
                    )
                    .child(
                        canvas(move |bounds, _, _| hue_bounds.set(bounds), |_, _, _, _| {})
                            .absolute()
                            .size_full(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                            this.drag = Some(Drag::Hue);
                            this.drag_to(event.position, window, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                h_flex().gap_1().children(
                    [
                        0xff6600, 0xff5b42, 0xf43f5e, 0xa855f7, 0x3b82f6, 0x14b8a6, 0x22c55e,
                        0xeab308,
                    ]
                    .into_iter()
                    .map(|color| {
                        div()
                            .id(("preset", color as u64))
                            .size(px(28.0))
                            .bg(gpui::rgb(color))
                            .cursor_pointer()
                            .tab_index(0isize)
                            .aria_label(format!("#{color:06X}"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.saving {
                                    return;
                                }
                                if let Some(hsv) = Hsv::from_hex(&format!("{color:06x}")) {
                                    this.hsv = hsv;
                                    this.set_preview(Some(hsv.hex()), cx);
                                    this.sync_editor(window, cx);
                                }
                            }))
                    }),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size_6().flex_none().bg(swatch_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .child(self.editor.clone()),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("theme-color", "Theme")
                            .disabled(self.saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_preview(Some(String::new()), cx);
                                this.sync_editor(window, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-color", "Cancel")
                            .disabled(self.saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.restore(cx);
                                this.error = None;
                                if let Some(hsv) = Hsv::from_hex(&effective_color(&this.file, cx)) {
                                    this.hsv = hsv;
                                }
                                this.sync_editor(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("apply-color", if self.saving { "Saving…" } else { "Apply" })
                            .style(ButtonStyle::Filled)
                            .disabled(self.error.is_some() || self.saving)
                            .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn folder_color_picker_preview_and_cancel(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let cx = cx.add_empty_window();
        let file = SettingsUiFile::Project((
            project::WorktreeId::from_usize(1),
            util::rel_path::RelPath::empty().into(),
        ));
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_local_settings(
                        project::WorktreeId::from_usize(1),
                        LocalSettingsPath::InWorktree(util::rel_path::RelPath::empty().into()),
                        LocalSettingsKind::Settings,
                        Some(r##"{"folder_color":"#FF6600","tab_size":8}"##),
                        cx,
                    )
                    .unwrap();
            });
        });
        let picker =
            cx.update(|window, cx| cx.new(|cx| FolderColorPicker::new(file.clone(), window, cx)));
        cx.draw(
            gpui::point(px(0.0), px(0.0)),
            gpui::size(px(256.0), px(400.0)),
            |_, _| picker.clone().into_any_element(),
        );
        picker.update_in(cx, |picker, window, cx| {
            let bounds = picker.color_bounds.get();
            assert_eq!(bounds.size, gpui::size(px(256.0), px(144.0)));
            picker.hsv = Hsv {
                hue: 0.0,
                saturation: 1.0,
                value: 1.0,
            };
            picker.drag = Some(Drag::Color);
            picker.drag_to(bounds.center(), window, cx);
            assert_eq!(picker.draft.as_deref(), Some("#804040"));
        });
        cx.run_until_parked();
        picker.update(cx, |picker, cx| {
            picker.set_preview(Some("#00FF00".into()), cx)
        });
        cx.read(|cx| {
            assert_eq!(stored_color(&file, cx).as_deref(), Some("#00FF00"));
            let content = SettingsStore::global(cx)
                .get_content_for_file(file.to_settings())
                .unwrap();
            assert_eq!(
                serde_json::to_value(&content.project).unwrap()["tab_size"],
                8
            );
        });
        picker.update(cx, |picker, cx| picker.restore(cx));
        cx.read(|cx| assert_eq!(stored_color(&file, cx).as_deref(), Some("#FF6600")));
        picker.update(cx, |picker, cx| {
            picker.set_preview(Some("#0000FF".into()), cx)
        });
        drop(picker);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(stored_color(&file, cx).as_deref(), Some("#FF6600")));
    }

    #[test]
    fn folder_color_picker_hex_round_trip() {
        for red in (0..=255).step_by(17) {
            for green in (0..=255).step_by(17) {
                for blue in (0..=255).step_by(17) {
                    let hex = format!("#{red:02X}{green:02X}{blue:02X}");
                    assert_eq!(Hsv::from_hex(&hex).unwrap().hex(), hex);
                }
            }
        }
        assert_eq!(Hsv::from_hex(" FF6600 ").unwrap().hex(), "#FF6600");
        for invalid in ["#FFF", "#GG6600", "#FF660000", "", "orange"] {
            assert!(Hsv::from_hex(invalid).is_none());
        }
    }
}
