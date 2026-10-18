use agent2api_server::server::db::schema;
use rusqlite::Connection;

#[test]
fn merge_schema_preserves_both_histories_and_repeated_upgrade() {
    for ddl in ["", include_str!("fixtures/local-v14.sql"), include_str!("fixtures/upstream-v15.sql")] {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(ddl).unwrap();
        schema::migrate(&conn).unwrap();
        conn.execute("INSERT INTO accounts(id,provider,priority,data) VALUES('sentinel','custom',0,'{}')", []).unwrap();
        schema::migrate(&conn).unwrap();
        for column in ["cache_creation_tokens", "upstream_credits", "is_test", "route", "client"] {
            let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('requests') WHERE name=?1)", [column], |r| r.get(0)).unwrap();
            assert!(exists, "missing {column}");
        }
        for column in ["identity", "queried_at", "last_success_at", "last_attempt_at"] {
            let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('account_usage_records') WHERE name=?1)", [column], |r| r.get(0)).unwrap();
            assert!(exists, "missing {column}");
        }
        let data: String = conn.query_row("SELECT data FROM accounts WHERE id='sentinel'", [], |r| r.get(0)).unwrap();
        assert_eq!(data, "{}");
        conn.execute_batch("SELECT * FROM checkin_records; SELECT * FROM requests INDEXED BY idx_requests_summary;").unwrap();
    }
}

#[test]
fn merge_schema_upgrades_structure_without_mutating_ciphertext() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("fixtures/upstream-v15.sql")).unwrap();
    conn.execute("INSERT INTO accounts(id,provider,priority,data) VALUES('encrypted','custom',0,'enc1:fixture')", []).unwrap();
    schema::migrate(&conn).unwrap();
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, schema::SCHEMA_VERSION);
    let data: String = conn.query_row("SELECT data FROM accounts WHERE id='encrypted'", [], |r| r.get(0)).unwrap();
    assert_eq!(data, "enc1:fixture");
}
