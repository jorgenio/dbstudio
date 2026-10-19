//! Inline SVG icon set: vector icons for the navigator, toolbar and tabs.
//!
//! Real SVGs (not emoji — fonts on some systems drop those to tofu boxes)
//! drawn on a 16×16 grid with per-icon accent colors that read well on both
//! the light and dark chrome. Handles are parsed once and cached for the
//! process lifetime.
//!
//! Raw strings use `r##"…"##` because attribute values like `fill="#5b8def"`
//! contain `"#`, which would close a plain `r#"…"#` literal.

use iced::widget::svg::{Handle, Svg};
use iced::Length;

/// Every icon the UI can show. `index()` must stay aligned with `build`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Connection,
    Database,
    Schema,
    Table,
    View,
    Collection,
    Column,
    Index,
    Function,
    Procedure,
    Trigger,
    User,
    Query,
    Run,
    Refresh,
    Design,
    NewTable,
    Delete,
    Sync,
    Er,
    Search,
    Sun,
    Moon,
    Add,
    Open,
    Close,
    Copy,
    Import,
    Save,
}

impl Icon {
    pub const COUNT: usize = 29;

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Option<Icon> {
        use Icon::*;
        Some(match i {
            0 => Connection,
            1 => Database,
            2 => Schema,
            3 => Table,
            4 => View,
            5 => Collection,
            6 => Column,
            7 => Index,
            8 => Function,
            9 => Procedure,
            10 => Trigger,
            11 => User,
            12 => Query,
            13 => Run,
            14 => Refresh,
            15 => Design,
            16 => NewTable,
            17 => Delete,
            18 => Sync,
            19 => Er,
            20 => Search,
            21 => Sun,
            22 => Moon,
            23 => Add,
            24 => Open,
            25 => Close,
            26 => Copy,
            27 => Import,
            28 => Save,
            _ => return None,
        })
    }

    /// Accent color of the icon as `#rrggbb` (also used by tests).
    pub fn color(self) -> &'static str {
        use Icon::*;
        match self {
            Connection => "#6f9ee0",
            Database => "#4aa3df",
            Schema => "#9b6ff0",
            Table => "#5b8def",
            View => "#2bb3a3",
            Collection => "#13aa52",
            Column => "#8b929e",
            Index => "#e6a23c",
            Function => "#d97706",
            Procedure => "#8b5cf6",
            Trigger => "#e5534b",
            User => "#7d8fa8",
            Query => "#5b8def",
            Run => "#3fb96a",
            Refresh => "#6f9ee0",
            Design => "#e6a23c",
            NewTable => "#5b8def",
            Delete => "#e5534b",
            Sync => "#2bb3a3",
            Er => "#9b6ff0",
            Search => "#8b929e",
            Sun => "#e6a23c",
            Moon => "#9b6ff0",
            Add => "#3fb96a",
            Open => "#5b8def",
            Close => "#e5534b",
            Copy => "#8a94a6",
            Import => "#4aa3df",
            Save => "#3fb96a",
        }
    }

    /// Icon for a navigator object kind (each kind gets its own glyph so the
    /// tree reads at a glance).
    pub fn for_kind(kind: dbstudio_core::ObjectKind) -> Icon {
        use dbstudio_core::ObjectKind as K;
        use Icon as I;
        match kind {
            K::Connection => I::Connection,
            K::Database => I::Database,
            K::Schema => I::Schema,
            K::Collection => I::Collection,
            K::Table => I::Table,
            K::View => I::View,
            K::Column => I::Column,
            K::Index => I::Index,
            K::Function => I::Function,
            K::Procedure => I::Procedure,
            K::Trigger => I::Trigger,
            K::User => I::User,
        }
    }
}

static HANDLES: std::sync::OnceLock<[Handle; Icon::COUNT]> = std::sync::OnceLock::new();

/// Cached SVG handle for an icon (parsed once per icon, per process).
pub fn handle(icon: Icon) -> Handle {
    let all = HANDLES.get_or_init(|| {
        std::array::from_fn(|i| {
            let ic = Icon::from_index(i).expect("icon index in range");
            Handle::from_memory(build(ic).into_bytes())
        })
    });
    all[icon.index()].clone()
}

/// Icon rendered as a fixed-size widget (square).
pub fn svg(icon: Icon, size: f32) -> Svg<'static> {
    Svg::new(handle(icon))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
}

/// Raw SVG document for an icon (exposed for tests).
#[cfg(test)]
pub fn markup(icon: Icon) -> String {
    build(icon)
}

fn build(icon: Icon) -> String {
    use Icon::*;
    let c = icon.color();
    let body = match icon {
        Connection => format!(
            r##"<rect x="1" y="2" width="14" height="5" rx="1.4" fill="{c}"/>
<rect x="1" y="9" width="14" height="5" rx="1.4" fill="{c}"/>
<circle cx="4.2" cy="4.5" r="1.15" fill="#ffffff"/>
<circle cx="4.2" cy="11.5" r="1.15" fill="#ffffff"/>
<rect x="6.6" y="3.9" width="6" height="1.3" rx=".65" fill="#ffffffaa"/>
<rect x="6.6" y="10.9" width="6" height="1.3" rx=".65" fill="#ffffffaa"/>"##
        ),
        Database => format!(
            r##"<path d="M2 4.3v7.4C2 13.2 4.7 14.5 8 14.5s6-1.3 6-2.8V4.3" fill="{c}"/>
<ellipse cx="8" cy="4.3" rx="6" ry="2.5" fill="{c}"/>
<ellipse cx="8" cy="4.3" rx="6" ry="2.5" fill="none" stroke="#ffffff55" stroke-width=".7"/>
<path d="M2 8.1c0 1.4 2.7 2.5 6 2.5s6-1.1 6-2.5" fill="none" stroke="#ffffff66" stroke-width="1"/>"##
        ),
        Schema => format!(
            r##"<rect x="1.5" y="1.5" width="5.7" height="5.7" rx="1.3" fill="{c}"/>
<rect x="8.8" y="1.5" width="5.7" height="5.7" rx="1.3" fill="{c}" opacity=".62"/>
<rect x="1.5" y="8.8" width="5.7" height="5.7" rx="1.3" fill="{c}" opacity=".62"/>
<rect x="8.8" y="8.8" width="5.7" height="5.7" rx="1.3" fill="{c}" opacity=".38"/>"##
        ),
        Table => format!(
            r##"<rect x="1.5" y="2.5" width="13" height="11" rx="1.6" fill="{c}"/>
<path d="M1.5 5.9h13" stroke="#ffffffaa" stroke-width="1.1"/>
<path d="M6.2 5.9v7.6M10.3 5.9v7.6M1.5 9.7h13" stroke="#ffffff77" stroke-width=".95"/>
<rect x="1.5" y="2.5" width="13" height="3.4" rx="1.6" fill="#ffffff26"/>"##
        ),
        View => format!(
            r##"<path d="M8 3.6C4.6 3.6 2 6.3 1.3 8c.23.5.23 1 0 1.5C2 11.2 4.6 13.9 8 13.9s6-2.7 6.7-4.4a1.7 1.7 0 0 0 0-1.5C14 6.3 11.4 3.6 8 3.6z" fill="{c}"/>
<circle cx="8" cy="8.75" r="2.5" fill="#ffffff"/>
<circle cx="8" cy="8.75" r="1.2" fill="{c}"/>"##
        ),
        Collection => format!(
            r##"<path d="M8 1.9l6.2 3.05L8 8 1.8 4.95 8 1.9z" fill="{c}"/>
<path d="M1.8 8L8 11.05 14.2 8" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" opacity=".8"/>
<path d="M1.8 11.1L8 14.15l6.2-3.05" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" opacity=".5"/>"##
        ),
        Column => format!(
            r##"<rect x="1.5" y="1.5" width="13" height="13" rx="3.2" fill="{c}" opacity=".2"/>
<path d="M6.4 4.4l-1 7.2M10.4 4.4l-1 7.2M4.4 6.9h7.4M3.9 9.6h7.4" stroke="{c}" stroke-width="1.5" stroke-linecap="round"/>"##
        ),
        Index => format!(
            r##"<circle cx="5.4" cy="5.6" r="3.3" fill="none" stroke="{c}" stroke-width="1.9"/>
<path d="M7.8 8l5.4 5.4M10.5 10.7l1.8-1.8M12.2 12.4l1.6-1.6" stroke="{c}" stroke-width="1.9" stroke-linecap="round"/>"##
        ),
        Function => format!(
            r##"<rect x="1.5" y="1.5" width="13" height="13" rx="3.2" fill="{c}" opacity=".2"/>
<path d="M10.6 4.6H8.5c-1.7 0-2.8 1-2.8 2.6v5.2" fill="none" stroke="{c}" stroke-width="1.6" stroke-linecap="round"/>
<path d="M5.1 7.5h6.1" stroke="{c}" stroke-width="1.5" stroke-linecap="round"/>"##
        ),
        Procedure => format!(
            r##"<path d="M5.7 5.2L2.9 8l2.8 2.8M10.3 5.2L13.1 8l-2.8 2.8" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>
<path d="M9.3 3.7L6.7 12.3" stroke="{c}" stroke-width="1.4" stroke-linecap="round" opacity=".7"/>"##
        ),
        Trigger => format!(
            r##"<path d="M9.2 1.6L3.4 9.1h3.6l-.9 5.3 5.9-7.7H8.7l.5-5.1z" fill="{c}"/>"##
        ),
        User => format!(
            r##"<circle cx="8" cy="5.2" r="3" fill="{c}"/>
<path d="M2.5 14c.6-3 2.9-4.7 5.5-4.7s4.9 1.7 5.5 4.7z" fill="{c}"/>"##
        ),
        Query => format!(
            r##"<rect x="2.5" y="1.5" width="11" height="13" rx="1.6" fill="#ffffff" stroke="{c}" stroke-width="1.4"/>
<path d="M5 5.4h6M5 8h6M5 10.6h4" stroke="{c}" stroke-width="1.3" stroke-linecap="round"/>"##
        ),
        Run => format!(
            r##"<path d="M4.8 2.9l8.2 5.1-8.2 5.1z" fill="{c}" stroke="{c}" stroke-width="1.4" stroke-linejoin="round"/>"##
        ),
        Refresh => format!(
            r##"<path d="M13.4 8a5.4 5.4 0 1 1-1.7-3.9" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round"/>
<path d="M13.7 1.9v3.3h-3.3" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
        Design => format!(
            r##"<path d="M2.4 13.7l.7-3.1 8.3-8.3 2.4 2.4-8.3 8.3-3.1.7z" fill="{c}"/>
<path d="M10.2 3.8l2.4 2.4" stroke="#ffffff" stroke-width="1"/>
<path d="M3.1 10.6l2.4 2.4" stroke="#ffffff" stroke-width="1"/>"##
        ),
        NewTable => format!(
            r##"<rect x="1.5" y="1.5" width="13" height="13" rx="3.2" fill="{c}" opacity=".2"/>
<path d="M8 4.4v7.2M4.4 8h7.2" stroke="{c}" stroke-width="2" stroke-linecap="round"/>"##
        ),
        Delete => format!(
            r##"<path d="M6.1 2.6h3.8v1.8H6.1z" fill="{c}"/>
<path d="M1.9 4.3h12.2" stroke="{c}" stroke-width="1.6" stroke-linecap="round"/>
<rect x="2.8" y="4.9" width="10.4" height="9.6" rx="1.6" fill="{c}"/>
<path d="M6.3 7.2v4.9M8 7.2v4.9M9.7 7.2v4.9" stroke="#ffffff" stroke-width="1.2" stroke-linecap="round"/>"##
        ),
        Sync => format!(
            r##"<path d="M2.4 5.6h9M9.2 3.2l2.4 2.4-2.4 2.4" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>
<path d="M13.6 10.4h-9M6.8 8l-2.4 2.4 2.4 2.4" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
        Er => format!(
            r##"<path d="M5.4 5.4l1.9 4.7M10.6 5.4L8.7 10.1M5.7 4h4.6" stroke="{c}" stroke-width="1.3" opacity=".75"/>
<circle cx="3.6" cy="4" r="2.1" fill="{c}"/>
<circle cx="12.4" cy="4" r="2.1" fill="{c}" opacity=".7"/>
<circle cx="8" cy="11.9" r="2.1" fill="{c}"/>"##
        ),
        Search => format!(
            r##"<circle cx="6.9" cy="6.9" r="4.4" fill="none" stroke="{c}" stroke-width="1.7"/>
<path d="M10.3 10.3l4.1 4.1" stroke="{c}" stroke-width="2" stroke-linecap="round"/>"##
        ),
        Sun => format!(
            r##"<circle cx="8" cy="8" r="3.1" fill="{c}"/>
<path d="M8 1.4v2.1M8 12.5v2.1M1.4 8h2.1M12.5 8h2.1M3.3 3.3l1.5 1.5M11.2 11.2l1.5 1.5M12.7 3.3l-1.5 1.5M4.8 11.2l-1.5 1.5" stroke="{c}" stroke-width="1.5" stroke-linecap="round"/>"##
        ),
        Moon => format!(
            r##"<path d="M13.5 9.9A5.9 5.9 0 0 1 6.1 2.5a6.3 6.3 0 1 0 7.4 7.4z" fill="{c}"/>"##
        ),
        Add => format!(
            r##"<rect x="1.5" y="1.5" width="13" height="13" rx="3.2" fill="{c}" opacity=".2"/>
<path d="M8 4.4v7.2M4.4 8h7.2" stroke="{c}" stroke-width="2" stroke-linecap="round"/>"##
        ),
        Open => format!(
            r##"<path d="M4.4 11.6L11.4 4.6M6.2 4.6h5.2v5.2" fill="none" stroke="{c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
        Close => format!(
            r##"<path d="M4.2 4.2l7.6 7.6M11.8 4.2l-7.6 7.6" stroke="{c}" stroke-width="1.8" stroke-linecap="round"/>"##
        ),
        Copy => format!(
            r##"<rect x="5.6" y="5.6" width="8.4" height="8.4" rx="1.6" fill="none" stroke="{c}" stroke-width="1.5"/>
<path d="M10.4 3.6H3.6a1 1 0 0 0-1 1v6.8" fill="none" stroke="{c}" stroke-width="1.5" stroke-linecap="round"/>"##
        ),
        Import => format!(
            r##"<path d="M8 2.4v6.4M5.6 6.4L8 8.8l2.4-2.4" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>
<path d="M3 10.4v2.8a.8.8 0 0 0 .8.8h8.4a.8.8 0 0 0 .8-.8v-2.8" fill="none" stroke="{c}" stroke-width="1.7" stroke-linecap="round"/>"##
        ),
        Save => format!(
            r##"<path d="M4.4 3.2A1.2 1.2 0 0 1 5.6 2h5.5L14 4.9v7.9A1.2 1.2 0 0 1 12.8 14H5.6A1.2 1.2 0 0 1 4.4 12.8z" fill="none" stroke="{c}" stroke-width="1.5" stroke-linejoin="round"/>
<rect x="6.2" y="2.4" width="3.6" height="3" rx=".5" fill="none" stroke="{c}" stroke-width="1.2"/>
<rect x="6.2" y="9" width="3.8" height="5" rx=".5" fill="none" stroke="{c}" stroke-width="1.2"/>"##
        ),
    };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" width="16" height="16">{body}</svg>"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ObjectKind;

    fn all() -> impl Iterator<Item = Icon> {
        (0..Icon::COUNT).map(|i| Icon::from_index(i).expect("icon index"))
    }

    #[test]
    fn every_index_maps_both_ways() {
        for icon in all() {
            assert_eq!(Icon::from_index(icon.index()), Some(icon));
        }
        assert!(Icon::from_index(Icon::COUNT).is_none());
    }

    #[test]
    fn every_icon_is_well_formed_svg() {
        for icon in all() {
            let m = markup(icon);
            assert!(m.starts_with("<svg "), "{icon:?}");
            assert!(m.ends_with("</svg>"), "{icon:?}");
            assert!(m.contains("viewBox=\"0 0 16 16\""), "{icon:?}");
            assert!(!m.contains('{'), "unreplaced placeholder in {icon:?}: {m}");
        }
    }

    #[test]
    fn icons_differ_from_each_other() {
        let all: Vec<String> = all().map(markup).collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "duplicate icon markup");
            }
        }
    }

    #[test]
    fn colors_are_six_digit_hex() {
        for icon in all() {
            let c = icon.color();
            assert_eq!(c.len(), 7, "{icon:?}");
            assert!(c.starts_with('#'), "{icon:?}");
            assert!(
                c[1..].chars().all(|ch| ch.is_ascii_hexdigit()),
                "{icon:?}"
            );
        }
    }

    #[test]
    fn object_kinds_have_icons() {
        let kinds = [
            ObjectKind::Connection,
            ObjectKind::Database,
            ObjectKind::Schema,
            ObjectKind::Collection,
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Column,
            ObjectKind::Index,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Trigger,
            ObjectKind::User,
        ];
        for k in kinds {
            let icon = Icon::for_kind(k);
            assert!(all().any(|i| i == icon), "{k:?} lost its icon");
        }
    }

    #[test]
    fn handles_are_cached() {
        let a = handle(Icon::Table);
        let b = handle(Icon::Table);
        assert!(a.id() == b.id(), "same cached handle expected");
    }
}
