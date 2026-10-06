//! Identifier and literal quoting per engine.

use crate::model::Engine;

/// Always quotes: used for generated SQL.
pub fn quote_ident(engine: Engine, ident: &str) -> String {
    match engine {
        Engine::Postgres => format!("\"{}\"", ident.replace('"', "\"\"")),
        Engine::MySql => format!("`{}`", ident.replace('`', "``")),
    }
}

/// Quotes only when the engine would not read the identifier back unchanged:
/// used for display and Copy Reference.
pub fn display_ident(engine: Engine, ident: &str) -> String {
    if needs_quotes(engine, ident) { quote_ident(engine, ident) } else { ident.to_string() }
}

pub fn needs_quotes(engine: Engine, ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return true;
    };
    let plain = match engine {
        // Unquoted identifiers fold to lower case on PostgreSQL.
        Engine::Postgres => {
            (first.is_ascii_lowercase() || first == '_')
                && ident.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
        }
        Engine::MySql => {
            (first.is_ascii_alphabetic() || first == '_' || first == '$')
                && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
                && !ident.chars().all(|c| c.is_ascii_digit())
        }
    };
    !plain || is_reserved(engine, ident)
}

pub fn qualified(engine: Engine, schema: &str, name: &str) -> String {
    format!("{}.{}", display_ident(engine, schema), display_ident(engine, name))
}

pub fn qualified_quoted(engine: Engine, schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(engine, schema), quote_ident(engine, name))
}

pub fn quote_literal(engine: Engine, value: &str) -> String {
    match engine {
        // standard_conforming_strings is on since PostgreSQL 9.1.
        Engine::Postgres => format!("'{}'", value.replace('\'', "''")),
        Engine::MySql => format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''")),
    }
}

fn is_reserved(engine: Engine, ident: &str) -> bool {
    let upper = ident.to_ascii_uppercase();
    let list: &[&str] = match engine {
        Engine::Postgres => POSTGRES_RESERVED,
        Engine::MySql => MYSQL_RESERVED,
    };
    list.binary_search(&upper.as_str()).is_ok()
}

/// PostgreSQL reserved key words (including "requires AS"), sorted.
const POSTGRES_RESERVED: &[&str] = &[
    "ALL",
    "ANALYSE",
    "ANALYZE",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "ASYMMETRIC",
    "AUTHORIZATION",
    "BINARY",
    "BOTH",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLLATION",
    "COLUMN",
    "CONCURRENTLY",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_CATALOG",
    "CURRENT_DATE",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DEFAULT",
    "DEFERRABLE",
    "DESC",
    "DISTINCT",
    "DO",
    "ELSE",
    "END",
    "EXCEPT",
    "FALSE",
    "FETCH",
    "FOR",
    "FOREIGN",
    "FREEZE",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INITIALLY",
    "INNER",
    "INTERSECT",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "LATERAL",
    "LEADING",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "NATURAL",
    "NOT",
    "NOTNULL",
    "NULL",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVERLAPS",
    "PLACING",
    "PRIMARY",
    "REFERENCES",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SESSION_USER",
    "SIMILAR",
    "SOME",
    "SYMMETRIC",
    "SYSTEM_USER",
    "TABLE",
    "TABLESAMPLE",
    "THEN",
    "TO",
    "TRAILING",
    "TRUE",
    "UNION",
    "UNIQUE",
    "USER",
    "USING",
    "VARIADIC",
    "VERBOSE",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// MySQL 8 reserved words most likely to appear as identifiers, sorted.
const MYSQL_RESERVED: &[&str] = &[
    "ACCESSIBLE",
    "ADD",
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "BEFORE",
    "BETWEEN",
    "BIGINT",
    "BINARY",
    "BLOB",
    "BOTH",
    "BY",
    "CALL",
    "CASCADE",
    "CASE",
    "CHANGE",
    "CHAR",
    "CHARACTER",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "CONDITION",
    "CONSTRAINT",
    "CONTINUE",
    "CONVERT",
    "CREATE",
    "CROSS",
    "CUBE",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "CURSOR",
    "DATABASE",
    "DATABASES",
    "DECIMAL",
    "DECLARE",
    "DEFAULT",
    "DELAYED",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DIV",
    "DOUBLE",
    "DROP",
    "DUAL",
    "EACH",
    "ELSE",
    "ELSEIF",
    "EMPTY",
    "ENCLOSED",
    "ESCAPED",
    "EXCEPT",
    "EXISTS",
    "EXIT",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FLOAT",
    "FOR",
    "FORCE",
    "FOREIGN",
    "FROM",
    "FULLTEXT",
    "FUNCTION",
    "GENERATED",
    "GET",
    "GRANT",
    "GROUP",
    "GROUPING",
    "GROUPS",
    "HAVING",
    "IF",
    "IGNORE",
    "IN",
    "INDEX",
    "INFILE",
    "INNER",
    "INOUT",
    "INSERT",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "ITERATE",
    "JOIN",
    "KEY",
    "KEYS",
    "KILL",
    "LATERAL",
    "LEADING",
    "LEAVE",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LINES",
    "LOAD",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "LOCK",
    "LONG",
    "LOOP",
    "MATCH",
    "MOD",
    "MODIFIES",
    "NATURAL",
    "NOT",
    "NULL",
    "NUMERIC",
    "OF",
    "ON",
    "OPTION",
    "OR",
    "ORDER",
    "OUT",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRECISION",
    "PRIMARY",
    "PROCEDURE",
    "RANGE",
    "RANK",
    "READ",
    "READS",
    "REAL",
    "RECURSIVE",
    "REFERENCES",
    "REGEXP",
    "RELEASE",
    "RENAME",
    "REPEAT",
    "REPLACE",
    "REQUIRE",
    "RESIGNAL",
    "RESTRICT",
    "RETURN",
    "REVOKE",
    "RIGHT",
    "RLIKE",
    "ROW",
    "ROWS",
    "SCHEMA",
    "SCHEMAS",
    "SELECT",
    "SET",
    "SHOW",
    "SIGNAL",
    "SMALLINT",
    "SPATIAL",
    "SQL",
    "STARTING",
    "STORED",
    "SYSTEM",
    "TABLE",
    "TERMINATED",
    "THEN",
    "TO",
    "TRAILING",
    "TRIGGER",
    "TRUE",
    "UNDO",
    "UNION",
    "UNIQUE",
    "UNLOCK",
    "UNSIGNED",
    "UPDATE",
    "USAGE",
    "USE",
    "USING",
    "VALUES",
    "VARCHAR",
    "VIRTUAL",
    "WHEN",
    "WHERE",
    "WHILE",
    "WINDOW",
    "WITH",
    "WRITE",
    "XOR",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_lists_are_sorted() {
        for list in [POSTGRES_RESERVED, MYSQL_RESERVED] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            assert_eq!(sorted, list);
        }
    }

    #[test]
    fn postgres_quotes_case_spaces_and_keywords() {
        let pg = Engine::Postgres;
        assert_eq!(display_ident(pg, "users"), "users");
        assert_eq!(display_ident(pg, "Users"), "\"Users\"");
        assert_eq!(display_ident(pg, "order"), "\"order\"");
        assert_eq!(display_ident(pg, "user"), "\"user\"");
        assert_eq!(display_ident(pg, "my table"), "\"my table\"");
        assert_eq!(display_ident(pg, "a\"b"), "\"a\"\"b\"");
        assert_eq!(qualified(pg, "public", "Order Lines"), "public.\"Order Lines\"");
    }

    #[test]
    fn mysql_uses_backticks() {
        let my = Engine::MySql;
        assert_eq!(display_ident(my, "Users"), "Users");
        assert_eq!(display_ident(my, "order"), "`order`");
        assert_eq!(display_ident(my, "a`b"), "`a``b`");
        assert_eq!(display_ident(my, "123"), "`123`");
        assert_eq!(qualified_quoted(my, "platform", "users"), "`platform`.`users`");
    }

    #[test]
    fn literals_escape_quotes() {
        assert_eq!(quote_literal(Engine::Postgres, "it's"), "'it''s'");
        assert_eq!(quote_literal(Engine::Postgres, "a\\b"), "'a\\b'");
        assert_eq!(quote_literal(Engine::MySql, "a\\'b"), "'a\\\\''b'");
    }
}
