use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde_json::Value as Json;

/// A single grid cell value, normalized across engines.
#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    DateTime(DateTime<Utc>),
    Json(Json),
    /// A value whose type the driver could not map to a known family.
    Other { type_name: String, value: String },
}

impl CellValue {
    pub fn is_null(&self) -> bool {
        matches!(self, CellValue::Null)
    }

    /// Text representation shown in the result grid.
    pub fn as_display(&self) -> Cow<'_, str> {
        match self {
            CellValue::Null => Cow::Borrowed(""),
            CellValue::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            CellValue::Int(i) => Cow::Owned(i.to_string()),
            CellValue::UInt(u) => Cow::Owned(u.to_string()),
            CellValue::Float(f) => Cow::Owned(format_float(*f)),
            CellValue::Text(s) => Cow::Borrowed(s),
            CellValue::Bytes(b) => Cow::Owned(hex_preview(b, 32)),
            CellValue::DateTime(dt) => Cow::Owned(dt.to_rfc3339()),
            CellValue::Json(j) => Cow::Owned(j.to_string()),
            CellValue::Other { value, .. } => Cow::Borrowed(value),
        }
    }
}

impl From<&str> for CellValue {
    fn from(s: &str) -> Self {
        CellValue::Text(s.to_string())
    }
}

impl From<String> for CellValue {
    fn from(s: String) -> Self {
        CellValue::Text(s)
    }
}

impl From<i64> for CellValue {
    fn from(v: i64) -> Self {
        CellValue::Int(v)
    }
}

impl From<bool> for CellValue {
    fn from(v: bool) -> Self {
        CellValue::Bool(v)
    }
}

impl From<f64> for CellValue {
    fn from(v: f64) -> Self {
        CellValue::Float(v)
    }
}

impl From<f32> for CellValue {
    fn from(v: f32) -> Self {
        CellValue::Float(v as f64)
    }
}

impl From<Vec<u8>> for CellValue {
    fn from(v: Vec<u8>) -> Self {
        CellValue::Bytes(v)
    }
}

impl From<DateTime<Utc>> for CellValue {
    fn from(v: DateTime<Utc>) -> Self {
        CellValue::DateTime(v)
    }
}

impl From<Json> for CellValue {
    fn from(v: Json) -> Self {
        CellValue::Json(v)
    }
}

fn format_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

fn hex_preview(bytes: &[u8], max: usize) -> String {
    let shown = bytes.len().min(max);
    let mut out = String::with_capacity(shown * 2 + 16);
    for b in &bytes[..shown] {
        out.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > max {
        out.push_str("…");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_null_is_empty() {
        assert_eq!(CellValue::Null.as_display(), "");
    }

    #[test]
    fn display_bytes_is_hex() {
        assert_eq!(CellValue::Bytes(vec![0xde, 0xad]).as_display(), "dead");
    }

    #[test]
    fn display_float_integrals() {
        assert_eq!(CellValue::Float(3.0).as_display(), "3.0");
        assert_eq!(CellValue::Float(3.5).as_display(), "3.5");
    }
}
