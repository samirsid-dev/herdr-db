//! `herdr-db admin readonly-role`: SQL script creating per-person read-only
//! roles. The plugin prints it and never runs it: the admin reviews, applies
//! and versions it with the infrastructure.

use crate::model::Engine;
use crate::sql::{display_ident, quote_ident, quote_literal};

pub const GROUP_ROLE: &str = "herdr_readonly";

pub struct RoleScriptInput<'a> {
    pub engine: Engine,
    pub source_id: &'a str,
    pub database: &'a str,
    pub schemas: &'a [String],
    /// One login role per person. Empty: a placeholder is used.
    pub users: &'a [String],
    /// Role that runs migrations (PostgreSQL default privileges). `None`: placeholder.
    pub owner: Option<&'a str>,
}

pub fn readonly_role_script(input: &RoleScriptInput<'_>) -> String {
    let placeholder = ["prenom_ro".to_string()];
    let users = if input.users.is_empty() { &placeholder[..] } else { input.users };
    match input.engine {
        Engine::Postgres => postgres(input, users),
        Engine::MySql => mysql(input, users),
    }
}

fn postgres(input: &RoleScriptInput<'_>, users: &[String]) -> String {
    let pg = Engine::Postgres;
    let group = GROUP_ROLE;
    let owner = input.owner.map(|o| display_ident(pg, o));
    let mut out = format!(
        "-- Rôles en lecture seule pour la source `{}` (PostgreSQL, base {}).\n\
         -- Généré par herdr-db : relire, appliquer avec un rôle admin, versionner avec l'infra.\n\
         -- Remplacer chaque mot de passe ; ne jamais le commiter.\n\n\
         BEGIN;\n\n\
         CREATE ROLE {group} NOLOGIN;\n\
         GRANT CONNECT ON DATABASE {} TO {group};\n",
        input.source_id,
        input.database,
        display_ident(pg, input.database),
    );
    for schema in input.schemas {
        let s = display_ident(pg, schema);
        out.push_str(&format!(
            "\nGRANT USAGE ON SCHEMA {s} TO {group};\n\
             GRANT SELECT ON ALL TABLES IN SCHEMA {s} TO {group};\n\
             GRANT SELECT ON ALL SEQUENCES IN SCHEMA {s} TO {group};\n"
        ));
        match &owner {
            Some(owner) => out.push_str(&format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {owner} IN SCHEMA {s} GRANT SELECT ON TABLES TO {group};\n\
                 ALTER DEFAULT PRIVILEGES FOR ROLE {owner} IN SCHEMA {s} GRANT SELECT ON SEQUENCES TO {group};\n"
            )),
            None => out.push_str(&format!(
                "-- Remplacer app_owner par le rôle qui exécute les migrations (--owner) :\n\
                 ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA {s} GRANT SELECT ON TABLES TO {group};\n\
                 ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA {s} GRANT SELECT ON SEQUENCES TO {group};\n"
            )),
        }
    }
    out.push('\n');
    for user in users {
        let u = display_ident(pg, user);
        out.push_str(&format!(
            "CREATE ROLE {u} LOGIN PASSWORD {} IN ROLE {group};\n\
             ALTER ROLE {u} SET default_transaction_read_only = on;\n",
            quote_literal(pg, "à-remplacer")
        ));
    }
    out.push_str("\nCOMMIT;\n");
    out
}

fn mysql(input: &RoleScriptInput<'_>, users: &[String]) -> String {
    let my = Engine::MySql;
    let group = quote_literal(my, GROUP_ROLE);
    let mut out = format!(
        "-- Rôles en lecture seule pour la source `{}` (MySQL 8).\n\
         -- Généré par herdr-db : relire, appliquer avec un compte admin, versionner avec l'infra.\n\
         -- Les droits au niveau base couvrent aussi les tables créées plus tard.\n\
         -- Remplacer chaque mot de passe ; ne jamais le commiter.\n\n\
         CREATE ROLE IF NOT EXISTS {group};\n",
        input.source_id
    );
    let mut databases: Vec<&str> = input.schemas.iter().map(String::as_str).collect();
    if databases.is_empty() {
        databases.push(input.database);
    }
    for database in databases {
        out.push_str(&format!("GRANT SELECT, SHOW VIEW ON {}.* TO {group};\n", quote_ident(my, database)));
    }
    out.push('\n');
    for user in users {
        let u = quote_literal(my, user);
        out.push_str(&format!(
            "CREATE USER IF NOT EXISTS {u}@'%' IDENTIFIED BY {} DEFAULT ROLE {group};\n\
             GRANT {group} TO {u}@'%';\n",
            quote_literal(my, "à-remplacer")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_script_covers_future_tables() {
        let schemas = vec!["public".to_string()];
        let users = vec!["samir_ro".to_string()];
        let script = readonly_role_script(&RoleScriptInput {
            engine: Engine::Postgres,
            source_id: "db_prod",
            database: "app",
            schemas: &schemas,
            users: &users,
            owner: Some("app"),
        });
        assert!(script.contains("CREATE ROLE herdr_readonly NOLOGIN;"));
        assert!(script.contains("GRANT SELECT ON ALL TABLES IN SCHEMA public TO herdr_readonly;"));
        assert!(script.contains(
            "ALTER DEFAULT PRIVILEGES FOR ROLE app IN SCHEMA public GRANT SELECT ON TABLES TO herdr_readonly;"
        ));
        assert!(script.contains("CREATE ROLE samir_ro LOGIN PASSWORD 'à-remplacer' IN ROLE herdr_readonly;"));
        assert!(script.contains("ALTER ROLE samir_ro SET default_transaction_read_only = on;"));
        assert!(script.trim_end().ends_with("COMMIT;"));
    }

    #[test]
    fn mysql_script_grants_per_database() {
        let schemas = vec!["platform".to_string()];
        let script = readonly_role_script(&RoleScriptInput {
            engine: Engine::MySql,
            source_id: "hc-platform",
            database: "platform",
            schemas: &schemas,
            users: &[],
            owner: None,
        });
        assert!(script.contains("GRANT SELECT, SHOW VIEW ON `platform`.* TO 'herdr_readonly';"));
        assert!(script.contains("CREATE USER IF NOT EXISTS 'prenom_ro'@'%'"));
    }
}
