//! Result-set export to CSV (RFC 4180 quoting) and JSON.

use dbstudio_core::{CellValue, ColumnMeta};
use serde_json::{Map, Value as Json};

/// TSV-independent CSV field quoting: quote when the value contains a
/// comma, quote, or line break; double embedded quotes.
fn field(s: &str) -> String {
    if s.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn csv(columns: &[ColumnMeta], rows: &[&[CellValue]]) -> String {
    let mut out = String::new();
    out.push_str(
        &columns
            .iter()
            .map(|c| field(&c.name))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push_str("\r\n");
    for row in rows {
        out.push_str(
            &row.iter()
                .map(|c| field(&c.as_display()))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push_str("\r\n");
    }
    out
}

pub fn json(columns: &[ColumnMeta], rows: &[&[CellValue]]) -> String {
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    let objs: Vec<Json> = rows
        .iter()
        .map(|row| {
            let mut map = Map::new();
            for (i, name) in names.iter().enumerate() {
                map.insert((*name).to_string(), cell_json(row.get(i)));
            }
            Json::Object(map)
        })
        .collect();
    serde_json::to_string_pretty(&objs).unwrap_or_else(|_| "[]".to_string())
}

fn cell_json(cell: Option<&CellValue>) -> Json {
    match cell {
        None | Some(CellValue::Null) => Json::Null,
        Some(CellValue::Bool(b)) => Json::Bool(*b),
        Some(CellValue::Int(i)) => Json::from(*i),
        Some(CellValue::UInt(u)) => Json::from(*u),
        Some(CellValue::Float(f)) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Some(CellValue::Text(s)) => Json::String(s.clone()),
        Some(CellValue::Bytes(b)) => {
            let mut hex = String::with_capacity(2 + b.len() * 2);
            hex.push_str("0x");
            for byte in b {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
            }
            Json::String(hex)
        }
        Some(CellValue::DateTime(dt)) => Json::String(dt.to_rfc3339()),
        Some(CellValue::Json(j)) => j.clone(),
        Some(CellValue::Other { value, .. }) => Json::String(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DataType;

    fn cols(names: &[&str]) -> Vec<ColumnMeta> {
        names
            .iter()
            .map(|n| ColumnMeta {
                name: (*n).to_string(),
                data_type: DataType::other("text"),
                nullable: true,
                comment: None,
            })
            .collect()
    }

    #[test]
    fn csv_quotes_specials() {
        let rows: Vec<Vec<CellValue>> = vec![vec![
            CellValue::Text("a,b".to_string()),
            CellValue::Text("say \"hi\"".to_string()),
            CellValue::Int(7),
        ]];
        let refs: Vec<&[CellValue]> = rows.iter().map(Vec::as_slice).collect();
        let out = csv(&cols(&["x", "y", "z"]), &refs);
        assert_eq!(out, "x,y,z\r\n\"a,b\",\"say \"\"hi\"\"\",7\r\n");
    }

    #[test]
    fn json_builds_objects() {
        let rows: Vec<Vec<CellValue>> = vec![vec![
            CellValue::Null,
            CellValue::Bool(true),
            CellValue::Float(2.5),
        ]];
        let refs: Vec<&[CellValue]> = rows.iter().map(Vec::as_slice).collect();
        let out = json(&cols(&["a", "b", "c"]), &refs);
        assert_eq!(out, "[\n  {\n    \"a\": null,\n    \"b\": true,\n    \"c\": 2.5\n  }\n]");
    }
}
