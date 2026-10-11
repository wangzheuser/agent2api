//! 双历史迁移验收：历史输入冻结，调用候选 schema 与原样装载的余额迁移，不依赖运行时 Git。
//! H = 94c674e7e3267bf1837afc44dc1860fe2dc6dc6c
//! U = f82308a9549ee4760f27e393058c3482056ec514
use agent2api_server::server::core::workbuddy_policy;
use rusqlite::{types::Value, Connection};
use serde_json::{json, Value as JsonValue};
use server::db::{schema, Db};

// js_truthy 是 pub(crate)：装载原工具源码，只接回真实依赖，不复制迁移或余额算法。
#[allow(dead_code)]
#[path = "../src/server/core/account_store/store_util.rs"]
pub(crate) mod store_util;

mod server {
    pub use agent2api_server::server::{config, db, logging};

    pub mod core {
        pub use agent2api_server::server::core::limiter;
        pub mod account_store {
            pub(crate) use crate::store_util;
            pub use agent2api_server::server::core::account_store::state;
        }
    }
}

// 原样冻结 H 的 tests/fixtures/requests-v6.sql；只包含 v7 以后迁移消费的表。
const REQUESTS_V6: &str = r#"
-- Frozen requests v6 DDL shared by upstream 9e77413 and local 69ba0e9.
-- Only the table consumed by migrations v7 onward is needed; no live database.
CREATE TABLE IF NOT EXISTS requests (
  row_id            INTEGER PRIMARY KEY AUTOINCREMENT,
  id                TEXT NOT NULL DEFAULT '',
  ts                INTEGER NOT NULL,
  model             TEXT NOT NULL,
  account_id        TEXT NOT NULL DEFAULT '',
  account_name      TEXT NOT NULL DEFAULT '',
  status            INTEGER NOT NULL DEFAULT 0,
  duration_ms       INTEGER NOT NULL DEFAULT 0,
  first_response_ms INTEGER,
  attempts          INTEGER NOT NULL DEFAULT 1,
  error             TEXT,
  prompt_tokens     INTEGER NOT NULL DEFAULT 0,
  completion_tokens INTEGER NOT NULL DEFAULT 0,
  total_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  provider          TEXT NOT NULL DEFAULT '',
  client_model      TEXT NOT NULL DEFAULT '',
  upstream_model    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
CREATE INDEX IF NOT EXISTS idx_requests_id ON requests(id);
ALTER TABLE requests ADD COLUMN attempt_details TEXT NOT NULL DEFAULT '[]';
ALTER TABLE requests ADD COLUMN sensitive_hits TEXT NOT NULL DEFAULT '[]';
ALTER TABLE requests ADD COLUMN client_reasoning TEXT NOT NULL DEFAULT '';
ALTER TABLE requests ADD COLUMN upstream_reasoning TEXT NOT NULL DEFAULT '';
ALTER TABLE requests ADD COLUMN phase TEXT NOT NULL DEFAULT '';
ALTER TABLE requests ADD COLUMN phase_started_at INTEGER;
PRAGMA user_version=6;
"#;

// U 的 V8_SCHEMA；H 的 v8 是 upstream_credits，两者不可互换。
const UPSTREAM_USAGE_V8: &str = r#"
CREATE TABLE IF NOT EXISTS account_usage_records (
  account_id      TEXT PRIMARY KEY,
  usage           TEXT,
  error           TEXT,
  code            TEXT,
  remaining       REAL,
  unlimited       INTEGER NOT NULL DEFAULT 0 CHECK (unlimited IN (0, 1)),
  last_success_at INTEGER NOT NULL DEFAULT 0,
  last_attempt_at INTEGER NOT NULL DEFAULT 0,
  updated_at      INTEGER NOT NULL DEFAULT 0
);
"#;

const OLD_REQUESTS: &str = "SELECT row_id,id,ts,model,status,prompt_tokens,completion_tokens,
    cache_read_tokens,attempt_details,sensitive_hits FROM requests ORDER BY row_id";
const OLD_USAGE: &str = "SELECT account_id,usage,error,code,remaining,unlimited,
    last_success_at,last_attempt_at,updated_at FROM account_usage_records ORDER BY account_id";

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut statement = conn.prepare(sql).unwrap();
    let count = statement.column_count();
    let mapped = statement
        .query_map([], |row| {
            (0..count)
                .map(|column| row.get(column))
                .collect::<rusqlite::Result<Vec<Value>>>()
        })
        .unwrap();
    mapped.collect::<rusqlite::Result<_>>().unwrap()
}

fn version(conn: &Connection) -> i64 {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn legacy_requests() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(REQUESTS_V6).unwrap();
    conn.execute_batch(
        "INSERT INTO requests(id,ts,model,status,prompt_tokens,completion_tokens,cache_read_tokens)
         VALUES ('zero',101,'fixture-zero',200,0,0,0),
                ('null',102,'fixture-null',500,47699,2918,47616),
                ('fractional',103,'fixture-fractional',402,13,7,5);
         UPDATE requests SET attempt_details='[{\"status\":402}]' WHERE id='fractional';",
    )
    .unwrap();
    conn
}

fn add_test_origin(conn: &Connection) {
    conn.execute_batch(
        "ALTER TABLE requests ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0;
         UPDATE requests SET is_test=1 WHERE id='fractional';",
    )
    .unwrap();
}

fn local_history(at: i64) -> Connection {
    assert!((7..=10).contains(&at));
    let conn = legacy_requests();
    conn.execute_batch(
        "ALTER TABLE requests ADD COLUMN cache_creation_tokens INTEGER;
         UPDATE requests SET cache_creation_tokens=0 WHERE id='zero';
         UPDATE requests SET cache_creation_tokens=42 WHERE id='fractional';",
    )
    .unwrap();
    if at >= 8 {
        conn.execute_batch(
            "ALTER TABLE requests ADD COLUMN upstream_credits REAL;
             UPDATE requests SET upstream_credits=0 WHERE id='zero';
             UPDATE requests SET upstream_credits=0.012345 WHERE id='fractional';",
        )
        .unwrap();
    }
    if at >= 9 {
        add_test_origin(&conn);
    }
    conn.pragma_update(None, "user_version", at).unwrap();
    conn
}

fn add_old_usage(conn: &Connection) {
    conn.execute_batch(UPSTREAM_USAGE_V8).unwrap();
    conn.execute_batch(
        "INSERT INTO account_usage_records VALUES
         ('zero','{\"available\":0}',NULL,NULL,0,0,100,100,100),
         ('unknown',NULL,'HTTP 401','unauthorized',NULL,0,0,150,150),
         ('fractional','{\"available\":0.012345}','HTTP 503','unavailable',0.012345,0,100,200,200),
         ('unlimited','{\"unlimited\":true}',NULL,NULL,NULL,1,300,300,300);",
    )
    .unwrap();
}

fn upstream_history(at: i64) -> Connection {
    assert!((7..=8).contains(&at));
    let conn = legacy_requests();
    add_test_origin(&conn);
    if at == 8 {
        add_old_usage(&conn);
    }
    conn.pragma_update(None, "user_version", at).unwrap();
    conn
}

fn assert_schema(conn: &Connection) {
    assert!(schema::SCHEMA_VERSION >= 11);
    assert_eq!(version(conn), schema::SCHEMA_VERSION);
    for (table, column, kind, required, default, primary_key) in [
        ("requests", "cache_creation_tokens", "INTEGER", 0, None, 0),
        ("requests", "upstream_credits", "REAL", 0, None, 0),
        ("requests", "is_test", "INTEGER", 1, Some("0"), 0),
        ("account_usage_records", "account_id", "TEXT", 0, None, 1),
        ("account_usage_records", "usage", "TEXT", 0, None, 0),
        ("account_usage_records", "error", "TEXT", 0, None, 0),
        ("account_usage_records", "code", "TEXT", 0, None, 0),
        ("account_usage_records", "remaining", "REAL", 0, None, 0),
        (
            "account_usage_records",
            "unlimited",
            "INTEGER",
            1,
            Some("0"),
            0,
        ),
        (
            "account_usage_records",
            "last_success_at",
            "INTEGER",
            1,
            Some("0"),
            0,
        ),
        (
            "account_usage_records",
            "last_attempt_at",
            "INTEGER",
            1,
            Some("0"),
            0,
        ),
        (
            "account_usage_records",
            "updated_at",
            "INTEGER",
            1,
            Some("0"),
            0,
        ),
        (
            "account_usage_records",
            "identity",
            "TEXT",
            1,
            Some("''"),
            0,
        ),
        (
            "account_usage_records",
            "queried_at",
            "INTEGER",
            1,
            Some("0"),
            0,
        ),
    ] {
        let actual: (String, i64, Option<String>, i64) = conn
            .query_row(
                "SELECT type,\"notnull\",dflt_value,pk FROM pragma_table_info(?1) WHERE name=?2",
                [table, column],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap_or_else(|error| panic!("{table}.{column}: {error}"));
        assert_eq!(
            actual,
            (
                kind.into(),
                required,
                default.map(str::to_string),
                primary_key
            ),
            "{table}.{column}"
        );
    }
    for (index, column) in [("idx_requests_ts", "ts"), ("idx_requests_id", "id")] {
        assert_eq!(
            rows(
                conn,
                &format!("SELECT name FROM pragma_index_info('{index}')")
            ),
            vec![vec![Value::Text(column.into())]]
        );
    }
}

fn assert_idempotent(conn: &Connection) {
    let queries = [
        "SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name",
        "SELECT * FROM requests ORDER BY row_id",
        "SELECT * FROM account_usage_records ORDER BY account_id",
    ];
    let before: Vec<_> = queries.iter().map(|sql| rows(conn, sql)).collect();
    for _ in 0..2 {
        schema::migrate(conn).unwrap();
        assert_schema(conn);
        for (sql, expected) in queries.iter().zip(&before) {
            assert_eq!(&rows(conn, sql), expected, "重复迁移改变了 {sql}");
        }
    }
}

#[test]
fn upstream_sync_new_database_has_usage_identity_and_queried_at_defaults() {
    let conn = Connection::open_in_memory().unwrap();
    schema::migrate(&conn).unwrap();
    assert_schema(&conn);
    conn.execute(
        "INSERT INTO account_usage_records(account_id) VALUES ('new')",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&conn, "SELECT identity,queried_at,remaining,unlimited,last_success_at,last_attempt_at FROM account_usage_records"),
        vec![vec![Value::Text(String::new()), Value::Integer(0), Value::Null,
            Value::Integer(0), Value::Integer(0), Value::Integer(0)]]
    );
    for sql in [
        "INSERT INTO account_usage_records(account_id,identity) VALUES ('bad-identity',NULL)",
        "INSERT INTO account_usage_records(account_id,queried_at) VALUES ('bad-time',NULL)",
        "INSERT INTO account_usage_records(account_id,unlimited) VALUES ('bad-unlimited',2)",
        "INSERT INTO account_usage_records(account_id) VALUES ('new')",
    ] {
        assert!(conn.execute(sql, []).is_err(), "应拒绝 {sql}");
    }
    assert_eq!(
        rows(&conn, "SELECT account_id FROM account_usage_records").len(),
        1
    );
    assert_idempotent(&conn);
}

#[test]
fn upstream_sync_local_v7_to_v10_preserve_request_history() {
    for at in 7..=10 {
        let conn = local_history(at);
        let before = rows(&conn, OLD_REQUESTS);
        schema::migrate(&conn).unwrap();
        assert_schema(&conn);
        assert_eq!(rows(&conn, OLD_REQUESTS), before, "H v{at}");
        let values: Vec<_> = conn
            .prepare("SELECT cache_creation_tokens,upstream_credits,is_test FROM requests ORDER BY row_id")
            .unwrap()
            .query_map([], |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<f64>>(1)?, row.get::<_, i64>(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            values,
            vec![
                (Some(0), if at >= 8 { Some(0.0) } else { None }, 0),
                (None, None, 0),
                (
                    Some(42),
                    if at >= 8 { Some(0.012345) } else { None },
                    if at >= 9 { 1 } else { 0 }
                ),
            ],
            "H v{at}"
        );
        assert!(rows(&conn, OLD_USAGE).is_empty());
        assert_idempotent(&conn);
    }
}

#[test]
fn upstream_sync_upstream_v7_v8_preserve_requests_and_usage_history() {
    for at in 7..=8 {
        let conn = upstream_history(at);
        let requests = rows(&conn, OLD_REQUESTS);
        let usage = if at == 8 {
            rows(&conn, OLD_USAGE)
        } else {
            Vec::new()
        };
        schema::migrate(&conn).unwrap();
        assert_schema(&conn);
        assert_eq!(rows(&conn, OLD_REQUESTS), requests, "U v{at}");
        assert_eq!(rows(&conn, OLD_USAGE), usage, "U v{at}");
        assert_eq!(rows(&conn, "SELECT cache_creation_tokens,upstream_credits,is_test FROM requests ORDER BY row_id"), vec![
            vec![Value::Null, Value::Null, Value::Integer(0)],
            vec![Value::Null, Value::Null, Value::Integer(0)],
            vec![Value::Null, Value::Null, Value::Integer(1)],
        ]);
        let expected_times = if at == 8 {
            vec![200, 150, 300, 100]
        } else {
            Vec::new()
        };
        assert_eq!(
            rows(
                &conn,
                "SELECT identity,queried_at FROM account_usage_records ORDER BY account_id"
            ),
            expected_times
                .into_iter()
                .map(|at| vec![Value::Text(String::new()), Value::Integer(at)])
                .collect::<Vec<_>>()
        );
        assert_idempotent(&conn);
    }
}

#[test]
fn upstream_sync_v11_repairs_partial_usage_columns_without_overwriting_values() {
    for (has_identity, has_queried_at) in [(true, false), (false, true), (true, true)] {
        let conn = upstream_history(8);
        if has_identity {
            conn.execute_batch(
                "ALTER TABLE account_usage_records ADD COLUMN identity TEXT NOT NULL DEFAULT '';
                UPDATE account_usage_records SET identity='fixture-identity';",
            )
            .unwrap();
        }
        if has_queried_at {
            conn.execute_batch("ALTER TABLE account_usage_records ADD COLUMN queried_at INTEGER NOT NULL DEFAULT 0;
                UPDATE account_usage_records SET queried_at=257;").unwrap();
        }
        let before = rows(&conn, OLD_USAGE);
        schema::migrate(&conn).unwrap();
        assert_schema(&conn);
        assert_eq!(rows(&conn, OLD_USAGE), before);
        assert_eq!(
            rows(
                &conn,
                "SELECT identity,queried_at FROM account_usage_records ORDER BY account_id"
            ),
            [200, 150, 300, 100]
                .into_iter()
                .map(|at| vec![
                    Value::Text(if has_identity { "fixture-identity" } else { "" }.into()),
                    Value::Integer(if has_queried_at { 257 } else { at }),
                ])
                .collect::<Vec<_>>()
        );
        assert_idempotent(&conn);
    }
}

#[test]
fn upstream_sync_v11_backfills_latest_time_without_changing_old_fields() {
    let conn = upstream_history(8);
    conn.execute_batch(
        "ALTER TABLE account_usage_records ADD COLUMN queried_at INTEGER NOT NULL DEFAULT 0;
         UPDATE account_usage_records SET last_attempt_at=170 WHERE account_id='zero';
         UPDATE account_usage_records SET last_success_at=250 WHERE account_id='fractional';
         UPDATE account_usage_records SET queried_at=257 WHERE account_id='unknown';",
    )
    .unwrap();
    let before = rows(&conn, OLD_USAGE);
    schema::migrate(&conn).unwrap();
    assert_schema(&conn);
    assert_eq!(rows(&conn, OLD_USAGE), before);
    assert_eq!(
        rows(
            &conn,
            "SELECT account_id,identity,queried_at FROM account_usage_records ORDER BY account_id"
        ),
        [
            ("fractional", 250),
            ("unknown", 257),
            ("unlimited", 300),
            ("zero", 170),
        ]
        .into_iter()
        .map(|(id, at)| vec![
            Value::Text(id.into()),
            Value::Text(String::new()),
            Value::Integer(at)
        ])
        .collect::<Vec<_>>()
    );
    assert_idempotent(&conn);
}

#[test]
fn upstream_sync_v11_failure_rolls_back_columns_and_version_then_retries() {
    let conn = local_history(10);
    add_old_usage(&conn);
    // 与 H 原反例相同：隐藏生成列不在 table_info，补 queried_at 冲突使本版 DDL 回滚。
    conn.execute_batch("ALTER TABLE account_usage_records ADD COLUMN queried_at INTEGER GENERATED ALWAYS AS (0) VIRTUAL;").unwrap();
    let requests = rows(&conn, "SELECT * FROM requests ORDER BY row_id");
    let usage = rows(&conn, OLD_USAGE);
    let definitions = rows(
        &conn,
        "SELECT type,name,sql FROM sqlite_master ORDER BY type,name",
    );
    let error = schema::migrate(&conn).unwrap_err();
    assert!(error.to_string().contains("queried_at"), "{error}");
    assert_eq!(version(&conn), 10);
    assert!(conn.is_autocommit());
    assert_eq!(
        rows(
            &conn,
            "SELECT type,name,sql FROM sqlite_master ORDER BY type,name"
        ),
        definitions
    );
    assert!(rows(
        &conn,
        "SELECT name FROM pragma_table_info('account_usage_records') WHERE name='identity'"
    )
    .is_empty());
    assert_eq!(
        rows(&conn, "SELECT * FROM requests ORDER BY row_id"),
        requests
    );
    assert_eq!(rows(&conn, OLD_USAGE), usage);
    conn.execute_batch("ALTER TABLE account_usage_records DROP COLUMN queried_at;")
        .unwrap();
    schema::migrate(&conn).unwrap();
    assert_schema(&conn);
    assert_eq!(
        rows(&conn, "SELECT * FROM requests ORDER BY row_id"),
        requests
    );
    assert_eq!(rows(&conn, OLD_USAGE), usage);
    assert_idempotent(&conn);
}

fn put_snapshot(conn: &Connection, key: &str, value: &JsonValue) {
    conn.execute(
        "INSERT INTO kv(key,value) VALUES (?1,?2)",
        rusqlite::params![key, value.to_string()],
    )
    .unwrap();
}

fn saved_json(conn: &Connection, key: &str) -> JsonValue {
    let text: String = conn
        .query_row("SELECT value FROM kv WHERE key=?1", [key], |row| row.get(0))
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn migration_state(db: &Db) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    db.with(|conn| {
        (
            rows(
                conn,
                "SELECT * FROM account_usage_records ORDER BY account_id",
            ),
            rows(conn, "SELECT key,value FROM kv ORDER BY key"),
        )
    })
    .unwrap()
}

#[test]
fn upstream_sync_nested_snapshot_keeps_other_tasks_and_each_rows_time() {
    // 每个用例单独装载实际模块，使 install 的 OnceLock 和内存事实不跨用例共享。
    #[allow(dead_code)]
    #[path = "../src/server/core/usage_records.rs"]
    mod records;

    let db = Db::open(std::path::Path::new(":memory:")).unwrap();
    let other_tasks = json!({
        "usageQuery":{"lastAttemptAt":800,"nextRunAt":1600,"retryAt":1700},
        "zcodeAutoClaim":{"value":{"granted":false},"nextRunAt":1800,"owner":"fixture-owner"},
        "unknownFutureTask":{"value":{"keep":[1,2,3]}}
    });
    let mut background = other_tasks.clone();
    background["usageQuerySnapshot"] = json!({"value":{
        "at":900,"skipped":7,"results":[
            {"id":"row-time","queriedAt":100,"usage":{"available":0.125,"creditDetails":{"fetchedAt":80}}},
            {"id":"fetched-time","usage":{"totalLeft":0,"creditDetails":{"fetchedAt":200}}},
            {"id":"batch-time","usage":{"unlimited":true}},
            {"id":"failure","usage":null,"error":"HTTP 401","code":"unauthorized"}
        ]
    }});
    db.with(|conn| {
        put_snapshot(conn, "backgroundTaskState", &background);
        put_snapshot(conn, "unrelatedSetting", &json!({"enabled":false}));
    })
    .unwrap();
    records::install(Some(db.clone()));
    db.with(|conn| {
        assert_eq!(saved_json(conn, "backgroundTaskState"), other_tasks);
        assert_eq!(
            saved_json(conn, "unrelatedSetting"),
            json!({"enabled":false})
        );
        assert_eq!(rows(conn, OLD_USAGE).len(), 4);
    })
    .unwrap();
    for (id, at, remaining, unlimited, failed) in [
        ("row-time", 100, Some(0.125), false, false),
        ("fetched-time", 200, Some(0.0), false, false),
        ("batch-time", 900, None, true, false),
        ("failure", 900, None, false, true),
    ] {
        let record = records::load(id);
        assert_eq!(record.identity, "", "历史快照不猜测账号身份：{id}");
        assert_eq!(record.queried_at, at, "{id}");
        assert_eq!(record.last_attempt_at, at, "{id}");
        assert_eq!(record.last_success_at, if failed { 0 } else { at }, "{id}");
        assert_eq!(record.remaining, remaining, "{id}");
        assert_eq!(record.unlimited, unlimited, "{id}");
        assert_eq!(
            record.error.as_deref(),
            if failed { Some("HTTP 401") } else { None }
        );
        assert_eq!(
            record.code.as_deref(),
            if failed { Some("unauthorized") } else { None }
        );
    }
    let before = migration_state(&db);
    records::install(Some(db.clone()));
    assert_eq!(migration_state(&db), before);
}

#[test]
fn upstream_sync_snapshot_sources_keep_newer_rows_and_bound_identities() {
    #[allow(dead_code)]
    #[path = "../src/server/core/usage_records.rs"]
    mod records;

    let db = Db::open(std::path::Path::new(":memory:")).unwrap();
    db.with(|conn| {
        conn.execute_batch(
            "INSERT INTO account_usage_records(account_id,usage,remaining,identity,queried_at,last_success_at,last_attempt_at,updated_at)
             VALUES ('newer','{\"available\":50}',50,'',500,500,500,500),
                    ('bound','{\"available\":1}',1,'current-identity',5,5,5,5);"
        ).unwrap();
        put_snapshot(conn,"usageQuerySnapshot",&json!({"at":1000,"results":[
            {"id":"nested-wins","queriedAt":100,"usage":{"available":1}},
            {"id":"standalone-wins","queriedAt":300,"usage":{"available":3}}
        ]}));
        put_snapshot(conn,"backgroundTaskState",&json!({
            "usageQuerySnapshot":{"value":{"at":2000,"results":[
                {"id":"nested-wins","queriedAt":200,"usage":{"available":2}},
                {"id":"standalone-wins","queriedAt":200,"usage":{"available":2}},
                {"id":"newer","queriedAt":400,"usage":{"available":4}},
                {"id":"bound","queriedAt":900,"usage":{"available":9}}
            ]}},
            "usageQuery":{"nextRunAt":7000}
        }));
    }).unwrap();
    records::install(Some(db.clone()));
    for (id, amount, at, identity) in [
        ("nested-wins", 2.0, 200, ""),
        ("standalone-wins", 3.0, 300, ""),
        ("newer", 50.0, 500, ""),
        ("bound", 1.0, 5, "current-identity"),
    ] {
        let record = records::load(id);
        assert_eq!(record.remaining, Some(amount), "{id}");
        assert_eq!(record.queried_at, at, "{id}");
        assert_eq!(record.identity, identity, "{id}");
    }
    db.with(|conn| {
        assert!(rows(conn, "SELECT key FROM kv WHERE key='usageQuerySnapshot'").is_empty());
        assert_eq!(
            saved_json(conn, "backgroundTaskState"),
            json!({"usageQuery":{"nextRunAt":7000}})
        );
    })
    .unwrap();
    let before = migration_state(&db);
    records::install(Some(db.clone()));
    assert_eq!(migration_state(&db), before);
}

fn assert_snapshot_rollback(trigger: &str, install: fn(Option<Db>)) {
    let db = Db::open(std::path::Path::new(":memory:")).unwrap();
    db.with(|conn| {
        conn.execute_batch(
            "INSERT INTO account_usage_records(account_id,usage,remaining,queried_at,last_success_at,last_attempt_at,updated_at)
             VALUES ('first','{\"available\":0.125}',0.125,10,10,10,10);"
        ).unwrap();
        put_snapshot(conn,"usageQuerySnapshot",&json!({"at":100,"results":[
            {"id":"first","usage":{"available":5}}
        ]}));
        put_snapshot(conn,"backgroundTaskState",&json!({
            "usageQuerySnapshot":{"value":{"at":200,"results":[
                {"id":"second","usage":null,"error":"HTTP 401","code":"unauthorized"}
            ]}},
            "workbuddyGrowth":{"nextRunAt":9000}
        }));
        conn.execute_batch(trigger).unwrap();
    }).unwrap();
    let before = migration_state(&db);
    install(Some(db.clone()));
    assert_eq!(
        migration_state(&db),
        before,
        "失败后结果与两个迁移来源须一起恢复"
    );
    db.with(|conn| {
        assert!(conn.is_autocommit());
        conn.execute_batch("DROP TRIGGER fail_snapshot_migration;")
            .unwrap();
    })
    .unwrap();
    install(Some(db.clone()));
    db.with(|conn| {
        assert_eq!(rows(conn,OLD_USAGE).len(),2);
        assert_eq!(rows(conn,"SELECT remaining,queried_at FROM account_usage_records WHERE account_id='first'"),
            vec![vec![Value::Real(5.0),Value::Integer(100)]]);
        assert_eq!(rows(conn,"SELECT usage,error,code,last_success_at,queried_at FROM account_usage_records WHERE account_id='second'"),
            vec![vec![Value::Null,Value::Text("HTTP 401".into()),Value::Text("unauthorized".into()),Value::Integer(0),Value::Integer(200)]]);
        assert!(rows(conn,"SELECT key FROM kv WHERE key='usageQuerySnapshot'").is_empty());
        assert_eq!(saved_json(conn,"backgroundTaskState"),json!({"workbuddyGrowth":{"nextRunAt":9000}}));
    }).unwrap();
    let after = migration_state(&db);
    install(Some(db.clone()));
    assert_eq!(migration_state(&db), after);
}

#[test]
fn upstream_sync_snapshot_row_failure_rolls_back_both_sources_and_retries() {
    #[allow(dead_code)]
    #[path = "../src/server/core/usage_records.rs"]
    mod records;

    assert_snapshot_rollback(
        "CREATE TRIGGER fail_snapshot_migration BEFORE INSERT ON account_usage_records
         WHEN NEW.account_id='second' BEGIN SELECT RAISE(ABORT,'fixture row failure'); END;",
        records::install,
    );
}

#[test]
fn upstream_sync_snapshot_cleanup_failure_rolls_back_imports_and_nested_removal() {
    #[allow(dead_code)]
    #[path = "../src/server/core/usage_records.rs"]
    mod records;

    assert_snapshot_rollback(
        "CREATE TRIGGER fail_snapshot_migration BEFORE DELETE ON kv
         WHEN OLD.key='usageQuerySnapshot' BEGIN SELECT RAISE(ABORT,'fixture cleanup failure'); END;",
        records::install,
    );
}

#[test]
fn upstream_sync_snapshot_new_failure_keeps_last_successful_balance() {
    #[allow(dead_code)]
    #[path = "../src/server/core/usage_records.rs"]
    mod records;

    let db = Db::open(std::path::Path::new(":memory:")).unwrap();
    db.with(|conn| {
        conn.execute_batch(
            "INSERT INTO account_usage_records(account_id,usage,remaining,queried_at,last_success_at,last_attempt_at,updated_at)
             VALUES ('existing','{\"available\":0.125}',0.125,100,100,100,100);"
        ).unwrap();
        put_snapshot(conn,"usageQuerySnapshot",&json!({"at":100,"results":[
            {"id":"from-snapshot","usage":{"available":0.125}},
            {"id":"reversed-sources","queriedAt":200,"usage":null,"error":"HTTP 401","code":"unauthorized"}
        ]}));
        put_snapshot(conn,"backgroundTaskState",&json!({"usageQuerySnapshot":{"value":{
            "at":900,"results":[
                {"id":"existing","queriedAt":200,"usage":null,"error":"HTTP 401","code":"unauthorized"},
                {"id":"from-snapshot","queriedAt":200,"usage":null,"error":"HTTP 401","code":"unauthorized"},
                {"id":"reversed-sources","queriedAt":100,"usage":{"available":0.125}}
            ]
        }}}));
    }).unwrap();
    records::install(Some(db.clone()));
    for id in ["existing", "from-snapshot", "reversed-sources"] {
        let record = records::load(id);
        assert_eq!(record.usage, Some(json!({"available":0.125})), "{id}");
        assert_eq!(record.remaining, Some(0.125), "{id}");
        assert_eq!(record.last_success_at, 100, "{id}");
        assert_eq!(record.last_attempt_at, 200, "{id}");
        assert_eq!(record.queried_at, 200, "{id}");
        assert_eq!(record.error.as_deref(), Some("HTTP 401"));
        assert_eq!(record.code.as_deref(), Some("unauthorized"));
        assert_eq!(record.identity, "");
    }
    db.with(|conn| {
        assert_eq!(
            rows(
                conn,
                "SELECT account_id,updated_at FROM account_usage_records ORDER BY account_id"
            ),
            ["existing", "from-snapshot", "reversed-sources"]
                .into_iter()
                .map(|id| vec![Value::Text(id.into()), Value::Integer(200)])
                .collect::<Vec<_>>()
        );
        assert!(rows(conn, "SELECT key FROM kv WHERE key='usageQuerySnapshot'").is_empty());
        assert_eq!(saved_json(conn, "backgroundTaskState"), json!({}));
    })
    .unwrap();
    let before = migration_state(&db);
    records::install(Some(db.clone()));
    assert_eq!(migration_state(&db), before);
}

#[test]
fn upstream_sync_snapshot_invalid_row_times_fall_back_to_valid_evidence() {
    #[allow(dead_code)]
    #[path = "../src/server/core/usage_records.rs"]
    mod records;

    let db = Db::open(std::path::Path::new(":memory:")).unwrap();
    db.with(|conn| {
        put_snapshot(conn,"backgroundTaskState",&json!({"usageQuerySnapshot":{"value":{
            "at":900,"results":[
                {"id":"zero","queriedAt":0,"usage":{"totalLeft":1,"creditDetails":{"fetchedAt":100}}},
                {"id":"negative","queriedAt":-1,"usage":{"totalLeft":1,"creditDetails":{"fetchedAt":200}}},
                {"id":"invalid-fetched","usage":{"totalLeft":1,"creditDetails":{"fetchedAt":-1}}},
                {"id":"failed-zero","queriedAt":0,"usage":null,"error":"HTTP 401"}
            ]
        }}}));
    }).unwrap();
    records::install(Some(db.clone()));
    for (id, at) in [
        ("zero", 100),
        ("negative", 200),
        ("invalid-fetched", 900),
        ("failed-zero", 900),
    ] {
        let record = records::load(id);
        assert_eq!(record.queried_at, at, "{id}");
        assert_eq!(record.last_attempt_at, at, "{id}");
    }
}
