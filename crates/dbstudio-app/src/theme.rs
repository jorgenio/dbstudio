//! Navicat-inspired chrome: palette, pane surfaces, tabs and the engine
//! accent colors used in the connection tree. Every style reads the active
//! `Theme` palette, so the same functions serve the light and dark modes.

use iced::theme::Palette;
use iced::widget::{button, container, rule};
use iced::{color, Border, Color, Theme};

/// App theme: light or dark surfaces, Navicat-ish blue accent.
pub fn app_theme(dark: bool) -> Theme {
    let palette = if dark {
        Palette {
            background: color!(0x14171c),
            text: color!(0xd8dce3),
            primary: color!(0x5b8def),
            success: color!(0x3fb96a),
            warning: color!(0xe6a23c),
            danger: color!(0xe5534b),
        }
    } else {
        Palette {
            background: color!(0xf1f3f6),
            text: color!(0x2e3238),
            primary: color!(0x2f6fed),
            success: color!(0x1f9d55),
            warning: color!(0xd97706),
            danger: color!(0xd93025),
        }
    };
    Theme::custom("jorgenio dbstudio", palette)
}

/// True when the given theme carries the dark palette.
fn is_dark(theme: &Theme) -> bool {
    let bg = theme.palette().background;
    (bg.r + bg.g + bg.b) / 3.0 < 0.5
}

/// Pane surface (editor / grid): white in light, deep slate in dark.
fn pane_bg(dark: bool) -> Color {
    if dark { color!(0x1c2026) } else { color!(0xffffff) }
}

/// Slightly recessed surface (navigator, form panels).
fn raised_bg(dark: bool) -> Color {
    if dark { color!(0x191d23) } else { color!(0xfbfcfe) }
}

/// Chrome surface (toolbar / tab strip / status bar).
fn chrome_bg(dark: bool) -> Color {
    if dark { color!(0x171b21) } else { color!(0xf5f6f9) }
}

/// Hairline separators and control borders.
fn line(dark: bool) -> Color {
    if dark { color!(0x2c313a) } else { color!(0xdfe3e9) }
}

/// Primary text.
fn ink(dark: bool) -> Color {
    if dark { color!(0xd8dce3) } else { color!(0x2e3238) }
}

// --- surfaces -------------------------------------------------------------

/// Working area (editor / grid panes) with a hairline border.
pub fn workspace(theme: &Theme) -> container::Style {
    let dark = is_dark(theme);
    container::Style {
        background: Some(pane_bg(dark).into()),
        border: Border {
            color: line(dark),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Navigator column background (slightly off-white, framed).
pub fn navigator(theme: &Theme) -> container::Style {
    let dark = is_dark(theme);
    container::Style {
        background: Some(raised_bg(dark).into()),
        border: Border {
            color: line(dark),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Toolbar / tab-strip / status-bar chrome.
pub fn chrome(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(chrome_bg(is_dark(theme)).into()),
        ..Default::default()
    }
}

/// Hairline separator used between panes.
pub fn hairline(theme: &Theme) -> rule::Style {
    rule::Style {
        color: line(is_dark(theme)),
        radius: 0.0.into(),
        fill_mode: rule::FillMode::Full,
        snap: true,
    }
}

// --- tabs -----------------------------------------------------------------

/// Active file-style tab sitting on top of the pane.
pub fn tab_active(theme: &Theme, status: button::Status) -> button::Style {
    let dark = is_dark(theme);
    button::Style {
        background: Some(
            match status {
                button::Status::Hovered => {
                    if dark { color!(0x232a35) } else { color!(0xf4f7fd) }
                }
                _ => pane_bg(dark),
            }
            .into(),
        ),
        text_color: theme.palette().primary,
        border: top_tab_border(dark),
        ..Default::default()
    }
}

/// Tab whose pane has no content yet (e.g. results before the first run):
/// framed like an active tab but recessed into the chrome.
pub fn tab_inactive(theme: &Theme, status: button::Status) -> button::Style {
    let dark = is_dark(theme);
    button::Style {
        background: match status {
            button::Status::Hovered => Some(if dark { color!(0x1f242c) } else { color!(0xeceef2) }.into()),
            _ => None,
        },
        text_color: muted_on(dark),
        border: top_tab_border(dark),
        ..Default::default()
    }
}

/// Tab border: framed on top and sides, flush with the pane below.
fn top_tab_border(dark: bool) -> Border {
    let mut border = Border {
        color: line(dark),
        width: 1.0,
        radius: 6.0.into(),
    };
    border.radius.bottom_left = 0.0;
    border.radius.bottom_right = 0.0;
    border
}

// --- buttons --------------------------------------------------------------

/// Flat chrome button (toolbar / strip actions); dims when disabled.
pub fn flat(theme: &Theme, status: button::Status) -> button::Style {
    let dark = is_dark(theme);
    let (background, text_color) = match status {
        button::Status::Hovered => (
            Some(if dark { color!(0x232932) } else { color!(0xe8edf5) }.into()),
            ink(dark),
        ),
        button::Status::Pressed => (
            Some(if dark { color!(0x2a313c) } else { color!(0xdde5f2) }.into()),
            ink(dark),
        ),
        button::Status::Disabled => (None, muted_on(dark)),
        button::Status::Active => (None, ink(dark)),
    };
    button::Style {
        background,
        text_color,
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Highlighted navigator row (the connection selected in the sidebar).
pub fn row_selected(theme: &Theme, status: button::Status) -> button::Style {
    let dark = is_dark(theme);
    button::Style {
        background: Some(
            match status {
                button::Status::Hovered => {
                    if dark { color!(0x24344f) } else { color!(0xd7e3fc) }
                }
                _ => if dark { color!(0x1f2d47) } else { color!(0xe3ecfd) },
            }
            .into(),
        ),
        text_color: if dark { color!(0x8ab0ff) } else { color!(0x1d4fd7) },
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Solid primary action (Ejecutar / Conectar).
pub fn solid(theme: &Theme, status: button::Status) -> button::Style {
    let base = theme.palette().primary;
    button::Style {
        background: Some(
            match status {
                button::Status::Hovered => shade(base, 0.9),
                button::Status::Pressed => shade(base, 0.82),
                _ => base.into(),
            },
        ),
        text_color: color!(0xffffff),
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Scale a color's brightness (<1 darkens, >1 lightens).
fn shade(c: Color, factor: f32) -> iced::Background {
    Color {
        r: (c.r * factor).min(1.0),
        g: (c.g * factor).min(1.0),
        b: (c.b * factor).min(1.0),
        a: c.a,
    }
    .into()
}

/// Caption button of the custom title bar (minimize / maximize).
pub fn win_control(theme: &Theme, status: button::Status) -> button::Style {
    let dark = is_dark(theme);
    button::Style {
        background: match status {
            button::Status::Hovered => Some(if dark { color!(0x262b33) } else { color!(0xe4e7eb) }.into()),
            button::Status::Pressed => Some(if dark { color!(0x2e343d) } else { color!(0xd8dce2) }.into()),
            _ => None,
        },
        text_color: ink(dark),
        border: Border::default(),
        ..Default::default()
    }
}

/// Close caption button: paints red on hover like the native title bar.
pub fn win_close(theme: &Theme, status: button::Status) -> button::Style {
    let dark = is_dark(theme);
    let hovered = matches!(
        status,
        button::Status::Hovered | button::Status::Pressed
    );
    button::Style {
        background: match status {
            button::Status::Hovered => Some(color!(0xe81123).into()),
            button::Status::Pressed => Some(color!(0xc50f1f).into()),
            _ => None,
        },
        text_color: if hovered { color!(0xffffff) } else { ink(dark) },
        border: Border::default(),
        ..Default::default()
    }
}

// --- engine accents -------------------------------------------------------

/// Accent color for the connection/tree markers of each engine.
pub fn engine_color(engine: dbstudio_core::Engine) -> Color {
    use dbstudio_core::Engine;
    match engine {
        Engine::Postgres => color!(0x336791),
        Engine::Mysql => color!(0x00758f),
        Engine::MariaDb => color!(0x1a8f9e),
        Engine::Mongodb => color!(0x13aa52),
        Engine::Sqlite => color!(0x4a72b8),
        Engine::SqlServer => color!(0xb7312c),
        Engine::Oracle => color!(0xc74634),
    }
}

/// Muted foreground for secondary labels (readable on both surfaces).
pub fn muted() -> Color {
    color!(0x8b929e)
}

fn muted_on(dark: bool) -> Color {
    if dark { color!(0x7d8592) } else { color!(0x8b919c) }
}

// --- completion popup -----------------------------------------------------

/// Floating card with a soft shadow, over the editor.
pub fn completion_popup(theme: &Theme) -> container::Style {
    let dark = is_dark(theme);
    container::Style {
        background: Some(pane_bg(dark).into()),
        border: Border {
            color: line(dark),
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: iced::Shadow {
            color: Color {
                a: 0.35,
                ..Color::BLACK
            },
            offset: iced::Vector::new(0.0, 4.0),
            blur_radius: 12.0,
        },
        ..Default::default()
    }
}

/// Unselected suggestion row.
pub fn completion_item(theme: &Theme, status: button::Status) -> button::Style {
    let dark = is_dark(theme);
    button::Style {
        background: match status {
            button::Status::Hovered => Some(if dark { color!(0x232932) } else { color!(0xeef2f8) }.into()),
            _ => None,
        },
        text_color: ink(dark),
        border: Border::default(),
        ..Default::default()
    }
}

/// Highlighted suggestion row (keyboard selection).
pub fn completion_selected(theme: &Theme, status: button::Status) -> button::Style {
    let base = theme.palette().primary;
    button::Style {
        background: Some(match status {
            button::Status::Pressed => shade(base, 0.82),
            _ => base.into(),
        }),
        text_color: color!(0xffffff),
        border: Border::default(),
        ..Default::default()
    }
}

// --- editor code palette --------------------------------------------------

/// Palette handed to `iced-code-editor`, matched to the app mode.
pub struct EditorPalette {
    pub background: Color,
    pub gutter_background: Color,
}

pub fn editor_palette(dark: bool) -> EditorPalette {
    EditorPalette {
        background: pane_bg(dark),
        gutter_background: if dark { color!(0x181c22) } else { color!(0xfafbfd) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::Background;

    fn bg_color(b: Background) -> Color {
        match b {
            Background::Color(c) => c,
            _ => panic!("expected a solid color"),
        }
    }

    #[test]
    fn is_dark_detects_the_app_palette() {
        assert!(is_dark(&app_theme(true)));
        assert!(!is_dark(&app_theme(false)));
    }

    #[test]
    fn dark_surfaces_are_darker_than_light() {
        let light = bg_color(workspace(&app_theme(false)).background.unwrap());
        let dark = bg_color(workspace(&app_theme(true)).background.unwrap());
        let light_sum = light.r + light.g + light.b;
        let dark_sum = dark.r + dark.g + dark.b;
        assert!(dark_sum < light_sum, "dark workspace must be darker");
    }

    #[test]
    fn editor_palette_follows_app_mode() {
        assert_ne!(
            editor_palette(true).background,
            editor_palette(false).background
        );
        assert_ne!(
            editor_palette(true).gutter_background,
            editor_palette(false).gutter_background
        );
    }
}
