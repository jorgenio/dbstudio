use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::value::CellValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeFamily {
    Bool,
    Int,
    UInt,
    Float,
    Decimal,
    Text,
    Bytes,
    DateTime,
    Json,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataType {
    /// Engine-reported type name, e.g. `"character varying"`.
    pub name: String,
    pub family: TypeFamily,
}

impl DataType {
    pub fn new(name: impl Into<String>, family: TypeFamily) -> Self {
        Self {
            name: name.into(),
            family,
        }
    }

    pub fn other(name: impl Into<String>) -> Self {
        Self::new(name, TypeFamily::Other)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<CellValue>>,
    /// Rows affected for statements without a result set.
    pub affected: Option<u64>,
    /// True when the driver stopped fetching at `max_rows`.
    pub truncated: bool,
    pub elapsed: Duration,
}

impl QueryResult {
    pub fn statement(affected: u64, elapsed: Duration) -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            affected: Some(affected),
            truncated: false,
            elapsed,
        }
    }

    pub fn is_rows(&self) -> bool {
        !self.columns.is_empty()
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    pub sql: String,
    /// Hard cap on returned rows; the result is marked truncated beyond it.
    pub max_rows: usize,
    /// Optional object the statement refers to (used by MongoDB to resolve
    /// the target collection for bare filters).
    pub context: Option<String>,
}

impl ExecuteRequest {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            max_rows: 10_000,
            context: None,
        }
    }

    pub fn with_max_rows(mut self, max_rows: usize) -> Self {
        self.max_rows = max_rows;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_results_are_not_rows() {
        let r = QueryResult::statement(3, Duration::from_millis(1));
        assert!(!r.is_rows());
        assert_eq!(r.affected, Some(3));
    }
}
