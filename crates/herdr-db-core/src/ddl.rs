//! DDL shown by Go to DDL. PostgreSQL has no `SHOW CREATE TABLE`: the
//! statement is rebuilt from the introspected model, using the catalog's own
//! deparsers (`pg_get_constraintdef`, `pg_get_indexdef`...) for the pieces
//! they cover. MySQL returns `SHOW CREATE` output stored at introspection.

use crate::model::{Column, ConstraintKind, Engine, Identity, ObjectKind, ObjectRef, TableDetail};
use crate::sql::{display_ident, qualified, quote_literal};

pub fn object_ddl(engine: Engine, object: &ObjectRef, detail: &TableDetail) -> String {
    match engine {
        Engine::Postgres => postgres_ddl(object, detail),
        Engine::MySql => detail
            .native_ddl
            .as_ref()
            .map(|ddl| format!("{};\n", ddl.trim_end().trim_end_matches(';')))
            .unwrap_or_else(|| fallback_ddl(engine, object, detail)),
    }
}

pub fn postgres_ddl(object: &ObjectRef, detail: &TableDetail) -> String {
    let pg = Engine::Postgres;
    let name = qualified(pg, &object.schema, &object.name);
    let mut out = String::new();

    match detail.kind {
        ObjectKind::View | ObjectKind::MaterializedView => {
            let materialized = detail.kind == ObjectKind::MaterializedView;
            let query = detail.view_definition.as_deref().unwrap_or("").trim().trim_end_matches(';').trim_end();
            out.push_str(&format!("CREATE {}VIEW {name} AS\n{query}", if materialized { "MATERIALIZED " } else { "" }));
            out.push_str(if materialized { "\nWITH DATA;\n" } else { ";\n" });
        }
        _ => {
            for sequence in detail.columns.iter().filter_map(|c| foreign_sequence(object, c)) {
                out.push_str(&format!("CREATE SEQUENCE IF NOT EXISTS {sequence};\n\n"));
            }
            let foreign = if detail.kind == ObjectKind::ForeignTable { "FOREIGN " } else { "" };
            out.push_str(&format!("CREATE {foreign}TABLE {name} (\n"));
            let mut lines: Vec<String> = detail.columns.iter().map(|c| column_line(object, c)).collect();
            let order =
                [ConstraintKind::PrimaryKey, ConstraintKind::Unique, ConstraintKind::Check, ConstraintKind::Exclusion];
            for kind in order {
                for constraint in detail.constraints.iter().filter(|c| c.kind == kind) {
                    if let Some(definition) = &constraint.definition {
                        lines.push(format!("CONSTRAINT {} {definition}", display_ident(pg, &constraint.name)));
                    }
                }
            }
            for fk in &detail.foreign_keys {
                if let Some(definition) = &fk.definition {
                    lines.push(format!("CONSTRAINT {} {definition}", display_ident(pg, &fk.name)));
                }
            }
            out.push_str(&lines.iter().map(|l| format!("    {l}")).collect::<Vec<_>>().join(",\n"));
            out.push_str("\n)");
            if let Some(key) = &detail.partition_key {
                out.push_str(&format!("\nPARTITION BY {key}"));
            }
            out.push_str(";\n");
        }
    }

    let kind_keyword = match detail.kind {
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        ObjectKind::ForeignTable => "FOREIGN TABLE",
        _ => "TABLE",
    };
    let mut comments = Vec::new();
    if let Some(comment) = &detail.comment {
        comments.push(format!("COMMENT ON {kind_keyword} {name} IS {};", quote_literal(pg, comment)));
    }
    for column in &detail.columns {
        if let Some(comment) = &column.comment {
            comments.push(format!(
                "COMMENT ON COLUMN {name}.{} IS {};",
                display_ident(pg, &column.name),
                quote_literal(pg, comment)
            ));
        }
    }
    if !comments.is_empty() {
        out.push('\n');
        out.push_str(&comments.join("\n"));
        out.push('\n');
    }

    let indexes: Vec<&str> =
        detail.indexes.iter().filter(|i| i.constraint.is_none()).filter_map(|i| i.definition.as_deref()).collect();
    if !indexes.is_empty() {
        out.push('\n');
        for definition in indexes {
            out.push_str(definition.trim_end_matches(';'));
            out.push_str(";\n");
        }
    }

    let triggers: Vec<&str> = detail.triggers.iter().filter_map(|t| t.definition.as_deref()).collect();
    if !triggers.is_empty() {
        out.push('\n');
        for definition in triggers {
            out.push_str(definition.trim_end_matches(';'));
            out.push_str(";\n");
        }
    }
    out
}

fn column_line(object: &ObjectRef, column: &Column) -> String {
    let pg = Engine::Postgres;
    let mut line = display_ident(pg, &column.name);
    let serial = serial_type(object, column);
    line.push(' ');
    line.push_str(serial.unwrap_or(&column.data_type));
    if let Some(generated) = &column.generated {
        line.push_str(&format!(
            " GENERATED ALWAYS AS ({}){}",
            strip_outer_parens(&generated.expression),
            if generated.stored { " STORED" } else { " VIRTUAL" }
        ));
    } else if let Some(identity) = column.identity {
        line.push_str(match identity {
            Identity::Always => " GENERATED ALWAYS AS IDENTITY",
            Identity::ByDefault => " GENERATED BY DEFAULT AS IDENTITY",
        });
    } else if serial.is_none()
        && let Some(default) = &column.default
    {
        line.push_str(&format!(" DEFAULT {default}"));
    }
    if !column.nullable {
        line.push_str(" NOT NULL");
    }
    line
}

/// `serial` / `bigserial` when the default is the sequence PostgreSQL itself
/// creates for that column (`<table>_<column>_seq`).
fn serial_type(object: &ObjectRef, column: &Column) -> Option<&'static str> {
    let sequence = nextval_sequence(column.default.as_deref()?)?;
    let expected = format!("{}_{}_seq", object.name, column.name);
    let unqualified = sequence.rsplit('.').next().unwrap_or(&sequence).trim_matches('"');
    if unqualified != expected {
        return None;
    }
    match column.data_type.as_str() {
        "integer" => Some("serial"),
        "bigint" => Some("bigserial"),
        "smallint" => Some("smallserial"),
        _ => None,
    }
}

/// Sequences referenced by a `nextval` default that `serial` would not create.
fn foreign_sequence(object: &ObjectRef, column: &Column) -> Option<String> {
    if serial_type(object, column).is_some() {
        return None;
    }
    nextval_sequence(column.default.as_deref()?)
}

fn nextval_sequence(default: &str) -> Option<String> {
    let rest = default.strip_prefix("nextval('")?;
    let end = rest.find("'::regclass)")?;
    Some(rest[..end].replace("''", "'"))
}

fn strip_outer_parens(expression: &str) -> &str {
    let trimmed = expression.trim();
    if !(trimmed.starts_with('(') && trimmed.ends_with(')')) {
        return trimmed;
    }
    // Only strip when the first parenthesis closes at the very end.
    let mut depth = 0;
    for (i, c) in trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != trimmed.len() - 1 {
                    return trimmed;
                }
            }
            _ => {}
        }
    }
    &trimmed[1..trimmed.len() - 1]
}

/// Minimal DDL when the engine gave none (MySQL object not yet introspected).
fn fallback_ddl(engine: Engine, object: &ObjectRef, detail: &TableDetail) -> String {
    let mut lines: Vec<String> = detail
        .columns
        .iter()
        .map(|c| {
            let mut line = format!("{} {}", display_ident(engine, &c.name), c.data_type);
            if !c.nullable {
                line.push_str(" NOT NULL");
            }
            if let Some(default) = &c.default {
                line.push_str(&format!(" DEFAULT {default}"));
            }
            line
        })
        .collect();
    let pk = detail.primary_key_columns();
    if !pk.is_empty() {
        let cols: Vec<String> = pk.iter().map(|c| display_ident(engine, c)).collect();
        lines.push(format!("PRIMARY KEY ({})", cols.join(", ")));
    }
    format!(
        "CREATE TABLE {} (\n{}\n);\n",
        qualified(engine, &object.schema, &object.name),
        lines.iter().map(|l| format!("    {l}")).collect::<Vec<_>>().join(",\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Constraint, ForeignKey, GeneratedColumn, Index, Trigger};

    fn col(name: &str, ordinal: u32, data_type: &str, nullable: bool, default: Option<&str>) -> Column {
        Column {
            name: name.into(),
            ordinal,
            data_type: data_type.into(),
            nullable,
            default: default.map(Into::into),
            comment: None,
            generated: None,
            identity: None,
            extra: None,
        }
    }

    fn orders() -> TableDetail {
        let mut total = col("total", 4, "numeric(10,2)", true, None);
        total.generated = Some(GeneratedColumn { expression: "(price * (qty)::numeric)".into(), stored: true });
        let mut seq = col("seq", 5, "bigint", false, None);
        seq.identity = Some(Identity::ByDefault);
        let mut note = col("Note", 6, "text", true, Some("''::text"));
        note.comment = Some("free text, l'auteur".into());
        TableDetail {
            kind: ObjectKind::PartitionedTable,
            columns: vec![
                col("id", 1, "integer", false, Some("nextval('public.orders_id_seq'::regclass)")),
                col("price", 2, "numeric(10,2)", false, None),
                col("qty", 3, "integer", false, Some("1")),
                total,
                seq,
                note,
                col("ext", 7, "bigint", true, Some("nextval('public.shared_seq'::regclass)")),
            ],
            indexes: vec![
                Index {
                    name: "orders_pkey".into(),
                    columns: vec!["id".into()],
                    unique: true,
                    primary: true,
                    method: Some("btree".into()),
                    definition: Some("CREATE UNIQUE INDEX orders_pkey ON ONLY public.orders USING btree (id)".into()),
                    constraint: Some("orders_pkey".into()),
                },
                Index {
                    name: "orders_qty_idx".into(),
                    columns: vec!["qty".into()],
                    unique: false,
                    primary: false,
                    method: Some("btree".into()),
                    definition: Some("CREATE INDEX orders_qty_idx ON ONLY public.orders USING btree (qty)".into()),
                    constraint: None,
                },
            ],
            foreign_keys: vec![ForeignKey {
                name: "orders_org_fk".into(),
                columns: vec!["qty".into()],
                ref_schema: "public".into(),
                ref_table: "orgs".into(),
                ref_columns: vec!["id".into()],
                on_update: None,
                on_delete: None,
                definition: Some("FOREIGN KEY (qty) REFERENCES public.orgs(id)".into()),
            }],
            constraints: vec![
                Constraint {
                    name: "positive".into(),
                    kind: ConstraintKind::Check,
                    columns: vec!["price".into()],
                    definition: Some("CHECK ((price > (0)::numeric))".into()),
                },
                Constraint {
                    name: "orders_pkey".into(),
                    kind: ConstraintKind::PrimaryKey,
                    columns: vec!["id".into()],
                    definition: Some("PRIMARY KEY (id)".into()),
                },
            ],
            triggers: vec![Trigger {
                name: "t".into(),
                timing: "BEFORE".into(),
                events: vec!["UPDATE".into()],
                definition: Some(
                    "CREATE TRIGGER t BEFORE UPDATE ON public.orders FOR EACH ROW EXECUTE FUNCTION public.touch()"
                        .into(),
                ),
            }],
            comment: Some("Orders".into()),
            partition_key: Some("RANGE (id)".into()),
            view_definition: None,
            native_ddl: None,
        }
    }

    #[test]
    fn postgres_table_ddl() {
        let ddl = postgres_ddl(&ObjectRef::new("s", "public", "orders"), &orders());
        let expected = r#"CREATE SEQUENCE IF NOT EXISTS public.shared_seq;

CREATE TABLE public.orders (
    id serial NOT NULL,
    price numeric(10,2) NOT NULL,
    qty integer DEFAULT 1 NOT NULL,
    total numeric(10,2) GENERATED ALWAYS AS (price * (qty)::numeric) STORED,
    seq bigint GENERATED BY DEFAULT AS IDENTITY NOT NULL,
    "Note" text DEFAULT ''::text,
    ext bigint DEFAULT nextval('public.shared_seq'::regclass),
    CONSTRAINT orders_pkey PRIMARY KEY (id),
    CONSTRAINT positive CHECK ((price > (0)::numeric)),
    CONSTRAINT orders_org_fk FOREIGN KEY (qty) REFERENCES public.orgs(id)
)
PARTITION BY RANGE (id);

COMMENT ON TABLE public.orders IS 'Orders';
COMMENT ON COLUMN public.orders."Note" IS 'free text, l''auteur';

CREATE INDEX orders_qty_idx ON ONLY public.orders USING btree (qty);

CREATE TRIGGER t BEFORE UPDATE ON public.orders FOR EACH ROW EXECUTE FUNCTION public.touch();
"#;
        assert_eq!(ddl, expected);
    }

    #[test]
    fn postgres_view_ddl() {
        let detail = TableDetail {
            kind: ObjectKind::MaterializedView,
            columns: vec![],
            indexes: vec![],
            foreign_keys: vec![],
            constraints: vec![],
            triggers: vec![],
            comment: None,
            partition_key: None,
            view_definition: Some(" SELECT 1 AS one;".into()),
            native_ddl: None,
        };
        assert_eq!(
            postgres_ddl(&ObjectRef::new("s", "Reporting", "mv"), &detail),
            "CREATE MATERIALIZED VIEW \"Reporting\".mv AS\nSELECT 1 AS one\nWITH DATA;\n"
        );
    }

    #[test]
    fn mysql_uses_native_ddl() {
        let mut detail = orders();
        detail.native_ddl = Some("CREATE TABLE `orders` (\n  `id` int\n)".into());
        assert_eq!(
            object_ddl(Engine::MySql, &ObjectRef::new("s", "app", "orders"), &detail),
            "CREATE TABLE `orders` (\n  `id` int\n);\n"
        );
    }

    #[test]
    fn parens() {
        assert_eq!(strip_outer_parens("(a + b)"), "a + b");
        assert_eq!(strip_outer_parens("(a) + (b)"), "(a) + (b)");
        assert_eq!(strip_outer_parens("a"), "a");
    }
}
