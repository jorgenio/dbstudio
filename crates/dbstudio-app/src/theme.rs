//! Navicat-inspired light chrome: palette, pane surfaces, tabs and the
//! engine accent colors used in the connection tree.

use iced::theme::Palette;
use iced::widget::{button, container, rule};
use iced::{color, Border, Color, Theme};

/// App theme: light surfaces, Navicat-ish blue accent.
pub fn app_theme() -> Theme {
    Theme::custom(
        "DBStudio",
        Palette {
            background: color!(0xf1f3f6),
            text: color!(0x2e3238),
            primary: color!(0x2f6fed),
            success: color!(0x1f9d55),
            warning: color!(0xd97706),
            danger: color!(0xd93025),
        },
    )
}

// --- surfaces -------------------------------------------------------------

/// White working area (editor / grid panes) with a hairline border.
pub fn workspace(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(color!(0xffffff).into()),
        border: Border {
            color: color!(0xdfe3e9),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Navigator column background (slightly off-white, framed).
pub fn navigator(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(color!(0xfbfcfe).into()),
        border: Border {
            color: color!(0xdfe3e9),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Toolbar / tab-strip / status-bar chrome.
pub fn chrome(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(color!(0xf5f6f9).into()),
        ..Default::default()
    }
}

/// Hairline separator used between panes.
pub fn hairline(theme: &Theme) -> rule::Style {
    let _ = theme;
    rule::Style {
        color: color!(0xdfe3e9),
        radius: 0.0.into(),
        fill_mode: rule::FillMode::Full,
        snap: true,
    }
}

// --- tabs -----------------------------------------------------------------

/// Active file-style tab sitting on top of a white pane.
pub fn tab_active(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(
            match status {
                button::Status::Hovered => color!(0xf4f7fd),
                _ => color!(0xffffff),
            }
            .into(),
        ),
        text_color: color!(0x2f6fed),
        border: top_tab_border(),
        ..Default::default()
    }
}

/// Tab whose pane has no content yet (e.g. results before the first run):
/// framed like an active tab but recessed into the chrome.
pub fn tab_inactive(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered => Some(color!(0xeceef2).into()),
            _ => None,
        },
        text_color: color!(0x8b919c),
        border: top_tab_border(),
        ..Default::default()
    }
}

/// Tab border: framed on top and sides, flush with the pane below.
fn top_tab_border() -> Border {
    let mut border = Border {
        color: color!(0xdfe3e9),
        width: 1.0,
        radius: 6.0.into(),
    };
    border.radius.bottom_left = 0.0;
    border.radius.bottom_right = 0.0;
    border
}

// --- buttons --------------------------------------------------------------

/// Flat chrome button (toolbar / strip actions); dims when disabled.
pub fn flat(_theme: &Theme, status: button::Status) -> button::Style {
    let (background, text_color) = match status {
        button::Status::Hovered => (Some(color!(0xe8edf5).into()), color!(0x2e3238)),
        button::Status::Pressed => (Some(color!(0xdde5f2).into()), color!(0x2e3238)),
        button::Status::Disabled => (None, color!(0xa8aeb8)),
        button::Status::Active => (None, color!(0x2e3238)),
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
pub fn row_selected(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(
            match status {
                button::Status::Hovered => color!(0xd7e3fc),
                _ => color!(0xe3ecfd),
            }
            .into(),
        ),
        text_color: color!(0x1d4fd7),
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Solid primary action (Ejecutar / Conectar).
pub fn solid(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(
            match status {
                button::Status::Hovered => color!(0x2a63d6),
                button::Status::Pressed => color!(0x2459c2),
                _ => color!(0x2f6fed),
            }
            .into(),
        ),
        text_color: color!(0xffffff),
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Caption button of the custom title bar (minimize / maximize).
pub fn win_control(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered => Some(color!(0xe4e7eb).into()),
            button::Status::Pressed => Some(color!(0xd8dce2).into()),
            _ => None,
        },
        text_color: color!(0x2e3238),
        border: Border::default(),
        ..Default::default()
    }
}

/// Close caption button: paints red on hover like the native title bar.
pub fn win_close(_theme: &Theme, status: button::Status) -> button::Style {
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
        text_color: if hovered {
            color!(0xffffff)
        } else {
            color!(0x2e3238)
        },
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

/// Muted foreground for secondary labels.
pub fn muted() -> Color {
    color!(0x7b8290)
}
