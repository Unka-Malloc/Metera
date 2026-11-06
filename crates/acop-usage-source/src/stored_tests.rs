use super::*;
use metera_sdk::store::{AdvanceKind, UsageCursor, UsageIssue};
use rusqlite::Connection;
use std::sync::OnceLock;

fn spec() -> &'static SqliteUsageSpec {
    static SPEC: OnceLock<SqliteUsageSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        SqliteUsageSpec::from_json(
            r#"{
                "root":"synthetic-store","fileNames":["synthetic.sqlite"],
                "sessions":{
                    "table":"runs","id":"id","model":"model","startedAt":"started","endedAt":"ended",
                    "tokens":{"input":"prompt","output":"completion","cacheRead":"read_cache","cacheWrite":"write_cache"}
                },
                "details":{
                    "table":"parts","session":"run","model":"model","task":"task",
                    "firstSeen":"first_seen","lastSeen":"last_seen",
                    "tokens":{"input":"prompt","output":"completion","cacheRead":"read_cache","cacheWrite":"write_cache"}
                },
                "activity":{"table":"activity","session":"run","timestamp":"ts"},
                "inputAccounting":"excludes-cache",
                "coverage":{"included":["main"],"additional":["auxiliary"]},
                "unknownModels":["not-reported-model"]
            }"#,
        )
        .unwrap()
    })
}

fn database() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE runs(id TEXT PRIMARY KEY, model TEXT, started REAL, ended REAL,
          prompt INTEGER, completion INTEGER, read_cache INTEGER, write_cache INTEGER);
         CREATE TABLE parts(counter_id INTEGER PRIMARY KEY, run TEXT, model TEXT, task TEXT, first_seen REAL, last_seen REAL,
          prompt INTEGER, completion INTEGER, read_cache INTEGER, write_cache INTEGER);
         CREATE TABLE activity(run TEXT, ts REAL);",
        )
        .unwrap();
    (directory, path, connection)
}

fn populate(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO runs VALUES('synthetic-run','selected-model',100,NULL,100,10,0,0);
          INSERT INTO parts(run,model,task,first_seen,last_seen,prompt,completion,read_cache,write_cache) VALUES('synthetic-run','auxiliary-model','auxiliary',110,130,20,5,0,0);
         INSERT INTO activity VALUES('synthetic-run',200);",
        )
        .unwrap();
}

#[test]
fn stored_sources_read_and_advance_through_the_bundled_kernel() {
    let (_directory, path, connection) = database();
    populate(&connection);
    let mut source = StoredSource::new(spec()).unwrap();
    let sessions = source.collect(&path).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].counts.total, Some(135));
    assert_eq!(sessions[0].allocations.len(), 2);
    assert!(!sessions[0].allocations[0].time_inferred);
    assert!(sessions[0].allocations[1].time_inferred);
    let cursor = UsageCursor::from(&sessions[0]);
    let baseline = source
        .advance(&[AdvanceInput {
            previous: None,
            current: cursor.clone(),
            count_initial: false,
        }])
        .unwrap();
    assert_eq!(baseline[0].kind, AdvanceKind::Baseline);
    assert_eq!(baseline[0].delta, None);
    let unchanged = source
        .advance(&[AdvanceInput {
            previous: Some(cursor.clone()),
            current: cursor.clone(),
            count_initial: true,
        }])
        .unwrap();
    assert_eq!(unchanged[0].delta.unwrap().total, Some(0));
    connection
        .execute("UPDATE runs SET prompt=104", [])
        .unwrap();
    let next = source.collect(&path).unwrap();
    let delta = source
        .advance(&[AdvanceInput {
            previous: Some(cursor),
            current: UsageCursor::from(&next[0]),
            count_initial: false,
        }])
        .unwrap();
    assert_eq!(delta[0].delta.unwrap().total, Some(4));
    assert!(!delta[0].attribution_gap);
}

#[test]
fn acquisition_distinguishes_empty_stores_from_unreadable_or_unsupported_stores() {
    let (directory, path, _connection) = database();
    let mut source = StoredSource::new(spec()).unwrap();
    assert!(source.collect(&path).unwrap().is_empty());
    let missing = directory.path().join("missing.sqlite");
    assert!(matches!(
        source.collect(&missing),
        Err(StoreSourceError::Read(SqliteUsageReadFailure::Open))
    ));
    assert!(!missing.exists());
    let unsupported = directory.path().join("unsupported.sqlite");
    let _empty = Connection::open(&unsupported).unwrap();
    assert!(matches!(
        source.collect(&unsupported),
        Err(StoreSourceError::Kernel(metera_sdk::Error::Refused { code }))
            if code == "usage_schema_unsupported"
    ));
}

#[test]
fn multi_batch_acquisition_neither_truncates_nor_returns_partial_success() {
    let (_directory, path, connection) = database();
    for index in 0..BATCH_RECORDS + 1 {
        connection
            .execute(
                "INSERT INTO runs VALUES(?1,'selected-model',100,200,1,0,0,0)",
                [format!("synthetic-{index:08}")],
            )
            .unwrap();
    }
    let mut source = StoredSource::new(spec()).unwrap();
    let sessions = source.collect(&path).unwrap();
    assert_eq!(sessions.len(), BATCH_RECORDS + 1);
    assert!(
        sessions
            .iter()
            .all(|session| session.counts.total == Some(1))
    );
    assert_eq!(
        sessions
            .iter()
            .map(|session| &session.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        sessions.len()
    );
    connection
        .execute(
            "INSERT INTO runs VALUES('synthetic-invalid','selected-model',100,200,-1,0,0,0)",
            [],
        )
        .unwrap();
    assert!(matches!(
        source.collect(&path),
        Err(StoreSourceError::Kernel(metera_sdk::Error::Refused { code }))
            if code == "usage_record_invalid"
    ));
}

#[test]
fn undeclared_task_coverage_stays_unknown_and_cannot_advance() {
    let (_directory, path, connection) = database();
    populate(&connection);
    connection
        .execute("UPDATE parts SET task='not-declared'", [])
        .unwrap();
    let mut source = StoredSource::new(spec()).unwrap();
    let sessions = source.collect(&path).unwrap();
    assert_eq!(sessions[0].counts.total, None);
    assert!(sessions[0].issues.contains(&UsageIssue::UnknownCoverage));
    assert!(matches!(
        source.advance(&[AdvanceInput {
            previous: None,
            current: UsageCursor::from(&sessions[0]),
            count_initial: true,
        }]),
        Err(StoreSourceError::Kernel(metera_sdk::Error::Refused { code }))
            if code == "usage_reading_incomplete"
    ));
}

#[test]
fn sqlite_json_and_wasm_preserve_counters_above_the_float_integer_range() {
    let (_directory, path, connection) = database();
    connection.execute(
        "INSERT INTO runs VALUES('synthetic-large','selected-model',100,200,9007199254740993,0,0,0)",
        [],
    ).unwrap();
    let mut source = StoredSource::new(spec()).unwrap();
    let sessions = source.collect(&path).unwrap();
    assert_eq!(sessions[0].counts.prompt, Some(9_007_199_254_740_993));
    assert_eq!(sessions[0].counts.total, Some(9_007_199_254_740_993));
}

#[test]
fn request_only_increments_survive_sqlite_and_the_bundled_kernel() {
    static SPEC: OnceLock<SqliteUsageSpec> = OnceLock::new();
    let spec = SPEC.get_or_init(|| {
        let mut value = spec().clone();
        value.sessions.requests = Some("calls".to_owned());
        value.details.requests = Some("calls".to_owned());
        value
    });
    let (_directory, path, connection) = database();
    populate(&connection);
    connection
        .execute_batch(
            "ALTER TABLE runs ADD COLUMN calls INTEGER;
         ALTER TABLE parts ADD COLUMN calls INTEGER;
         UPDATE runs SET calls=4;
         UPDATE parts SET calls=1;",
        )
        .unwrap();
    let mut source = StoredSource::new(spec).unwrap();
    let previous = source.collect(&path).unwrap();
    assert_eq!(previous[0].counts.total, Some(135));
    assert_eq!(previous[0].counts.requests, Some(5));
    connection.execute("UPDATE runs SET calls=5", []).unwrap();
    let current = source.collect(&path).unwrap();
    let delta = source
        .advance(&[AdvanceInput {
            previous: Some(UsageCursor::from(&previous[0])),
            current: UsageCursor::from(&current[0]),
            count_initial: false,
        }])
        .unwrap();
    assert_eq!(delta[0].delta.unwrap().total, Some(0));
    assert_eq!(delta[0].delta.unwrap().requests, Some(1));
    assert_eq!(delta[0].counters[0].delta.requests, Some(1));
}
