#![allow(clippy::await_holding_lock)]

mod common;

use herdr_db_core::cell::{Cell, StatementOutcome};
use herdr_db_core::ddl::postgres_ddl;
use herdr_db_core::model::{Engine, Identity, ObjectKind, ObjectRef};
use herdr_db_core::paging::{PageRequest, Position, Sort, SortDir};
use herdr_db_drivers::{Adapter, AnyAdapter, DriverError};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const VAR: &str = "HERDR_DB_TEST_POSTGRES";
const SCHEMA: &str = "herdr_fixture";
static LOADED: AtomicBool = AtomicBool::new(false);

async fn setup() -> Option<(HashMap<String, String>, AnyAdapter)> {
    let Some(settings) = common::settings(VAR) else {
        eprintln!("{VAR} absent : tests PostgreSQL ignorés");
        return None;
    };
    let mut db = common::connect(common::params(Engine::Postgres, &settings, None)).await;
    if !LOADED.swap(true, Ordering::SeqCst) {
        db.execute(include_str!("fixtures/postgres.sql"), 10).await.expect("fixture loads");
    }
    Some((settings, db))
}

fn obj(name: &str) -> ObjectRef {
    ObjectRef::new("test", SCHEMA, name)
}

#[tokio::test]
async fn introspects_schema_objects() {
    let _guard = common::lock();
    let Some((_, mut db)) = setup().await else { return };
    assert!(db.list_schemas().await.unwrap().contains(&SCHEMA.to_string()));
    let model = db.introspect_schema(SCHEMA).await.unwrap();
    let kinds: BTreeMap<&str, ObjectKind> = model.objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(kinds.get("orgs"), Some(&ObjectKind::Table));
    assert_eq!(kinds.get("events"), Some(&ObjectKind::PartitionedTable));
    assert_eq!(kinds.get("org_members"), Some(&ObjectKind::View));
    assert_eq!(kinds.get("org_counts"), Some(&ObjectKind::MaterializedView));
    assert_eq!(kinds.get("Audit Log"), Some(&ObjectKind::Table));
    assert!(!kinds.contains_key("events_2025"), "partitions stay under their parent");
    let big = model.objects.iter().find(|o| o.name == "big").unwrap();
    let estimate = big.estimated_rows.unwrap();
    assert!((250_000..=350_000).contains(&estimate), "estimate {estimate}");
    let events = model.objects.iter().find(|o| o.name == "events").unwrap();
    assert_eq!(events.estimated_rows, Some(2));
    let orgs = model.objects.iter().find(|o| o.name == "orgs").unwrap();
    assert_eq!(orgs.comment.as_deref(), Some("Organisations clientes"));
}

#[tokio::test]
async fn introspects_table_details() {
    let _guard = common::lock();
    let Some((_, mut db)) = setup().await else { return };
    let detail = db.introspect_table(&obj("Audit Log")).await.unwrap();
    let names: Vec<&str> = detail.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["Id", "org_id", "user_id", "Event Type", "payload", "price", "qty", "total", "note"]);
    let id = detail.column("Id").unwrap();
    assert_eq!(id.identity, Some(Identity::ByDefault));
    assert_eq!(detail.column("Event Type").unwrap().data_type, "herdr_fixture.mood");
    let total = detail.column("total").unwrap();
    assert!(total.generated.as_ref().unwrap().stored);
    assert_eq!(total.default, None);
    assert_eq!(detail.primary_key_columns(), vec!["Id"]);

    let fk = &detail.foreign_keys[0];
    assert_eq!(fk.columns, vec!["org_id", "user_id"]);
    assert_eq!(fk.ref_table, "memberships");
    assert_eq!(fk.ref_columns, vec!["org_id", "user_id"]);
    assert_eq!(detail.foreign_key_for("user_id").map(|(_, c)| c), Some("user_id"));

    let badges = detail.badges("Id");
    assert!(badges.primary_key && badges.indexed && badges.not_null && !badges.foreign_key);
    let lower = detail.indexes.iter().find(|i| i.name == "audit_lower_note_idx").unwrap();
    assert_eq!(lower.columns, vec!["lower(note)"]);
    assert_eq!(detail.comment.as_deref(), Some("Journal d'audit"));

    let orgs = db.introspect_table(&obj("orgs")).await.unwrap();
    assert_eq!(orgs.triggers.len(), 1);
    assert_eq!(orgs.triggers[0].timing, "BEFORE");
    assert_eq!(orgs.column("name").unwrap().comment.as_deref(), Some("Nom affiché, l'unique"));

    let missing = db.introspect_table(&obj("nope")).await.unwrap_err();
    assert!(matches!(missing, DriverError::NotFound(_)));
}

/// The rebuilt DDL, applied to an empty database and introspected again,
/// must give back the same model. Comparing models rather than text against
/// `pg_dump`: two different DDLs can produce the same table.
#[tokio::test]
async fn ddl_roundtrip() {
    let _guard = common::lock();
    let Some((settings, mut db)) = setup().await else { return };
    let tables = ["orgs", "memberships", "Audit Log", "events", "big", "org_members", "org_counts"];
    let mut originals = Vec::new();
    for name in tables {
        let detail = db.introspect_table(&obj(name)).await.unwrap();
        originals.push((name, detail));
    }

    let _ = db.execute("DROP DATABASE IF EXISTS herdr_db_roundtrip", 1).await;
    db.execute("CREATE DATABASE herdr_db_roundtrip", 1).await.unwrap();
    let mut target = common::connect(common::params(Engine::Postgres, &settings, Some("herdr_db_roundtrip"))).await;
    // Dependencies outside table DDL: the schema, the enum and the trigger function.
    target
        .execute(
            "CREATE SCHEMA herdr_fixture;
             CREATE TYPE herdr_fixture.mood AS ENUM ('happy', 'sad');
             CREATE FUNCTION herdr_fixture.touch() RETURNS trigger LANGUAGE plpgsql
               AS $$ BEGIN NEW.created_at := now(); RETURN NEW; END $$;",
            1,
        )
        .await
        .unwrap();
    for (name, detail) in &originals {
        let ddl = postgres_ddl(&obj(name), detail);
        target.execute(&ddl, 1).await.unwrap_or_else(|e| panic!("DDL of {name} does not apply: {e}\n{ddl}"));
    }
    for (name, original) in &originals {
        let rebuilt = target.introspect_table(&obj(name)).await.unwrap();
        assert_eq!(&rebuilt, original, "model of {name} differs after roundtrip");
    }
    drop(target);
    let _ = db.execute("DROP DATABASE IF EXISTS herdr_db_roundtrip", 1).await;
}

fn page_request(position: Position, sort: Option<Sort>) -> PageRequest {
    PageRequest {
        engine: Engine::Postgres,
        object: obj("big"),
        filter: None,
        sort,
        primary_key: vec!["id".into()],
        page_size: 100,
        position,
        column_types: BTreeMap::from([("id".to_string(), "integer".to_string())]),
    }
}

fn first_id(page: &herdr_db_core::cell::Page) -> String {
    page.rows.rows.first().unwrap()[0].copy_text().unwrap()
}

fn last_id(page: &herdr_db_core::cell::Page) -> String {
    page.rows.rows.last().unwrap()[0].copy_text().unwrap()
}

#[tokio::test]
async fn keyset_pagination_on_large_table() {
    let _guard = common::lock();
    let Some((_, mut db)) = setup().await else { return };
    let start = Instant::now();
    let first = db.fetch_page(&page_request(Position::First, None)).await.unwrap();
    assert_eq!(first.rows.rows.len(), 100);
    assert!(first.has_more);
    assert_eq!((first_id(&first), last_id(&first)), ("1".into(), "100".into()));
    assert_eq!(first.rows.columns[0].type_name.as_deref(), Some("integer"));

    let next = db.fetch_page(&page_request(Position::After(vec!["100".into()]), None)).await.unwrap();
    assert_eq!((first_id(&next), last_id(&next)), ("101".into(), "200".into()));

    let previous = db.fetch_page(&page_request(Position::Before(vec!["101".into()]), None)).await.unwrap();
    assert_eq!((first_id(&previous), last_id(&previous)), ("1".into(), "100".into()));

    let last = db.fetch_page(&page_request(Position::Last, None)).await.unwrap();
    assert_eq!((first_id(&last), last_id(&last)), ("299901".into(), "300000".into()));
    let deep = db.fetch_page(&page_request(Position::After(vec!["299950".into()]), None)).await.unwrap();
    assert_eq!(deep.rows.rows.len(), 50);
    assert!(!deep.has_more);
    assert!(start.elapsed() < Duration::from_secs(3), "pages must stay instant");

    // NULL and empty string stay distinct.
    let maybe: Vec<&Cell> = first.rows.rows.iter().take(3).map(|r| &r[2]).collect();
    assert_eq!(maybe, vec![&Cell::Text(String::new()), &Cell::Text("x".into()), &Cell::Null]);

    let sorted = db
        .fetch_page(&page_request(Position::Offset(100), Some(Sort { column: "label".into(), dir: SortDir::Desc })))
        .await
        .unwrap();
    assert_eq!(first_id(&sorted), "299900");

    let mut filtered = page_request(Position::First, None);
    filtered.filter = Some("label LIKE 'row 00001%'".into());
    let page = db.fetch_page(&filtered).await.unwrap();
    assert_eq!(page.rows.rows.len(), 10);
    assert_eq!(
        db.count(&herdr_db_core::paging::count_sql(Engine::Postgres, &obj("big"), filtered.filter.as_deref()))
            .await
            .unwrap(),
        10
    );
}

#[tokio::test]
async fn console_caps_rows_and_stays_usable() {
    let _guard = common::lock();
    let Some((_, mut db)) = setup().await else { return };
    let outcome = db.execute("SELECT * FROM herdr_fixture.big", 50).await.unwrap();
    match &outcome.statements[0] {
        StatementOutcome::Rows { set, truncated } => {
            assert_eq!(set.rows.len(), 50);
            assert!(truncated);
            assert_eq!(set.columns[1].type_name.as_deref(), Some("text"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let outcome = db
        .execute(
            "SELECT payload FROM herdr_fixture.\"Audit Log\" ORDER BY 1; UPDATE herdr_fixture.big SET maybe = maybe WHERE id <= 3",
            100,
        )
        .await
        .unwrap();
    assert_eq!(outcome.statements.len(), 2);
    assert_eq!(outcome.statements[1], StatementOutcome::Command { tag: "UPDATE".into(), affected: Some(3) });

    let outcome = db.execute("SELECT payload FROM herdr_fixture.\"Audit Log\" ORDER BY 1", 100).await.unwrap();
    let StatementOutcome::Rows { set, .. } = &outcome.statements[0] else { panic!() };
    assert_eq!(set.rows[0][0], Cell::Json("{\"k\": [1, 2]}".into()));

    let error = db.execute("SELECT * FROM missing_table", 10).await.unwrap_err();
    assert!(matches!(error, DriverError::Query { ref code, .. } if code.as_deref() == Some("42P01")), "{error:?}");
}

#[tokio::test]
async fn read_only_session_refuses_writes() {
    let _guard = common::lock();
    let Some((settings, _db)) = setup().await else { return };
    let params = common::with_session(common::params(Engine::Postgres, &settings, None), true, None);
    let mut db = common::connect(params).await;
    let error = db.execute("UPDATE herdr_fixture.big SET maybe = 'y' WHERE id = 1", 10).await.unwrap_err();
    assert!(matches!(error, DriverError::Query { ref code, .. } if code.as_deref() == Some("25006")), "{error:?}");
    db.set_read_only(false).await.unwrap();
    db.execute("UPDATE herdr_fixture.big SET maybe = maybe WHERE id = 1", 10).await.unwrap();
}

#[tokio::test]
async fn cancel_and_timeout() {
    let _guard = common::lock();
    let Some((settings, mut db)) = setup().await else { return };
    let cancel = db.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    let start = Instant::now();
    let error = db.execute("SELECT pg_sleep(10)", 10).await.unwrap_err();
    canceller.await.unwrap();
    assert_eq!(error, DriverError::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(5));
    db.execute("SELECT 1", 1).await.unwrap();

    let params =
        common::with_session(common::params(Engine::Postgres, &settings, None), true, Some(Duration::from_millis(200)));
    let mut db = common::connect(params).await;
    let error = db.execute("SELECT pg_sleep(2)", 1).await.unwrap_err();
    assert!(
        matches!(error, DriverError::Query { ref code, ref message } if code.as_deref() == Some("57014") && message.contains("timeout")),
        "{error:?}"
    );
}
