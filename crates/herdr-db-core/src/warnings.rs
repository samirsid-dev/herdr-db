//! UX warnings shown by the console before running a statement. Never a
//! security boundary: read-only enforcement is the engine's job (session set
//! read-only), these only catch obvious mistakes.

use crate::model::Engine;
use sqlparser::ast::Statement;
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warning {
    UpdateWithoutWhere,
    DeleteWithoutWhere,
    Truncate,
    Drop,
    /// A write on a read-only session: the engine will refuse it.
    WriteOnReadOnly,
}

impl Warning {
    pub fn message(self) -> &'static str {
        match self {
            Warning::UpdateWithoutWhere => "UPDATE sans WHERE : toutes les lignes seront modifiées",
            Warning::DeleteWithoutWhere => "DELETE sans WHERE : toutes les lignes seront supprimées",
            Warning::Truncate => "TRUNCATE vide la table",
            Warning::Drop => "DROP supprime l'objet",
            Warning::WriteOnReadOnly => "Session en lecture seule : le moteur refusera cette écriture",
        }
    }

    /// Requires an explicit confirmation before running.
    pub fn needs_confirmation(self) -> bool {
        !matches!(self, Warning::WriteOnReadOnly)
    }
}

/// Analyzes `sql`. Text the parser does not understand yields no warning.
pub fn analyze(engine: Engine, sql: &str, read_only: bool) -> Vec<Warning> {
    let parsed = match engine {
        Engine::Postgres => Parser::parse_sql(&PostgreSqlDialect {}, sql),
        Engine::MySql => Parser::parse_sql(&MySqlDialect {}, sql),
    };
    let Ok(statements) = parsed else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    let mut push = |w: Warning| {
        if !warnings.contains(&w) {
            warnings.push(w);
        }
    };
    for statement in &statements {
        match statement {
            Statement::Update(update) if update.selection.is_none() => push(Warning::UpdateWithoutWhere),
            Statement::Delete(delete) if delete.selection.is_none() && delete.using.is_none() => {
                push(Warning::DeleteWithoutWhere)
            }
            Statement::Truncate(_) => push(Warning::Truncate),
            Statement::Drop { .. } => push(Warning::Drop),
            _ => {}
        }
        if read_only && is_write(statement) {
            push(Warning::WriteOnReadOnly);
        }
    }
    warnings
}

fn is_write(statement: &Statement) -> bool {
    !matches!(
        statement,
        Statement::Query(_)
            | Statement::Explain { .. }
            | Statement::ExplainTable { .. }
            | Statement::ShowTables { .. }
            | Statement::ShowColumns { .. }
            | Statement::ShowVariable { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowCreate { .. }
            | Statement::ShowDatabases { .. }
            | Statement::ShowSchemas { .. }
            | Statement::ShowStatus { .. }
            | Statement::ShowFunctions { .. }
            | Statement::ShowCollation { .. }
            | Statement::Set(_)
            | Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::Use(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_statements() {
        let pg = Engine::Postgres;
        assert_eq!(analyze(pg, "update t set a = 1", false), vec![Warning::UpdateWithoutWhere]);
        assert_eq!(analyze(pg, "update t set a = 1 where id = 2", false), vec![]);
        assert_eq!(analyze(pg, "delete from t", false), vec![Warning::DeleteWithoutWhere]);
        assert_eq!(analyze(pg, "delete from t where id = 1", false), vec![]);
        assert_eq!(analyze(pg, "truncate t", false), vec![Warning::Truncate]);
        assert_eq!(analyze(pg, "drop table t", false), vec![Warning::Drop]);
        assert_eq!(analyze(Engine::MySql, "DELETE FROM `t`", false), vec![Warning::DeleteWithoutWhere]);
    }

    #[test]
    fn read_only_sessions() {
        let pg = Engine::Postgres;
        assert_eq!(analyze(pg, "select 1", true), vec![]);
        assert_eq!(analyze(pg, "set statement_timeout = 0", true), vec![]);
        assert_eq!(analyze(pg, "insert into t values (1)", true), vec![Warning::WriteOnReadOnly]);
        assert_eq!(analyze(pg, "delete from t", true), vec![Warning::DeleteWithoutWhere, Warning::WriteOnReadOnly]);
    }

    #[test]
    fn unparsable_text_is_silent() {
        assert_eq!(analyze(Engine::Postgres, "vacuum (verbose) some thing ;;", false), vec![]);
    }
}
