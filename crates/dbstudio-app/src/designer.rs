//! Table designer: column grid state, DDL generation and the SQL preview
//! that the "Aplicar" button runs.
//!
//! New tables produce a single `CREATE TABLE`; edits against an existing
//! table are diffed column-by-column (add / drop / modify) with
//! engine-specific `ALTER` dialects. Statements the engine cannot express
//! (SQLite type changes, PK rewrites) come back as `--` comments so the
//! preview stays honest about what will and will not run.

use dbstudio_core::{ColumnMeta, ConnectionId, Engine, ObjectRef};
use iced::widget::{button, checkbox, container, row, scrollable, text, text_input, Column};
use iced::{Element, Length};

/// One editable row of the column grid.
#[derive(Clone, Debug, PartialEq)]
pub struct ColRow {
    pub name: String,
    pub ty: String,
    pub nullable: bool,
    pub default: String,
    pub pk: bool,
    pub comment: String,
}

impl ColRow {
    fn from_meta(meta: &ColumnMeta, pk: bool) -> Self {
        Self {
            name: meta.name.clone(),
            ty: meta.data_type.name.clone(),
            nullable: meta.nullable,
            default: String::new(),
            pk,
            comment: meta.comment.clone().unwrap_or_default(),
        }
    }
}

/// Designer document: what the grid holds plus what it was loaded from.
#[derive(Clone, Debug)]
pub struct DesignerState {
    pub connection: ConnectionId,
    /// Container of the table (for qualified CREATE and tree refresh).
    pub parent: ObjectRef,
    /// `Some` while editing a saved table (its name is immutable here).
    pub table: Option<ObjectRef>,
    pub name: String,
    pub columns: Vec<ColRow>,
    /// Column snapshot at load: the diff baseline (`None` = new table).
    pub existing: Option<Vec<ColRow>>,
    /// Original primary keys (PK edits are not auto-applied on save).
    pub original_pks: Vec<String>,
}

impl DesignerState {
    /// Fresh document for a new table under `parent`.
    pub fn new_table(connection: ConnectionId, parent: ObjectRef) -> Self {
        Self {
            connection,
            parent,
            table: None,
            name: String::new(),
            columns: vec![default_column_row()],
            existing: None,
            original_pks: Vec::new(),
        }
    }

    /// Designer for an existing table, seeded from `describe_table`.
    pub fn from_table(
        connection: ConnectionId,
        table: ObjectRef,
        detail: dbstudio_drivers::TableDetail,
    ) -> Self {
        let name = table
            .path
            .last()
            .cloned()
            .unwrap_or_else(|| "tabla".to_string());
        let parent = if table.path.len() > 1 {
            ObjectRef {
                connection,
                path: table.path[..table.path.len() - 1].to_vec(),
            }
        } else {
            ObjectRef::root(connection)
        };
        let columns: Vec<ColRow> = detail
            .columns
            .iter()
            .map(|meta| {
                let pk = detail
                    .primary_keys
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(&meta.name));
                ColRow::from_meta(meta, pk)
            })
            .collect();
        let original_pks = detail.primary_keys.clone();
        Self {
            connection,
            parent,
            table: Some(table),
            name,
            existing: Some(columns.clone()),
            columns,
            original_pks,
        }
    }

    /// Document that diffs a desired `source` structure against the
    /// `target` currently stored at `table` (`None` = target table is
    /// missing, so the source is created from scratch). Used by `sync`.
    pub fn for_sync(
        connection: ConnectionId,
        table: ObjectRef,
        source: &dbstudio_drivers::TableDetail,
        target: Option<&dbstudio_drivers::TableDetail>,
    ) -> Self {
        let mut state = Self::from_table(connection, table, source.clone());
        match target {
            Some(t) => {
                let pks: Vec<String> = t.primary_keys.clone();
                let columns: Vec<ColRow> = t
                    .columns
                    .iter()
                    .map(|meta| {
                        let pk = pks.iter().any(|k| k.eq_ignore_ascii_case(&meta.name));
                        ColRow::from_meta(meta, pk)
                    })
                    .collect();
                state.existing = Some(columns);
                state.original_pks = pks;
            }
            // No baseline: preview falls through to a plain CREATE TABLE.
            None => state.existing = None,
        }
        state
    }

    pub fn apply(&mut self, msg: DesignMsg) {
        match msg {
            DesignMsg::Name(name) => {
                if self.table.is_none() {
                    self.name = name;
                }
            }
            DesignMsg::Add => self.columns.push(default_column_row()),
            DesignMsg::Remove(i) => {
                if i < self.columns.len() {
                    self.columns.remove(i);
                }
            }
            DesignMsg::Col(i, field) => {
                if let Some(col) = self.columns.get_mut(i) {
                    match field {
                        Field::Name(v) => col.name = v,
                        Field::Type(v) => col.ty = v,
                        Field::Nullable(v) => col.nullable = v,
                        Field::Default(v) => col.default = v,
                        Field::Pk(v) => col.pk = v,
                        Field::Comment(v) => col.comment = v,
                    }
                }
            }
        }
    }

    /// Object the column list would refresh after an apply.
    pub fn target(&self) -> ObjectRef {
        match &self.table {
            Some(t) => t.clone(),
            None => self.parent.child(&self.name),
        }
    }
}

fn default_column_row() -> ColRow {
    ColRow {
        name: "id".to_string(),
        ty: "INTEGER".to_string(),
        nullable: false,
        default: String::new(),
        pk: true,
        comment: String::new(),
    }
}

/// Grid edits, one per widget action; mapped to `Message::Design`.
#[derive(Clone, Debug)]
pub enum DesignMsg {
    Name(String),
    Add,
    Remove(usize),
    Col(usize, Field),
}

#[derive(Clone, Debug)]
pub enum Field {
    Name(String),
    Type(String),
    Nullable(bool),
    Default(String),
    Pk(bool),
    Comment(String),
}

// --- DDL -------------------------------------------------------------------

/// Identifier quoting: backticks for MySQL/MariaDB, double quotes elsewhere.
pub fn quote_ident(engine: Engine, ident: &str) -> String {
    match engine {
        Engine::Mysql | Engine::MariaDb => format!("`{}`", ident.replace('`', "``")),
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

/// Qualified name: `db.table` / `schema.table` from the parent's last
/// segment (SQLite is a single file — bare name).
fn qualified(engine: Engine, parent: &ObjectRef, name: &str) -> String {
    let q = |s: &str| quote_ident(engine, s);
    if engine == Engine::Sqlite {
        return q(name);
    }
    match parent.path.last() {
        Some(seg) => format!("{}.{}", q(seg), q(name)),
        None => q(name),
    }
}

/// SQL literal for a default: bare for numbers / known functions or
/// keywords, quoted as a string otherwise.
fn default_literal(raw: &str) -> String {
    let v = raw.trim();
    let upper = v.to_ascii_uppercase();
    if v.parse::<f64>().is_ok()
        || v.contains('(')
        || upper == "NULL"
        || matches!(
            upper.as_str(),
            "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME" | "LOCALTIME"
                | "LOCALTIMESTAMP" | "TRUE" | "FALSE" | "RANDOM"
        )
    {
        v.to_string()
    } else {
        format!("'{}'", v.replace('\'', "''"))
    }
}

fn column_line(engine: Engine, c: &ColRow, trailing_comma: bool) -> String {
    let mut line = format!(
        "  {} {}{}{}",
        quote_ident(engine, &c.name),
        c.ty,
        if c.nullable { "" } else { " NOT NULL" },
        if c.default.trim().is_empty() {
            String::new()
        } else {
            format!(" DEFAULT {}", default_literal(&c.default))
        }
    );
    if engine == Engine::Mysql || engine == Engine::MariaDb {
        if let Some(kw) = keyword_for(engine, &c.ty) {
            line.push_str(&format!(" {kw}"));
        }
    }
    if engine == Engine::Mysql || engine == Engine::MariaDb {
        if !c.comment.is_empty() {
            line.push_str(&format!(" COMMENT '{}'", c.comment.replace('\'', "''")));
        }
    }
    if trailing_comma {
        line.push(',');
    }
    line
}

/// MySQL wants an explicit KEY/PRIMARY KEY marker per column for PKs.
fn keyword_for(engine: Engine, ty: &str) -> Option<&'static str> {
    match engine {
        Engine::Mysql | Engine::MariaDb if ty.eq_ignore_ascii_case("integer") => {
            Some("/* col */")
        }
        _ => None,
    }
}

/// Statements the designer would run, `--` lines included as comments.
pub fn preview(engine: Engine, state: &DesignerState) -> Vec<String> {
    if state.name.trim().is_empty() || state.columns.iter().all(|c| c.name.trim().is_empty()) {
        return Vec::new();
    }
    match &state.existing {
        None => vec![create_statement(engine, state)],
        Some(orig) => diff_statements(engine, state, orig),
    }
}

fn create_statement(engine: Engine, state: &DesignerState) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (i, col) in state.columns.iter().enumerate() {
        let is_last = i + 1 == state.columns.len();
        let has_pk_clause = state.columns.iter().any(|c| c.pk);
        lines.push(column_line(
            engine,
            col,
            !(is_last && !has_pk_clause),
        ));
        if col.pk && !is_last && engine != Engine::Mysql && engine != Engine::MariaDb {
            // Comma already added by column_line; PK clause appended later.
        }
    }
    if let Some(pks) = pk_clause(engine, state) {
        lines.push(pks);
    }
    let table = qualified(engine, &state.parent, &state.name);
    let mut stmt = format!("CREATE TABLE {table} (\n{}\n)", lines.join("\n"));
    stmt.push(';');
    if engine == Engine::Postgres {
        for (i, col) in state.columns.iter().enumerate() {
            if !col.comment.is_empty() {
                stmt.push_str(&format!(
                    "\nCOMMENT ON COLUMN {}.{} IS '{}';",
                    table,
                    quote_ident(engine, &col.name),
                    col.comment.replace('\'', "''")
                ));
                let _ = i;
            }
        }
    }
    stmt
}

/// `PRIMARY KEY (...)` clause for engines that take it at table level.
fn pk_clause(engine: Engine, state: &DesignerState) -> Option<String> {
    let pks: Vec<String> = state
        .columns
        .iter()
        .filter(|c| c.pk)
        .map(|c| quote_ident(engine, &c.name))
        .collect();
    if pks.is_empty() {
        return None;
    }
    // MySQL/MariaDB can carry PRIMARY KEY inline on the column instead;
    // table-level works too, but avoid emitting both.
    Some(format!("  PRIMARY KEY ({})", pks.join(", ")))
}

fn diff_statements(engine: Engine, state: &DesignerState, orig: &[ColRow]) -> Vec<String> {
    let mut stmts: Vec<String> = Vec::new();
    let table = qualified(engine, &state.parent, &state.name);
    let find_orig = |name: &str| orig.iter().find(|c| c.name.eq_ignore_ascii_case(name));
    let has_new = |name: &str| {
        state
            .columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(name))
    };
    let mut dropped: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();

    for col in &state.columns {
        if col.name.trim().is_empty() {
            continue;
        }
        if find_orig(&col.name).is_none() {
            added.push(col.name.clone());
            let mut line = format!(
                "ALTER TABLE {table} ADD COLUMN {} {}{}{}",
                quote_ident(engine, &col.name),
                col.ty,
                if col.nullable { "" } else { " NOT NULL" },
                if col.default.trim().is_empty() {
                    String::new()
                } else {
                    format!(" DEFAULT {}", default_literal(&col.default))
                }
            );
            if col.pk {
                line.push_str(" /* añade PK manualmente */");
            }
            stmts.push(format!("{line};"));
        }
    }
    for old in orig {
        if !has_new(&old.name) {
            dropped.push(old.name.clone());
            stmts.push(format!(
                "ALTER TABLE {table} DROP COLUMN {};",
                quote_ident(engine, &old.name)
            ));
        }
    }
    if !dropped.is_empty() && !added.is_empty() {
        stmts.insert(
            0,
            format!(
                "-- AVISO: se eliminan [{}] y se añaden [{}]: si era un renombre, revisa la tabla",
                dropped.join(", "),
                added.join(", ")
            ),
        );
    }

    for col in &state.columns {
        let Some(old) = find_orig(&col.name) else {
            continue;
        };
        let type_changed = !old.ty.eq_ignore_ascii_case(&col.ty);
        let null_changed = old.nullable != col.nullable;
        let default_given = !col.default.trim().is_empty();
        match engine {
            Engine::Sqlite => {
                if type_changed || null_changed {
                    stmts.push(format!(
                        "-- {}: SQLite no permite cambiar tipo/nulabilidad (recrea la tabla)",
                        col.name
                    ));
                }
            }
            Engine::Mysql | Engine::MariaDb => {
                if type_changed || null_changed {
                    let mut line = format!(
                        "ALTER TABLE {table} MODIFY COLUMN {} {}{}",
                        quote_ident(engine, &col.name),
                        col.ty,
                        if col.nullable { "" } else { " NOT NULL" }
                    );
                    if default_given {
                        line.push_str(&format!(" DEFAULT {}", default_literal(&col.default)));
                    } else {
                        line.push_str(&format!(
                            " /* {}: revisa si tenía DEFAULT (MODIFY lo sustituye) */",
                            col.name
                        ));
                    }
                    stmts.push(format!("{line};"));
                }
            }
            Engine::Postgres => {
                if type_changed {
                    stmts.push(format!(
                        "ALTER TABLE {table} ALTER COLUMN {} TYPE {} USING {}::{};",
                        quote_ident(engine, &col.name),
                        col.ty,
                        quote_ident(engine, &col.name),
                        col.ty
                    ));
                }
                if null_changed {
                    stmts.push(format!(
                        "ALTER TABLE {table} ALTER COLUMN {} {};",
                        quote_ident(engine, &col.name),
                        if col.nullable { "DROP NOT NULL" } else { "SET NOT NULL" }
                    ));
                }
            }
            Engine::SqlServer => {
                if type_changed || null_changed {
                    stmts.push(format!(
                        "ALTER TABLE {table} ALTER COLUMN {} {} {};",
                        quote_ident(engine, &col.name),
                        col.ty,
                        if col.nullable { "NULL" } else { "NOT NULL" }
                    ));
                }
            }
            Engine::Mongodb | Engine::Oracle => {}
        }
    }

    let now_pks: Vec<String> = state
        .columns
        .iter()
        .filter(|c| c.pk)
        .map(|c| c.name.to_lowercase())
        .collect();
    let before_pks: Vec<String> = state
        .original_pks
        .iter()
        .map(|k| k.to_lowercase())
        .collect();
    if now_pks != before_pks {
        stmts.push(
            "-- cambios en PRIMARY KEY no se aplican automáticamente (hazlos a mano)".to_string(),
        );
    }
    stmts
}

// --- view ------------------------------------------------------------------

/// Column grid + SQL preview; mapped to `Message::Design` by the app.
pub fn view(state: &DesignerState, engine: Engine) -> Element<'_, DesignMsg> {
    let stmts = preview(engine, state);

    let mut header = row![].spacing(6).align_y(iced::Alignment::Center);
    for (label, w) in [
        ("Columna", 150.0),
        ("Tipo", 150.0),
        ("NN", 44.0),
        ("Default", 120.0),
        ("PK", 44.0),
        ("Comentario", 170.0),
    ] {
        header = header.push(
            text(label)
                .size(11)
                .color(crate::theme::muted())
                .width(Length::Fixed(w)),
        );
    }
    header = header.push(text(" ").size(11).width(Length::Fill));

    let mut rows = Column::new().push(header).spacing(4);
    for (i, col) in state.columns.iter().enumerate() {
        let name_input: Element<'_, DesignMsg> = if state.table.is_some() {
            text(col.name.clone())
                .size(13)
                .width(Length::Fixed(150.0))
                .into()
        } else {
            text_input("nombre", &col.name)
                .on_input(move |v| DesignMsg::Col(i, Field::Name(v)))
                .size(13)
                .padding([6, 8])
                .width(Length::Fixed(150.0))
                .into()
        };
        let row_el = row![
            name_input,
            text_input("tipo", &col.ty)
                .on_input(move |v| DesignMsg::Col(i, Field::Type(v)))
                .size(13)
                .padding([6, 8])
                .width(Length::Fixed(150.0)),
            checkbox(col.nullable)
                .on_toggle(move |v| DesignMsg::Col(i, Field::Nullable(v)))
                .size(14)
                .width(Length::Fixed(44.0)),
            text_input("—", &col.default)
                .on_input(move |v| DesignMsg::Col(i, Field::Default(v)))
                .size(13)
                .padding([6, 8])
                .width(Length::Fixed(120.0)),
            checkbox(col.pk)
                .on_toggle(move |v| DesignMsg::Col(i, Field::Pk(v)))
                .size(14)
                .width(Length::Fixed(44.0)),
            text_input("…", &col.comment)
                .on_input(move |v| DesignMsg::Col(i, Field::Comment(v)))
                .size(13)
                .padding([6, 8])
                .width(Length::Fixed(170.0)),
            button(text("✕").size(11))
                .padding([4, 8])
                .style(crate::theme::flat)
                .on_press_maybe(
                    (state.columns.len() > 1).then_some(DesignMsg::Remove(i)),
                ),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        rows = rows.push(row_el);
    }
    rows = rows.push(
        button(text("+ Añadir columna").size(12))
            .padding([6, 10])
            .style(crate::theme::flat)
            .on_press(DesignMsg::Add),
    );

    let name_field: Element<'_, DesignMsg> = if state.table.is_some() {
        text(state.name.clone())
            .size(13)
            .width(Length::Fixed(240.0))
            .into()
    } else {
        text_input("nombre_de_la_tabla", &state.name)
            .on_input(DesignMsg::Name)
            .size(13)
            .padding([6, 8])
            .width(Length::Fixed(240.0))
            .into()
    };

    let sql = if stmts.is_empty() {
        "-- sin cambios pendientes".to_string()
    } else {
        stmts.join("\n")
    };
    let preview_pane = container(
        scrollable(text(sql).size(12).font(iced::Font::MONOSPACE))
            .height(Length::Fixed(150.0)),
    )
    .padding(10)
    .style(crate::theme::workspace);

    Column::new()
        .push(
            row![
                text("Nombre").size(11).color(crate::theme::muted()),
                name_field,
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        )
        .push(rows)
        .push(text("SQL a aplicar").size(11).color(crate::theme::muted()))
        .push(preview_pane)
        .spacing(8)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_new(name: &str) -> DesignerState {
        let mut s = DesignerState::new_table(
            ConnectionId::nil(),
            ObjectRef {
                connection: ConnectionId::nil(),
                path: vec!["archivos".into()],
            },
        );
        s.name = name.to_string();
        s.columns = vec![
            ColRow {
                name: "id".into(),
                ty: "INTEGER".into(),
                nullable: false,
                default: String::new(),
                pk: true,
                comment: String::new(),
            },
            ColRow {
                name: "nombre".into(),
                ty: "TEXT".into(),
                nullable: false,
                default: "sin nombre".into(),
                pk: false,
                comment: "display".into(),
            },
        ];
        s
    }

    #[test]
    fn create_quotes_per_engine_and_adds_pk_clause() {
        let s = state_new("pedidos");
        let sqlite = preview(Engine::Sqlite, &s);
        assert_eq!(sqlite.len(), 1);
        assert!(sqlite[0].starts_with("CREATE TABLE \"pedidos\""));
        assert!(sqlite[0].contains("\"id\" INTEGER NOT NULL"));
        assert!(sqlite[0].contains("\"nombre\" TEXT NOT NULL DEFAULT 'sin nombre'"));
        assert!(sqlite[0].contains("PRIMARY KEY (\"id\")"));
        assert!(!sqlite[0].contains("archivos"), "sqlite is file-scoped");

        let mysql = preview(Engine::Mysql, &s);
        assert!(mysql[0].starts_with("CREATE TABLE `archivos`.`pedidos`"), "{}", mysql[0]);
        assert!(mysql[0].contains("`nombre` TEXT NOT NULL DEFAULT 'sin nombre' COMMENT 'display'"));
    }

    #[test]
    fn postgres_adds_comment_on_column_statements() {
        let s = state_new("pedidos");
        let stmts = preview(Engine::Postgres, &s);
        assert_eq!(stmts.len(), 1, "one statement, embedded comments");
        assert!(stmts[0].contains("COMMENT ON COLUMN \"archivos\".\"pedidos\".\"nombre\" IS 'display';"));
    }

    #[test]
    fn defaults_stay_bare_for_numbers_and_functions() {
        let mut s = state_new("t");
        s.columns[1].default = "0".into();
        assert!(preview(Engine::Sqlite, &s)[0].contains("DEFAULT 0"));
        s.columns[1].default = "CURRENT_TIMESTAMP".into();
        assert!(preview(Engine::Sqlite, &s)[0].contains("DEFAULT CURRENT_TIMESTAMP"));
        s.columns[1].default = "it's".into();
        assert!(preview(Engine::Sqlite, &s)[0].contains("DEFAULT 'it''s'"));
    }

    #[test]
    fn empty_name_or_columns_produces_no_statements() {
        let mut s = state_new("  ");
        assert!(preview(Engine::Sqlite, &s).is_empty());
        s.name = "t".into();
        s.columns.clear();
        assert!(preview(Engine::Sqlite, &s).is_empty());
    }

    fn editing() -> (DesignerState, Vec<ColRow>) {
        let orig = vec![
            ColRow {
                name: "id".into(),
                ty: "INTEGER".into(),
                nullable: false,
                default: String::new(),
                pk: true,
                comment: String::new(),
            },
            ColRow {
                name: "vieja".into(),
                ty: "TEXT".into(),
                nullable: true,
                default: String::new(),
                pk: false,
                comment: String::new(),
            },
        ];
        let mut s = DesignerState::from_table(
            ConnectionId::nil(),
            ObjectRef {
                connection: ConnectionId::nil(),
                path: vec!["demo.db".into(), "pedidos".into()],
            },
            dbstudio_drivers::TableDetail {
                columns: orig
                    .iter()
                    .map(|c| ColumnMeta {
                        name: c.name.clone(),
                        data_type: dbstudio_core::DataType::new(
                            &c.ty,
                            dbstudio_core::TypeFamily::Other,
                        ),
                        nullable: c.nullable,
                        comment: None,
                    })
                    .collect(),
                primary_keys: vec!["id".into()],
                foreign_keys: Vec::new(),
            },
        );
        s.columns = vec![
            ColRow {
                name: "id".into(),
                ty: "INTEGER".into(),
                nullable: false,
                default: String::new(),
                pk: true,
                comment: String::new(),
            },
            ColRow {
                name: "nueva".into(),
                ty: "TEXT".into(),
                nullable: true,
                default: String::new(),
                pk: false,
                comment: String::new(),
            },
        ];
        (s, orig)
    }

    #[test]
    fn diff_emits_add_and_drop_with_rename_warning() {
        let (s, _orig) = editing();
        let stmts = preview(Engine::Sqlite, &s);
        assert!(stmts[0].starts_with("-- AVISO"), "rename warning first: {}", stmts[0]);
        assert!(
            stmts.iter().any(|s| s.contains("ADD COLUMN \"nueva\"")),
            "{stmts:?}"
        );
        assert!(stmts.iter().any(|s| s.contains("DROP COLUMN \"vieja\"")));
        assert!(
            stmts.iter().all(|s| !s.contains("MODIFY")),
            "sqlite cannot modify"
        );
        let no_pk_note = stmts.iter().any(|s| s.contains("PRIMARY KEY"));
        assert!(!no_pk_note, "pk set unchanged: {stmts:?}");
    }

    #[test]
    fn mysql_modify_includes_default_warning() {
        let (mut s, _orig) = editing();
        s.columns[0].ty = "BIGINT".into();
        let stmts = preview(Engine::Mysql, &s);
        let modify = stmts
            .iter()
            .find(|s| s.contains("MODIFY COLUMN"))
            .expect("mysql modify");
        assert!(modify.contains("`id` BIGINT NOT NULL"));
        assert!(modify.contains("revisa si tenía DEFAULT"));
    }

    #[test]
    fn pg_modify_splits_type_and_nullability() {
        let (mut s, _orig) = editing();
        s.columns[0].ty = "BIGINT".into();
        s.columns[1].nullable = false; // "nueva" exists in new snapshot only…
        s.columns[1].name = "vieja".into(); // keep it matched to the original
        s.columns[1].ty = "TEXT".into();
        let stmts = preview(Engine::Postgres, &s);
        assert!(
            stmts.iter().any(|s| s.contains("ALTER COLUMN \"id\" TYPE BIGINT USING")),
            "{stmts:?}"
        );
        assert!(
            stmts.iter().any(|s| s.contains("ALTER COLUMN \"vieja\" SET NOT NULL")),
            "{stmts:?}"
        );
    }
}
