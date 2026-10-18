//! Structure synchronization: compare a source table (desired structure)
//! against a target table and produce a Navicat-style action grid plus the
//! DDL that migrates the target to match.
//!
//! The migration itself reuses the designer's diff engine, so every engine
//! dialect (SQLite comments, MySQL `MODIFY`, Postgres `TYPE ... USING`,
//! PK change warnings) stays consistent with what "Aplicar" runs.

use dbstudio_core::{ColumnMeta, Engine, ObjectRef};
use dbstudio_drivers::TableDetail;

use crate::designer::{preview, DesignerState};

/// What the migration would do to one target column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Add,
    Drop,
    Modify,
    Keep,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Add => "Añadir",
            Action::Drop => "Eliminar",
            Action::Modify => "Modificar",
            Action::Keep => "Sin cambios",
        }
    }
}

/// One row of the comparison grid.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncRow {
    pub column: String,
    pub action: Action,
    /// Source side summary (`—` when the column does not exist there).
    pub source: String,
    /// Target side summary (`—` when the column does not exist there).
    pub target: String,
}

/// Comparison result: grid rows and the statements to run on the target.
/// Comment-only lines are included; callers filter what they execute.
pub struct Plan {
    pub rows: Vec<SyncRow>,
    pub statements: Vec<String>,
}

/// Compare `source` (desired) with `target` (`None` = table is missing in
/// the destination, so the whole structure is created) at `target_ref`.
pub fn plan(
    engine: Engine,
    source: &TableDetail,
    target: Option<&TableDetail>,
    target_ref: &ObjectRef,
) -> Plan {
    let state = DesignerState::for_sync(target_ref.connection, target_ref.clone(), source, target);
    let statements = preview(engine, &state);
    let rows = grid(source, target);
    Plan { rows, statements }
}

/// Grid rows: union of both column lists, matched case-insensitively.
fn grid(source: &TableDetail, target: Option<&TableDetail>) -> Vec<SyncRow> {
    let empty = TableDetail::default();
    let target = target.unwrap_or(&empty);
    let mut rows = Vec::new();

    for s in &source.columns {
        match find(&target.columns, &s.name) {
            None => rows.push(SyncRow {
                column: s.name.clone(),
                action: Action::Add,
                source: summary(s),
                target: "—".to_string(),
            }),
            Some(t) => {
                let changed = !s.data_type.name.eq_ignore_ascii_case(&t.data_type.name)
                    || s.nullable != t.nullable
                    || s.comment != t.comment;
                rows.push(SyncRow {
                    column: s.name.clone(),
                    action: if changed {
                        Action::Modify
                    } else {
                        Action::Keep
                    },
                    source: summary(s),
                    target: summary(t),
                });
            }
        }
    }
    for t in &target.columns {
        if find(&source.columns, &t.name).is_none() {
            rows.push(SyncRow {
                column: t.name.clone(),
                action: Action::Drop,
                source: "—".to_string(),
                target: summary(t),
            });
        }
    }
    rows
}

fn find<'a>(cols: &'a [ColumnMeta], name: &str) -> Option<&'a ColumnMeta> {
    cols.iter().find(|c| c.name.eq_ignore_ascii_case(name))
}

fn summary(c: &ColumnMeta) -> String {
    format!(
        "{} {}",
        c.data_type.name,
        if c.nullable { "NULL" } else { "NOT NULL" }
    )
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ConnectionId, DataType, TypeFamily};

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            data_type: DataType::new(ty, TypeFamily::Other),
            nullable,
            comment: None,
        }
    }

    fn detail(cols: Vec<ColumnMeta>, pks: Vec<&str>) -> TableDetail {
        TableDetail {
            columns: cols,
            primary_keys: pks.into_iter().map(str::to_string).collect(),
            foreign_keys: Vec::new(),
        }
    }

    fn target_ref() -> ObjectRef {
        ObjectRef {
            connection: ConnectionId::nil(),
            path: vec!["demo.db".into(), "pedidos".into()],
        }
    }

    #[test]
    fn grid_reports_add_drop_modify_and_keep() {
        let source = detail(
            vec![col("id", "INTEGER", false), col("nueva", "TEXT", true)],
            vec!["id"],
        );
        let target = detail(
            vec![col("id", "INTEGER", false), col("vieja", "TEXT", true)],
            vec!["id"],
        );
        let rows = grid(&source, Some(&target));
        let action = |name: &str| {
            rows.iter()
                .find(|r| r.column == name)
                .map(|r| r.action)
                .expect("row")
        };
        assert_eq!(action("id"), Action::Keep);
        assert_eq!(action("nueva"), Action::Add);
        assert_eq!(action("vieja"), Action::Drop);
    }

    #[test]
    fn type_or_nullability_changes_are_modify() {
        let source = detail(vec![col("total", "NUMERIC", false)], vec![]);
        let target = detail(vec![col("total", "INTEGER", true)], vec![]);
        let rows = grid(&source, Some(&target));
        assert_eq!(rows[0].action, Action::Modify);
        assert_eq!(rows[0].source, "NUMERIC NOT NULL");
        assert_eq!(rows[0].target, "INTEGER NULL");
    }

    #[test]
    fn missing_target_creates_the_source_table() {
        let source = detail(vec![col("id", "INTEGER", false)], vec!["id"]);
        let plan = plan(Engine::Sqlite, &source, None, &target_ref());
        assert_eq!(plan.rows.len(), 1);
        assert_eq!(plan.rows[0].action, Action::Add);
        assert_eq!(plan.statements.len(), 1, "{}", plan.statements.join("\n"));
        assert!(
            plan.statements[0].starts_with("CREATE TABLE \"pedidos\""),
            "{}",
            plan.statements[0]
        );
    }

    #[test]
    fn existing_target_diffs_with_alter_statements() {
        let source = detail(
            vec![col("id", "INTEGER", false), col("nueva", "TEXT", true)],
            vec!["id"],
        );
        let target = detail(vec![col("id", "INTEGER", false)], vec!["id"]);
        let plan = plan(Engine::Postgres, &source, Some(&target), &target_ref());
        assert!(
            plan.statements.iter().any(|s| s.contains("ADD COLUMN")),
            "{}",
            plan.statements.join("\n")
        );
        assert!(!plan.statements[0].starts_with("CREATE"));
    }

    #[test]
    fn extra_target_column_is_dropped() {
        let source = detail(vec![col("id", "INTEGER", false)], vec!["id"]);
        let target = detail(
            vec![col("id", "INTEGER", false), col("sobra", "TEXT", true)],
            vec!["id"],
        );
        let plan = plan(Engine::Postgres, &source, Some(&target), &target_ref());
        assert!(
            plan.statements.iter().any(|s| s.contains("DROP COLUMN \"sobra\"")),
            "{}",
            plan.statements.join("\n")
        );
    }

    #[test]
    fn no_differences_produce_no_runnable_statements() {
        let d = detail(vec![col("id", "INTEGER", false)], vec!["id"]);
        let plan = plan(Engine::Sqlite, &d, Some(&d), &target_ref());
        assert!(plan
            .statements
            .iter()
            .all(|s| s.trim().is_empty() || s.trim_start().starts_with("--")));
        assert!(plan.rows.iter().all(|r| r.action == Action::Keep));
    }
}
