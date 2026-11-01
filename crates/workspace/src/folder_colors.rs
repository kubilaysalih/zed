use gpui::{App, Hsla};
use project::Project;
use settings::{Settings, SettingsLocation};
use util::rel_path::RelPath;

use crate::WorkspaceSettings;

pub fn for_project(project: &Project, cx: &App) -> Option<Hsla> {
    let worktree = project.visible_worktrees(cx).next()?;
    let location = SettingsLocation {
        worktree_id: worktree.read(cx).id(),
        path: RelPath::empty(),
    };
    parse_color(&WorkspaceSettings::get(Some(location), cx).folder_color)
}

fn parse_color(value: &str) -> Option<Hsla> {
    let value = value.trim();
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(gpui::rgb(u32::from_str_radix(hex, 16).ok()?).into())
}

#[derive(Clone, Copy)]
pub(crate) struct ActivityBarColors {
    pub foreground: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub pressed: Hsla,
}

impl ActivityBarColors {
    pub fn new(background: Hsla, appearance: theme::Appearance) -> Self {
        let luminance = relative_luminance(background);
        let black: Hsla = gpui::rgb(0).into();
        let white: Hsla = gpui::rgb(0xffffff).into();
        let foreground = if appearance.is_light() { black } else { white };
        let overlay = if (0.04..=0.179).contains(&luminance) || luminance > 0.8 {
            black
        } else {
            white
        };
        Self {
            foreground,
            hover: background.blend(overlay.opacity(0.08)),
            selected: background.blend(overlay.opacity(0.18)),
            pressed: background.blend(overlay.opacity(0.26)),
        }
    }
}

fn relative_luminance(color: Hsla) -> f32 {
    let color = color.to_rgb();
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_folder_colors() {
        assert_eq!(parse_color("#12aBcD"), Some(gpui::rgb(0x12abcd).into()));
        assert_eq!(parse_color("#000000"), Some(gpui::rgb(0).into()));
        assert_eq!(parse_color("FF5B42"), Some(gpui::rgb(0xff5b42).into()));
        assert_eq!(parse_color(" #FF5B42 "), Some(gpui::rgb(0xff5b42).into()));
        for value in ["", "red", "#123", "#12345678", "#zzzzzz", "#éabcd"] {
            assert_eq!(parse_color(value), None);
        }
    }

    #[test]
    fn custom_folder_colors_follow_theme_appearance() {
        let mut colors = vec![0xff5b42, 0x000000, 0xffffff, 0x777777];
        for red in (0..=255).step_by(51) {
            for green in (0..=255).step_by(51) {
                for blue in (0..=255).step_by(51) {
                    colors.push((red << 16) | (green << 8) | blue);
                }
            }
        }
        for color in colors {
            let background = gpui::rgb(color).into();
            for (appearance, foreground) in [
                (theme::Appearance::Dark, gpui::rgb(0xffffff).into()),
                (theme::Appearance::Light, gpui::rgb(0).into()),
            ] {
                let palette = ActivityBarColors::new(background, appearance);
                assert_eq!(palette.foreground, foreground, "{color:06x}");
                assert_ne!(background, palette.hover);
                assert_ne!(palette.hover, palette.selected);
                assert_ne!(palette.selected, palette.pressed);
            }
        }
    }
}
