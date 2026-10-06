//! Server-side pagination for the data grid. Keyset pagination on the primary
//! key when the order is stable (no sort, or a sort on a single-column key),
//! `OFFSET` otherwise. Pages fetch one extra row to know whether more follow.

use crate::model::{Engine, ObjectRef};
use crate::sql::{qualified_quoted, quote_ident, quote_literal};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    fn sql(self) -> &'static str {
        match self {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        }
    }

    fn flip(self) -> SortDir {
        match self {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    pub column: String,
    pub dir: SortDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Position {
    First,
    /// `OFFSET` mode: page starting at this row.
    Offset(u64),
    /// Keyset mode: rows after the row with these key values.
    After(Vec<String>),
    /// Keyset mode: rows before the row with these key values.
    Before(Vec<String>),
    /// Keyset mode: the last page.
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub engine: Engine,
    pub object: ObjectRef,
    /// Free `WHERE` clause typed by the user, without the keyword.
    pub filter: Option<String>,
    pub sort: Option<Sort>,
    pub primary_key: Vec<String>,
    pub page_size: usize,
    pub position: Position,
    /// Column name -> SQL type, from the cached table detail. Lets drivers
    /// that only return text (PostgreSQL simple protocol) type the cells.
    pub column_types: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageQuery {
    pub sql: String,
    /// Rows come back in reverse display order and must be reversed.
    pub reversed: bool,
    pub keyset: bool,
}

impl PageRequest {
    pub fn keyset_applicable(&self) -> bool {
        !self.primary_key.is_empty()
            && match &self.sort {
                None => true,
                Some(sort) => self.primary_key.len() == 1 && sort.column == self.primary_key[0],
            }
    }

    pub fn to_sql(&self) -> PageQuery {
        let engine = self.engine;
        let table = qualified_quoted(engine, &self.object.schema, &self.object.name);
        let limit = self.page_size + 1;
        let mut conditions: Vec<String> = Vec::new();
        if let Some(filter) = self.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            conditions.push(format!("({filter})"));
        }

        if self.keyset_applicable() {
            let dir = self.sort.as_ref().map_or(SortDir::Asc, |s| s.dir);
            let key_list = self.primary_key.iter().map(|c| quote_ident(engine, c)).collect::<Vec<_>>();
            let tuple = |values: &[String]| -> (String, String) {
                let literals = values.iter().map(|v| quote_literal(engine, v)).collect::<Vec<_>>();
                if key_list.len() == 1 {
                    (key_list[0].clone(), literals.join(""))
                } else {
                    (format!("({})", key_list.join(", ")), format!("({})", literals.join(", ")))
                }
            };
            let (order_dir, reversed, offset) = match &self.position {
                Position::First => (dir, false, None),
                Position::Offset(n) => (dir, false, Some(*n)),
                Position::After(values) => {
                    let (lhs, rhs) = tuple(values);
                    let op = if dir == SortDir::Asc { ">" } else { "<" };
                    conditions.push(format!("{lhs} {op} {rhs}"));
                    (dir, false, None)
                }
                Position::Before(values) => {
                    let (lhs, rhs) = tuple(values);
                    let op = if dir == SortDir::Asc { "<" } else { ">" };
                    conditions.push(format!("{lhs} {op} {rhs}"));
                    (dir.flip(), true, None)
                }
                Position::Last => (dir.flip(), true, None),
            };
            let order = key_list.iter().map(|k| format!("{k} {}", order_dir.sql())).collect::<Vec<_>>().join(", ");
            let mut sql = format!("SELECT * FROM {table}");
            push_where(&mut sql, &conditions);
            sql.push_str(&format!(" ORDER BY {order} LIMIT {limit}"));
            if let Some(offset) = offset.filter(|o| *o > 0) {
                sql.push_str(&format!(" OFFSET {offset}"));
            }
            return PageQuery { sql, reversed, keyset: true };
        }

        let offset = match self.position {
            Position::Offset(n) => n,
            _ => 0,
        };
        let mut sql = format!("SELECT * FROM {table}");
        push_where(&mut sql, &conditions);
        if let Some(sort) = &self.sort {
            sql.push_str(&format!(" ORDER BY {} {}", quote_ident(engine, &sort.column), sort.dir.sql()));
            // Tie-break on the key so OFFSET pages stay stable.
            for key in self.primary_key.iter().filter(|k| **k != sort.column) {
                sql.push_str(&format!(", {} ASC", quote_ident(engine, key)));
            }
        } else if !self.primary_key.is_empty() {
            let order = self
                .primary_key
                .iter()
                .map(|k| format!("{} ASC", quote_ident(engine, k)))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" ORDER BY {order}"));
        }
        sql.push_str(&format!(" LIMIT {limit}"));
        if offset > 0 {
            sql.push_str(&format!(" OFFSET {offset}"));
        }
        PageQuery { sql, reversed: false, keyset: false }
    }
}

fn push_where(sql: &mut String, conditions: &[String]) {
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
}

/// Exact count, an explicit user action (the grid shows the catalog estimate).
pub fn count_sql(engine: Engine, object: &ObjectRef, filter: Option<&str>) -> String {
    let mut sql = format!("SELECT COUNT(*) FROM {}", qualified_quoted(engine, &object.schema, &object.name));
    if let Some(filter) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        sql.push_str(&format!(" WHERE ({filter})"));
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(engine: Engine, pk: &[&str], sort: Option<Sort>, position: Position) -> PageRequest {
        PageRequest {
            engine,
            object: ObjectRef::new("s", "public", "users"),
            filter: None,
            sort,
            primary_key: pk.iter().map(|s| s.to_string()).collect(),
            page_size: 100,
            position,
            column_types: BTreeMap::new(),
        }
    }

    #[test]
    fn keyset_first_and_next() {
        let q = request(Engine::Postgres, &["id"], None, Position::First).to_sql();
        assert_eq!(q.sql, r#"SELECT * FROM "public"."users" ORDER BY "id" ASC LIMIT 101"#);
        assert!(q.keyset && !q.reversed);

        let q = request(Engine::Postgres, &["id"], None, Position::After(vec!["42".into()])).to_sql();
        assert_eq!(q.sql, r#"SELECT * FROM "public"."users" WHERE "id" > '42' ORDER BY "id" ASC LIMIT 101"#);
    }

    #[test]
    fn keyset_previous_and_last_are_reversed() {
        let q = request(Engine::Postgres, &["id"], None, Position::Before(vec!["42".into()])).to_sql();
        assert_eq!(q.sql, r#"SELECT * FROM "public"."users" WHERE "id" < '42' ORDER BY "id" DESC LIMIT 101"#);
        assert!(q.reversed);

        let q = request(Engine::Postgres, &["id"], None, Position::Last).to_sql();
        assert_eq!(q.sql, r#"SELECT * FROM "public"."users" ORDER BY "id" DESC LIMIT 101"#);
        assert!(q.reversed);
    }

    #[test]
    fn keyset_descending_sort_on_key() {
        let sort = Sort { column: "id".into(), dir: SortDir::Desc };
        let q = request(Engine::MySql, &["id"], Some(sort), Position::After(vec!["9".into()])).to_sql();
        assert_eq!(q.sql, "SELECT * FROM `public`.`users` WHERE `id` < '9' ORDER BY `id` DESC LIMIT 101");
    }

    #[test]
    fn composite_key_uses_row_comparison() {
        let mut req = request(Engine::Postgres, &["org", "id"], None, Position::After(vec!["a'b".into(), "7".into()]));
        req.filter = Some("active".into());
        assert_eq!(
            req.to_sql().sql,
            r#"SELECT * FROM "public"."users" WHERE (active) AND ("org", "id") > ('a''b', '7') ORDER BY "org" ASC, "id" ASC LIMIT 101"#
        );
    }

    #[test]
    fn sort_on_other_column_falls_back_to_offset() {
        let sort = Sort { column: "name".into(), dir: SortDir::Asc };
        let mut req = request(Engine::Postgres, &["id"], Some(sort), Position::Offset(200));
        req.filter = Some("  ".into());
        let q = req.to_sql();
        assert!(!q.keyset);
        assert_eq!(q.sql, r#"SELECT * FROM "public"."users" ORDER BY "name" ASC, "id" ASC LIMIT 101 OFFSET 200"#);
    }

    #[test]
    fn table_without_key_has_no_order() {
        let q = request(Engine::MySql, &[], None, Position::Offset(0)).to_sql();
        assert_eq!(q.sql, "SELECT * FROM `public`.`users` LIMIT 101");
    }

    #[test]
    fn count() {
        let obj = ObjectRef::new("s", "public", "users");
        assert_eq!(count_sql(Engine::Postgres, &obj, None), r#"SELECT COUNT(*) FROM "public"."users""#);
        assert_eq!(
            count_sql(Engine::MySql, &obj, Some("a = 1")),
            "SELECT COUNT(*) FROM `public`.`users` WHERE (a = 1)"
        );
    }
}
