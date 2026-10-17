//! Helpers shared across drivers.

use dbstudio_core::{TypeFamily, DataType};

/// Classify a type name reported by `information_schema`-style catalogs.
pub fn family_from_name(name: &str) -> TypeFamily {
    let n = name.to_ascii_lowercase();
    if n.contains("bool") {
        TypeFamily::Bool
    } else if n.contains("smallint") || n.contains("int") && !n.contains("point") {
        TypeFamily::Int
    } else if n.contains("double") || n.contains("real") || n.contains("float") {
        TypeFamily::Float
    } else if n.contains("numeric") || n.contains("decimal") {
        TypeFamily::Decimal
    } else if n.contains("char") || n.contains("text") || n.contains("clob") {
        TypeFamily::Text
    } else if n.contains("bytea") || n.contains("blob") || n.contains("binary") {
        TypeFamily::Bytes
    } else if n.contains("timestamp") || n.contains("time") || n.contains("date") {
        TypeFamily::DateTime
    } else if n.contains("json") || n.contains("bson") {
        TypeFamily::Json
    } else {
        TypeFamily::Other
    }
}

pub fn data_type_from_name(name: impl Into<String>) -> DataType {
    let name = name.into();
    let family = family_from_name(&name);
    DataType::new(name, family)
}

/// Hex-escape a string for use inside a MySQL-quoted identifier.
pub fn backtick_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Single-quoted SQL string literal (`'` doubled), for hand-built queries.
pub fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
