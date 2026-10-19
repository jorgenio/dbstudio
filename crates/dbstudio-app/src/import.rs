//! CSV/TSV/JSON parsing, column auto-mapping and INSERT generation for the
//! import wizard. Pure functions only: the caller does file I/O and UI.

use std::collections::HashMap;
use std::fmt;

use dbstudio_core::{ColumnMeta, Engine, ObjectRef, TypeFamily};
use serde_json::Value;

use crate::{quote_ident, table_ref, text_literal};

/// Source file format of the import wizard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportFormat {
    Csv,
    Tsv,
    Json,
}

impl ImportFormat {
    pub const ALL: [ImportFormat; 3] =
        [ImportFormat::Csv, ImportFormat::Tsv, ImportFormat::Json];

    /// Format suggested by the file extension (`.json` → JSON, `.tsv` → TSV).
    pub fn from_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("json") => ImportFormat::Json,
            Some(ext) if ext.eq_ignore_ascii_case("tsv") => ImportFormat::Tsv,
            _ => ImportFormat::Csv,
        }
    }
}

impl fmt::Display for ImportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ImportFormat::Csv => "CSV",
            ImportFormat::Tsv => "TSV",
            ImportFormat::Json => "JSON",
        };
        f.write_str(s)
    }
}

/// Parsed source file: column names plus raw string rows, all normalized to
/// the same width (short rows padded, extra cells get `colN` names).
#[derive(Clone)]
pub struct Parsed {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Delimiter guessed from a text head: whichever of `,` `;` TAB `|` is most
/// frequent in the first line (ties prefer `,`; no candidate → `,`).
pub fn sniff_delimiter(head: &str) -> char {
    let line = head.lines().next().unwrap_or("");
    // `,` listed last so `max_by_key` (last maximum) prefers it on ties.
    let counts = [
        (';', line.matches(';').count()),
        ('\t', line.matches('\t').count()),
        ('|', line.matches('|').count()),
        (',', line.matches(',').count()),
    ];
    counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(ch, _)| *ch)
        .unwrap_or(',')
}

/// Same as [`sniff_delimiter`] but reading only the first 8 KiB of a file.
pub fn sniff_file(path: &std::path::Path) -> Option<char> {
    use std::io::Read;

    let mut buf = [0u8; 8192];
    let mut file = std::fs::File::open(path).ok()?;
    let n = file.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }
    Some(sniff_delimiter(&String::from_utf8_lossy(&buf[..n])))
}

/// Parse file contents in the chosen format.
pub fn parse(
    text: &str,
    format: ImportFormat,
    delimiter: char,
    header: bool,
) -> Result<Parsed, String> {
    match format {
        ImportFormat::Tsv => parse_delimited(text, '\t', header),
        ImportFormat::Csv => parse_delimited(text, delimiter, header),
        ImportFormat::Json => parse_json(text),
    }
}

/// RFC 4180-style delimited parser: quoted fields, doubled quotes, embedded
/// delimiters/newlines, CR LF or LF line ends, BOM tolerated.
fn parse_delimited(text: &str, delim: char, header: bool) -> Result<Parsed, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        quoted = false;
                    }
                }
                _ => field.push(c),
            }
        } else {
            match c {
                // A quote only opens a quoted field at field start; mid-field
                // quotes are literal characters.
                '"' if field.is_empty() => quoted = true,
                c if c == delim => row.push(std::mem::take(&mut field)),
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                '\r' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                }
                _ => field.push(c),
            }
        }
    }
    if quoted {
        return Err("comilla sin cerrar en el fichero".into());
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    // A blank line parses as one empty field: drop it.
    rows.retain(|r| !(r.len() == 1 && r[0].is_empty()));
    if rows.is_empty() {
        return Ok(Parsed {
            columns: Vec::new(),
            rows: Vec::new(),
        });
    }

    let mut columns = if header {
        let head = rows.remove(0);
        let mut seen: HashMap<String, usize> = HashMap::new();
        head.into_iter()
            .enumerate()
            .map(|(i, raw)| {
                let trimmed = raw.trim();
                let base = if trimmed.is_empty() {
                    format!("col{}", i + 1)
                } else {
                    trimmed.to_string()
                };
                let n = seen.entry(base.clone()).or_insert(0);
                *n += 1;
                if *n == 1 {
                    base
                } else {
                    format!("{base}_{n}")
                }
            })
            .collect::<Vec<_>>()
    } else {
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        (1..=width).map(|i| format!("col{i}")).collect()
    };

    // Normalize every row to the column width (extra cells widen the header
    // with `colN` names; short rows pad with empty strings).
    let width = std::cmp::max(columns.len(), rows.iter().map(Vec::len).max().unwrap_or(0));
    while columns.len() < width {
        columns.push(format!("col{}", columns.len() + 1));
    }
    for r in &mut rows {
        r.resize(width, String::new());
    }

    Ok(Parsed { columns, rows })
}

/// JSON source: an array of objects `[{...}]`, one object per line (NDJSON)
/// or a single object. `null` becomes an empty string; column order is first
/// appearance across the whole file.
fn parse_json(text: &str) -> Result<Parsed, String> {
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Ok(Parsed {
            columns: Vec::new(),
            rows: Vec::new(),
        });
    }

    let values: Vec<Value> = if text.starts_with('[') {
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("JSON inválido: {e}"))?;
        value
            .as_array()
            .ok_or_else(|| "el JSON debe ser un array de objetos".to_string())?
            .clone()
    } else {
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|e| format!("línea {}: JSON inválido: {e}", i + 1))?;
            out.push(value);
        }
        out
    };

    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(values.len());
    for (i, value) in values.iter().enumerate() {
        let obj = value
            .as_object()
            .ok_or_else(|| format!("el elemento {} no es un objeto", i + 1))?;
        for key in obj.keys() {
            if !columns.iter().any(|c| c == key) {
                columns.push(key.clone());
            }
        }
        rows.push(
            columns
                .iter()
                .map(|c| obj.get(c).map(value_text).unwrap_or_default())
                .collect(),
        );
    }
    // Objects seen later can widen `columns`: pad earlier rows to match.
    for r in &mut rows {
        r.resize(columns.len(), String::new());
    }
    Ok(Parsed { columns, rows })
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Default mapping: match file columns to target columns by name
/// (case-insensitive, surrounding spaces ignored); unmatched stay `None`.
pub fn auto_map(file_columns: &[String], target: &[ColumnMeta]) -> Vec<Option<usize>> {
    file_columns
        .iter()
        .map(|file| {
            let file = file.trim();
            target
                .iter()
                .position(|t| t.name.eq_ignore_ascii_case(file))
        })
        .collect()
}

/// Multi-row `INSERT` batches for the mapped rows: only the mapped target
/// columns, literals by type family (numbers unquoted when they parse,
/// blanks become `''` for text and `NULL` otherwise). Batches are capped by
/// row count and statement size.
pub fn insert_statements(
    engine: Engine,
    source: &ObjectRef,
    columns: &[ColumnMeta],
    mapping: &[Option<usize>],
    rows: &[Vec<String>],
) -> Result<Vec<String>, String> {
    // (file column, target column) in file order; reject duplicate targets.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (file, target) in mapping.iter().enumerate() {
        let Some(target) = target else {
            continue;
        };
        let Some(meta) = columns.get(*target) else {
            return Err("mapeo fuera de rango".into());
        };
        if pairs.iter().any(|(_, t)| t == target) {
            return Err(format!(
                "la columna de destino «{}» está mapeada dos veces",
                meta.name
            ));
        }
        pairs.push((file, *target));
    }
    if pairs.is_empty() || rows.is_empty() {
        return Ok(Vec::new());
    }

    let from = table_ref(engine, source);
    let col_list = pairs
        .iter()
        .map(|(_, t)| quote_ident(&columns[*t].name, engine))
        .collect::<Vec<_>>()
        .join(", ");

    const BATCH_ROWS: usize = 200;
    const BATCH_BYTES: usize = 60_000;
    let mut stmts: Vec<String> = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let mut batch_bytes = 0usize;

    for row in rows {
        let mut values = Vec::with_capacity(pairs.len());
        for &(file, target) in &pairs {
            let raw = row.get(file).map(String::as_str).unwrap_or("");
            values.push(cell_literal(engine, columns[target].data_type.family, raw));
        }
        let tuple = format!("({})", values.join(", "));
        if !batch.is_empty()
            && (batch.len() >= BATCH_ROWS || batch_bytes + tuple.len() > BATCH_BYTES)
        {
            stmts.push(format!(
                "INSERT INTO {from} ({col_list}) VALUES {};",
                batch.join(", ")
            ));
            batch.clear();
            batch_bytes = 0;
        }
        batch_bytes += tuple.len() + 2;
        batch.push(tuple);
    }
    if !batch.is_empty() {
        stmts.push(format!(
            "INSERT INTO {from} ({col_list}) VALUES {};",
            batch.join(", ")
        ));
    }
    Ok(stmts)
}

/// Literal for one raw cell: blanks are `NULL` for non-text families (and
/// `''` for text), numeric families stay unquoted when they parse.
fn cell_literal(engine: Engine, family: TypeFamily, raw: &str) -> String {
    if raw.is_empty() {
        return match family {
            TypeFamily::Text | TypeFamily::Bytes => text_literal(engine, ""),
            _ => "NULL".to_string(),
        };
    }
    let trimmed = raw.trim();
    match family {
        TypeFamily::Int if trimmed.parse::<i64>().is_ok() => trimmed.to_string(),
        TypeFamily::UInt if trimmed.parse::<u64>().is_ok() => trimmed.to_string(),
        TypeFamily::Float | TypeFamily::Decimal if trimmed.parse::<f64>().is_ok() => {
            trimmed.to_string()
        }
        _ => text_literal(engine, raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ConnectionId, DataType};

    fn col(name: &str, family: TypeFamily) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            data_type: DataType::new(name, family),
            nullable: true,
            comment: None,
        }
    }

    fn table(path: &[&str]) -> ObjectRef {
        ObjectRef {
            connection: ConnectionId::nil(),
            path: path.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn delimited_handles_quotes_newlines_and_crlf() {
        let text = "\u{feff}id;nombre;extra\r\n1;\"Ana; L.\";\"línea 1\nlínea 2\"\r\n2;\"con \"\"comillas\"\"\";fin\r\n";
        let p = parse(text, ImportFormat::Csv, ';', true).unwrap();
        assert_eq!(p.columns, vec!["id", "nombre", "extra"]);
        assert_eq!(
            p.rows,
            vec![
                vec![
                    "1".to_string(),
                    "Ana; L.".into(),
                    "línea 1\nlínea 2".into()
                ],
                vec![
                    "2".to_string(),
                    "con \"comillas\"".into(),
                    "fin".into()
                ],
            ]
        );
    }

    #[test]
    fn delimited_without_header_pads_and_names_columns() {
        let p = parse("a,b,c\n1,2\n", ImportFormat::Csv, ',', false).unwrap();
        assert_eq!(p.columns, vec!["col1", "col2", "col3"]);
        assert_eq!(p.rows, vec![vec!["a", "b", "c"], vec!["1", "2", ""]]);
        // Duplicate header names get numeric suffixes; blank ones become colN.
        let p = parse("id, id, ,x\n1,2,3,4\n", ImportFormat::Csv, ',', true).unwrap();
        assert_eq!(p.columns, vec!["id", "id_2", "col3", "x"]);
        // Blank lines are ignored and an unterminated quote is an error.
        assert_eq!(
            parse("a,b\n\n1,2\n", ImportFormat::Csv, ',', true).unwrap().rows.len(),
            1
        );
        assert!(parse("a,\"broken", ImportFormat::Csv, ',', true).is_err());
    }

    #[test]
    fn sniff_prefers_the_richest_delimiter() {
        assert_eq!(sniff_delimiter("a;b;c;d"), ';');
        assert_eq!(sniff_delimiter("a\tb\tc"), '\t');
        // Ties (and plain comma files) prefer ','.
        assert_eq!(sniff_delimiter("a,b;c"), ',');
        assert_eq!(sniff_delimiter("a,b,c"), ',');
        assert_eq!(sniff_delimiter("sin delimitadores"), ',');
    }

    #[test]
    fn json_array_and_ndjson_share_column_union() {
        let p = parse(
            r#"[{"b": 2, "a": "x"}, {"c": null, "a": "y"}]"#,
            ImportFormat::Json,
            ',',
            true,
        )
        .unwrap();
        assert_eq!(p.columns, vec!["b", "a", "c"]);
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[0], vec!["2", "x", ""]);
        assert_eq!(p.rows[1], vec!["", "y", ""]);
        // Numbers/bools keep their raw text; strings stay verbatim.
        let p = parse(
            "{\"a\": 1.5, \"b\": true, \"c\": \"x,;y\"}\n{\"a\": 2}\n",
            ImportFormat::Json,
            ',',
            false,
        )
        .unwrap();
        assert_eq!(p.columns, vec!["a", "b", "c"]);
        assert_eq!(p.rows[0], vec!["1.5", "true", "x,;y"]);
        // Errors: malformed JSON and non-object elements.
        assert!(parse("{oops", ImportFormat::Json, ',', true).is_err());
        assert!(parse("[1, 2]", ImportFormat::Json, ',', true).is_err());
    }

    #[test]
    fn auto_map_is_case_insensitive_and_skips_unknowns() {
        let target = vec![col("ID", TypeFamily::Int), col("Nombre", TypeFamily::Text)];
        let map = auto_map(
            &[" id ".into(), "correo".into(), "NOMBRE".into()],
            &target,
        );
        assert_eq!(map, vec![Some(0), None, Some(1)]);
        assert_eq!(auto_map(&["x".into()], &[]), vec![None]);
    }

    #[test]
    fn insert_statements_batch_and_literalize_by_family() {
        let source = table(&["demo.db", "pedidos"]);
        let columns = vec![
            col("id", TypeFamily::Int),
            col("nombre", TypeFamily::Text),
            col("total", TypeFamily::Float),
        ];
        let map = vec![Some(0), Some(1), Some(2)];
        let rows = vec![
            vec!["1".into(), "Ana".into(), "9.5".into()],
            // Blank numeric → NULL, blank text → ''.
            vec![String::new(), String::new(), String::new()],
        ];
        let stmts = insert_statements(Engine::Sqlite, &source, &columns, &map, &rows).unwrap();
        assert_eq!(
            stmts,
            vec!["INSERT INTO \"pedidos\" (\"id\", \"nombre\", \"total\") VALUES (1, 'Ana', 9.5), (NULL, '', NULL);".to_string()]
        );

        // MySQL: backticks + db qualification and backslash escaping.
        let mysql_src = table(&["appdb", "pedidos"]);
        let rows = vec![vec!["7".into(), "a\\b'c".into(), "1".into()]];
        let stmts =
            insert_statements(Engine::Mysql, &mysql_src, &columns, &map, &rows).unwrap();
        assert_eq!(
            stmts,
            vec!["INSERT INTO `appdb`.`pedidos` (`id`, `nombre`, `total`) VALUES (7, 'a\\\\b''c', 1);".to_string()]
        );

        // Unparseable numeric falls back to a quoted literal.
        let rows = vec![vec!["x".into(), "n".into(), "y".into()]];
        let stmts = insert_statements(Engine::Postgres, &source, &columns, &map, &rows).unwrap();
        assert!(stmts[0].contains("('x', 'n', 'y')"));

        // Batch split: 201 rows → two statements.
        let rows: Vec<Vec<String>> = (0..201)
            .map(|i| vec![i.to_string(), String::new(), String::new()])
            .collect();
        let stmts = insert_statements(Engine::Sqlite, &source, &columns, &map, &rows).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("), ("));
    }

    #[test]
    fn insert_statements_reject_duplicates_and_empty_maps() {
        let source = table(&["demo.db", "pedidos"]);
        let columns = vec![col("id", TypeFamily::Int), col("nombre", TypeFamily::Text)];
        let rows = vec![vec!["1".into(), "x".into()]];
        let dup = vec![Some(0), Some(0)];
        assert!(
            insert_statements(Engine::Sqlite, &source, &columns, &dup, &rows)
                .unwrap_err()
                .contains("dos veces")
        );
        // Nothing mapped (or no rows) → no statements, no error.
        let none = vec![None, None];
        assert!(
            insert_statements(Engine::Sqlite, &source, &columns, &none, &rows)
                .unwrap()
                .is_empty()
        );
        let full = vec![Some(0), Some(1)];
        assert!(
            insert_statements(Engine::Sqlite, &source, &columns, &full, &[])
                .unwrap()
                .is_empty()
        );
    }
}
