//! Display values. The grid and the console never handle Rust types per
//! column: drivers turn every value into a [`Cell`], which is what makes an
//! arbitrary `SELECT` displayable without knowing its shape in advance.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cell {
    Null,
    Text(String),
    Json(String),
    Binary { len: usize },
}

impl Cell {
    /// Builds a cell from the engine's text representation of a value.
    pub fn from_text(type_name: Option<&str>, raw: String) -> Cell {
        match type_name.map(str::to_ascii_lowercase).as_deref() {
            Some("json" | "jsonb") => Cell::Json(raw),
            // PostgreSQL text format for bytea is hex: \x0102...
            Some("bytea") if raw.starts_with("\\x") => Cell::Binary { len: (raw.len() - 2) / 2 },
            _ => Cell::Text(raw),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Cell::Null)
    }

    /// Single-line preview, at most `max` characters.
    pub fn preview(&self, max: usize) -> String {
        match self {
            Cell::Null => "<null>".to_string(),
            Cell::Binary { len } => format!("<binary {len} B>"),
            Cell::Text(s) | Cell::Json(s) => truncate_line(s, max),
        }
    }

    /// Value copied to the clipboard; `None` for NULL.
    pub fn copy_text(&self) -> Option<String> {
        match self {
            Cell::Null => None,
            Cell::Text(s) | Cell::Json(s) => Some(s.clone()),
            Cell::Binary { len } => Some(format!("<binary {len} B>")),
        }
    }

    /// Full value for the row inspector; JSON is pretty-printed.
    pub fn full_text(&self) -> String {
        match self {
            Cell::Json(s) => serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| s.clone()),
            other => other.copy_text().unwrap_or_else(|| "<null>".to_string()),
        }
    }
}

/// Collapses whitespace runs (newlines, tabs) and truncates with an ellipsis.
pub fn truncate_line(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max + 3));
    let mut count = 0;
    let mut last_space = false;
    for c in s.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if c == ' ' && last_space {
            continue;
        }
        last_space = c == ' ';
        if count == max {
            out.pop();
            out.push('…');
            return out;
        }
        out.push(c);
        count += 1;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultColumn {
    pub name: String,
    /// SQL type when the driver knows it, used for alignment and viewers.
    pub type_name: Option<String>,
}

impl ResultColumn {
    pub fn new(name: impl Into<String>, type_name: Option<String>) -> Self {
        Self { name: name.into(), type_name }
    }

    pub fn is_numeric(&self) -> bool {
        let Some(t) = self.type_name.as_deref() else {
            return false;
        };
        let t = t.to_ascii_lowercase();
        ["int", "serial", "numeric", "decimal", "real", "double", "float", "money", "oid"].iter().any(|k| t.contains(k))
            && !t.contains("interval")
            && !t.contains("point")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RowSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Cell>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub rows: RowSet,
    /// More rows exist after this page (the driver fetched one extra row).
    pub has_more: bool,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatementOutcome {
    Rows { set: RowSet, truncated: bool },
    Command { tag: String, affected: Option<u64> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryOutcome {
    pub statements: Vec<StatementOutcome>,
    pub elapsed: Duration,
}

// ---------------------------------------------------------------------------
// Export

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// CSV with a header line. NULL becomes an empty field.
pub fn to_csv(columns: &[ResultColumn], rows: &[&[Cell]]) -> String {
    let mut out = columns.iter().map(|c| csv_field(&c.name)).collect::<Vec<_>>().join(",");
    out.push('\n');
    for row in rows {
        let line =
            row.iter().map(|cell| csv_field(&cell.copy_text().unwrap_or_default())).collect::<Vec<_>>().join(",");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// JSON array of objects. JSON cells are embedded as values, NULL as null.
pub fn to_json(columns: &[ResultColumn], rows: &[&[Cell]]) -> String {
    let values: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let mut object = serde_json::Map::new();
            for (column, cell) in columns.iter().zip(row.iter()) {
                let value = match cell {
                    Cell::Null => serde_json::Value::Null,
                    Cell::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.clone())),
                    Cell::Text(s) => serde_json::Value::String(s.clone()),
                    Cell::Binary { len } => serde_json::Value::String(format!("<binary {len} B>")),
                };
                object.insert(column.name.clone(), value);
            }
            serde_json::Value::Object(object)
        })
        .collect();
    serde_json::to_string_pretty(&values).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_empty_string_differ() {
        assert_eq!(Cell::Null.preview(10), "<null>");
        assert_eq!(Cell::Text(String::new()).preview(10), "");
        assert_eq!(Cell::Null.copy_text(), None);
        assert_eq!(Cell::Text(String::new()).copy_text(), Some(String::new()));
    }

    #[test]
    fn preview_collapses_and_truncates() {
        assert_eq!(truncate_line("a\n\n  b", 10), "a b");
        assert_eq!(truncate_line("abcdef", 4), "abc…");
        assert_eq!(truncate_line("abcd", 4), "abcd");
    }

    #[test]
    fn typed_cells() {
        assert_eq!(Cell::from_text(Some("jsonb"), "{}".into()), Cell::Json("{}".into()));
        assert_eq!(Cell::from_text(Some("bytea"), "\\x0102".into()), Cell::Binary { len: 2 });
        assert_eq!(Cell::from_text(None, "x".into()), Cell::Text("x".into()));
        assert!(Cell::Json("{\"a\":1}".into()).full_text().contains("\"a\": 1"));
    }

    #[test]
    fn export_formats() {
        let columns = vec![ResultColumn::new("id", None), ResultColumn::new("data", Some("json".into()))];
        let row = vec![Cell::Text("1".into()), Cell::Json("{\"k\":\"v, w\"}".into())];
        let null_row = vec![Cell::Text("2".into()), Cell::Null];
        let rows = [row.as_slice(), null_row.as_slice()];
        assert_eq!(to_csv(&columns, &rows), "id,data\n1,\"{\"\"k\"\":\"\"v, w\"\"}\"\n2,\n");
        let json: serde_json::Value = serde_json::from_str(&to_json(&columns, &rows)).unwrap();
        assert_eq!(json[0]["data"]["k"], "v, w");
        assert!(json[1]["data"].is_null());
    }

    #[test]
    fn numeric_detection() {
        assert!(ResultColumn::new("a", Some("bigint".into())).is_numeric());
        assert!(ResultColumn::new("a", Some("numeric(10,2)".into())).is_numeric());
        assert!(!ResultColumn::new("a", Some("interval".into())).is_numeric());
        assert!(!ResultColumn::new("a", Some("text".into())).is_numeric());
    }
}
