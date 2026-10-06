#![allow(clippy::await_holding_lock)]

mod common;

use herdr_db_core::cell::{Cell, StatementOutcome};
use herdr_db_core::ddl::object_ddl;
use herdr_db_core::model::{ConstraintKind, Engine, ObjectKind, ObjectRef};
use herdr_db_core::paging::{PageRequest, Position, Sort, SortDir, count_sql};
use herdr_db_drivers::{Adapter, AnyAdapter, DriverError};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const VAR: &str = "HERDR_DB_TEST_MYSQL";
const SCHEMA: &str = "herdr_fixture";
static LOADED: AtomicBool = AtomicBool::new(false);

async fn setup() -> Option<(HashMap<String, String>, AnyAdapter)> {
    let Some(settings) = common::settings(VAR) else {
        eprintln!("{VAR} absent : tests MySQL ignorés");
        return None;
    };
    let mut db = common::connect(common::params(Engine::MySql, &settings, None)).await;
    if !LOADED.swap(true, Ordering::SeqCst) {
        db.execute(include_str!("fixtures/mysql.sql"), 10).await.expect("fixture loads");
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
    let schemas = db.list_schemas().await.unwrap();
    assert!(schemas.contains(&SCHEMA.to_string()));
    assert!(!schemas.contains(&"mysql".to_string()), "system databases are hidden");
    let model = db.introspect_schema(SCHEMA).await.unwrap();
    let kinds: BTreeMap<&str, ObjectKind> = model.objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(kinds.get("orgs"), Some(&ObjectKind::Table));
    assert_eq!(kinds.get("Audit Log"), Some(&ObjectKind::Table));
    assert_eq!(kinds.get("org_members"), Some(&ObjectKind::View));
    let big = model.objects.iter().find(|o| o.name == "big").unwrap();
    let estimate = big.estimated_rows.unwrap();
    assert!((100_000..=600_000).contains(&estimate), "estimate {estimate}");
    let orgs = model.objects.iter().find(|o| o.name == "orgs").unwrap();
    assert_eq!(orgs.comment.as_deref(), Some("Organisations clientes"));
    let view = model.objects.iter().find(|o| o.name == "org_members").unwrap();
    assert_eq!(view.comment, None);
}

#[tokio::test]
async fn introspects_table_details() {
    let _guard = common::lock();
    let Some((_, mut db)) = setup().await else { return };
    let detail = db.introspect_table(&obj("Audit Log")).await.unwrap();
    let names: Vec<&str> = detail.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["Id", "org_id", "user_id", "payload", "price", "qty", "total", "qty_label", "note", "raw"]);
    assert_eq!(detail.primary_key_columns(), vec!["Id"]);
    assert!(detail.column("Id").unwrap().extra.as_deref().unwrap().contains("auto_increment"));
    let total = detail.column("total").unwrap().generated.clone().unwrap();
    assert!(total.stored);
    assert!(!detail.column("qty_label").unwrap().generated.as_ref().unwrap().stored);
    assert_eq!(detail.column("price").unwrap().data_type, "decimal(10,2)");

    let fk = detail.foreign_keys.iter().find(|f| f.name == "audit_membership_fk").unwrap();
    assert_eq!(fk.columns, vec!["org_id", "user_id"]);
    assert_eq!(fk.ref_columns, vec!["org_id", "user_id"]);
    assert_eq!(fk.ref_table, "memberships");
    let check = detail.constraints.iter().find(|c| c.kind == ConstraintKind::Check).unwrap();
    assert_eq!(check.name, "positive_qty");
    assert!(check.definition.as_deref().unwrap().contains("qty"));
    let note_index = detail.indexes.iter().find(|i| i.name == "audit_note_idx").unwrap();
    assert_eq!(note_index.columns, vec!["note"]);
    assert_eq!(detail.comment.as_deref(), Some("Journal d'audit"));
    let ddl = object_ddl(Engine::MySql, &obj("Audit Log"), &detail);
    assert!(ddl.starts_with("CREATE TABLE `Audit Log`"), "{ddl}");

    let memberships = db.introspect_table(&obj("memberships")).await.unwrap();
    assert_eq!(memberships.primary_key_columns(), vec!["org_id", "user_id"]);
    assert_eq!(memberships.column("role").unwrap().data_type, "enum('member','admin')");
    assert_eq!(memberships.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));

    let orgs = db.introspect_table(&obj("orgs")).await.unwrap();
    assert_eq!(orgs.triggers.len(), 1);
    assert_eq!(orgs.triggers[0].timing, "BEFORE");
    assert_eq!(orgs.column("name").unwrap().comment.as_deref(), Some("Nom affiché, l'unique"));

    let view = db.introspect_table(&obj("org_members")).await.unwrap();
    assert_eq!(view.kind, ObjectKind::View);
    assert!(view.native_ddl.as_deref().unwrap().contains("VIEW"));

    let missing = db.introspect_table(&obj("nope")).await.unwrap_err();
    assert!(matches!(missing, DriverError::NotFound(_)));
}

fn page_request(position: Position, sort: Option<Sort>) -> PageRequest {
    PageRequest {
        engine: Engine::MySql,
        object: obj("big"),
        filter: None,
        sort,
        primary_key: vec!["id".into()],
        page_size: 100,
        position,
        column_types: BTreeMap::from([("label".to_string(), "varchar(20)".to_string())]),
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
    assert_eq!((first_id(&first), last_id(&first)), ("1".into(), "100".into()));
    assert!(first.has_more);
    assert_eq!(first.rows.columns[1].type_name.as_deref(), Some("varchar(20)"));
    let next = db.fetch_page(&page_request(Position::After(vec!["100".into()]), None)).await.unwrap();
    assert_eq!((first_id(&next), last_id(&next)), ("101".into(), "200".into()));
    let previous = db.fetch_page(&page_request(Position::Before(vec!["101".into()]), None)).await.unwrap();
    assert_eq!((first_id(&previous), last_id(&previous)), ("1".into(), "100".into()));
    let last = db.fetch_page(&page_request(Position::Last, None)).await.unwrap();
    assert_eq!((first_id(&last), last_id(&last)), ("299901".into(), "300000".into()));
    assert!(start.elapsed() < Duration::from_secs(3), "pages must stay instant");

    let maybe: Vec<&Cell> = first.rows.rows.iter().take(3).map(|r| &r[2]).collect();
    assert_eq!(maybe, vec![&Cell::Text(String::new()), &Cell::Text("x".into()), &Cell::Null]);

    let sorted = db
        .fetch_page(&page_request(Position::Offset(100), Some(Sort { column: "label".into(), dir: SortDir::Desc })))
        .await
        .unwrap();
    assert_eq!(first_id(&sorted), "299900");

    let mut filtered = page_request(Position::First, None);
    filtered.filter = Some("label LIKE 'row 00001%'".into());
    assert_eq!(db.fetch_page(&filtered).await.unwrap().rows.rows.len(), 10);
    assert_eq!(db.count(&count_sql(Engine::MySql, &obj("big"), filtered.filter.as_deref())).await.unwrap(), 10);
}

#[tokio::test]
async fn console_caps_rows_and_types_cells() {
    let _guard = common::lock();
    let Some((_, mut db)) = setup().await else { return };
    let outcome = db.execute("SELECT * FROM herdr_fixture.big", 50).await.unwrap();
    match &outcome.statements[0] {
        StatementOutcome::Rows { set, truncated } => {
            assert_eq!(set.rows.len(), 50);
            assert!(truncated);
        }
        other => panic!("unexpected {other:?}"),
    }
    db.execute("SELECT 1", 1).await.expect("connection usable after a capped query");

    let outcome = db.execute("SELECT payload, raw FROM herdr_fixture.`Audit Log` ORDER BY `Id`", 10).await.unwrap();
    let StatementOutcome::Rows { set, .. } = &outcome.statements[0] else { panic!() };
    assert_eq!(set.columns[0].type_name.as_deref(), Some("json"));
    assert!(matches!(&set.rows[0][0], Cell::Json(j) if j.contains("\"k\"")));
    assert_eq!(set.rows[0][1], Cell::Binary { len: 2 });
    assert_eq!(set.rows[1][0], Cell::Null);

    let outcome = db
        .execute("SELECT 1 AS a; UPDATE herdr_fixture.big SET maybe = maybe WHERE id <= 3; SELECT 2 AS b", 10)
        .await
        .unwrap();
    assert_eq!(outcome.statements.len(), 3);
    assert!(matches!(&outcome.statements[1], StatementOutcome::Command { tag, .. } if tag == "UPDATE"));
    assert!(matches!(&outcome.statements[2], StatementOutcome::Rows { set, .. } if set.columns[0].name == "b"));

    let error = db.execute("SELECT * FROM herdr_fixture.missing", 10).await.unwrap_err();
    assert!(matches!(error, DriverError::Query { ref code, .. } if code.as_deref() == Some("1146")), "{error:?}");
}

#[tokio::test]
async fn read_only_session_refuses_writes() {
    let _guard = common::lock();
    let Some((settings, _db)) = setup().await else { return };
    let params = common::with_session(common::params(Engine::MySql, &settings, None), true, None);
    let mut db = common::connect(params).await;
    let error = db.execute("UPDATE herdr_fixture.big SET maybe = 'y' WHERE id = 1", 10).await.unwrap_err();
    assert!(matches!(error, DriverError::Query { ref code, .. } if code.as_deref() == Some("1792")), "{error:?}");
    db.set_read_only(false).await.unwrap();
    db.execute("UPDATE herdr_fixture.big SET maybe = maybe WHERE id = 1", 10).await.unwrap();
}

#[tokio::test]
async fn cancel_and_timeout() {
    let _guard = common::lock();
    let Some((settings, mut db)) = setup().await else { return };
    let heavy = "SELECT COUNT(*) FROM herdr_fixture.big a, herdr_fixture.big b WHERE a.label < b.label";
    let cancel = db.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel().await.unwrap();
    });
    let start = Instant::now();
    let error = db.execute(heavy, 10).await.unwrap_err();
    canceller.await.unwrap();
    assert_eq!(error, DriverError::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(10));
    db.execute("SELECT 1", 1).await.unwrap();

    let params =
        common::with_session(common::params(Engine::MySql, &settings, None), true, Some(Duration::from_millis(300)));
    let mut db = common::connect(params).await;
    let error = db.execute(heavy, 1).await.unwrap_err();
    assert!(matches!(error, DriverError::Query { ref code, .. } if code.as_deref() == Some("3024")), "{error:?}");
}
