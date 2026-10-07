//! 独立合并契约：经公开入口验证行为，不随协议实现文件的内联测试一起消失。
use std::collections::HashMap;
use std::sync::Arc;

use agent2api_server::server::core::upstream::usage::RequestTelemetry;
use agent2api_server::server::core::{billing::checkin, protocol, providers, routing};
use agent2api_server::server::db::schema;
use bytes::Bytes;
use rusqlite::Connection;
use serde_json::{json, Value};

fn frame(value: Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

fn events(frames: &[Bytes]) -> Vec<Value> {
    frames
        .iter()
        .flat_map(|frame| {
            std::str::from_utf8(frame)
                .unwrap()
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .map(|data| serde_json::from_str(data).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn anthropic_late_usage_survives_network_splits() {
    let mut input = frame(json!({"choices":[{"delta":{"content":"ok"}}]}));
    input.extend(frame(json!({"choices":[],"usage":{
        "prompt_tokens":47_699,"completion_tokens":2_918,"total_tokens":50_617,
        "prompt_tokens_details":{"cached_tokens":47_616},"cache_creation_input_tokens":20,
    }})));
    input.extend(b"data: [DONE]\n\n");
    for size in [1, 7, input.len()] {
        let mut stream = protocol::anthropic::AnthropicStream::new("fixture");
        let mut frames = Vec::new();
        for part in input.chunks(size) {
            frames.extend(stream.push(part));
        }
        frames.extend(stream.finish());
        let values = events(&frames);
        let start = values
            .iter()
            .find(|v| v["type"] == "message_start")
            .unwrap();
        let end = values
            .iter()
            .find(|v| v["type"] == "message_delta")
            .unwrap();
        assert_eq!(start["message"]["usage"]["input_tokens"], 63);
        assert_eq!(start["message"]["usage"]["cache_read_input_tokens"], 47_616);
        assert_eq!(start["message"]["usage"]["cache_creation_input_tokens"], 20);
        assert_eq!(end["usage"]["output_tokens"], 2_918);
        assert_eq!(
            values
                .iter()
                .filter(|v| v["type"] == "message_start")
                .count(),
            1
        );
        assert_eq!(
            values
                .iter()
                .filter(|v| v["type"] == "message_stop")
                .count(),
            1
        );
        assert!(values.iter().any(|v| v["delta"]["text"] == "ok"));
    }
}

#[test]
fn telemetry_zero_placeholder_does_not_erase_real_usage() {
    let telemetry = Arc::new(RequestTelemetry::new());
    telemetry.report_usage(
        &json!({"prompt_tokens":100,"completion_tokens":9,"total_tokens":109,
        "prompt_tokens_details":{"cached_tokens":80},"cache_creation_input_tokens":0}),
    );
    telemetry.report_usage(&json!({"prompt_tokens":0,"completion_tokens":0}));
    let mut stream = protocol::anthropic::AnthropicStream::new("fixture");
    stream.set_telemetry(telemetry);
    let mut output = stream.push(&frame(json!({"choices":[{"delta":{"content":"ok"}}]})));
    output.extend(stream.finish());
    let values = events(&output);
    let start = values
        .iter()
        .find(|v| v["type"] == "message_start")
        .unwrap();
    let end = values
        .iter()
        .find(|v| v["type"] == "message_delta")
        .unwrap();
    assert_eq!(start["message"]["usage"]["input_tokens"], 20);
    assert_eq!(start["message"]["usage"]["cache_read_input_tokens"], 80);
    assert_eq!(start["message"]["usage"]["cache_creation_input_tokens"], 0);
    assert_eq!(end["usage"]["output_tokens"], 9);
}

#[test]
fn responses_stream_and_collector_have_identical_usage() {
    let request = json!({"model":"fixture","input":"hi"});
    let input = frame(
        json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":100,"completion_tokens":9,"total_tokens":109,
        "prompt_tokens_details":{"cached_tokens":80},"cache_creation_input_tokens":3}}),
    );
    let mut stream = protocol::responses::ResponsesStream::new("fixture", &request);
    let mut output = stream.push(&input);
    output.extend(stream.finish());
    let values = events(&output);
    let completed = values
        .iter()
        .find(|v| v["type"] == "response.completed")
        .unwrap();
    let mut collector = protocol::responses::ResponsesCollector::new();
    collector.push(&input);
    collector.finish();
    let response = collector.into_response("fixture", &request);
    assert_eq!(completed["response"]["usage"], response["usage"]);
    assert_eq!(response["usage"]["input_tokens"], 100);
    assert_eq!(response["usage"]["output_tokens"], 9);
    assert_eq!(response["usage"]["total_tokens"], 109);
    assert_eq!(response["output"][0]["content"][0]["text"], "ok");
    assert!(values
        .iter()
        .any(|v| v["type"] == "response.output_text.delta" && v["delta"] == "ok"));
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        80
    );
}

#[test]
fn anthropic_missing_usage_is_bounded_and_terminates_once() {
    let mut stream = protocol::anthropic::AnthropicStream::new("fixture");
    let text = "x".repeat(1024 * 1024 + 1);
    // 字节边界触发后，正文须在 usage/EOF 前输出；这不声称小响应有时间上限。
    let output = events(&stream.push(&frame(json!({"choices":[{"delta":{"content":text}}]}))));
    assert_eq!(
        output
            .iter()
            .filter(|v| v["type"] == "message_start")
            .count(),
        1
    );
    assert_eq!(
        output
            .iter()
            .find(|v| v["type"] == "message_start")
            .unwrap()["message"]["usage"]["input_tokens"],
        0
    );
    assert_eq!(
        output
            .iter()
            .find(|v| v["type"] == "content_block_delta")
            .unwrap()["delta"]["text"],
        text
    );
    let next = events(&stream.push(&frame(json!({"choices":[{"delta":{"content":"tail"}}]}))));
    assert_eq!(next.len(), 1);
    assert_eq!(next[0]["delta"]["text"], "tail");
    assert!(stream.push(&frame(json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":9,"total_tokens":109}}))).is_empty());
    let tail = events(&stream.finish());
    assert_eq!(
        tail.iter().find(|v| v["type"] == "message_delta").unwrap()["usage"]["output_tokens"],
        9
    );
    assert_eq!(
        tail.iter().filter(|v| v["type"] == "message_stop").count(),
        1
    );
    assert!(stream.finish().is_empty());
}

fn legacy_v6() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("fixtures/requests-v6.sql"))
        .unwrap();
    conn.execute("INSERT INTO requests(id,ts,model,prompt_tokens,completion_tokens) VALUES ('old',1,'fixture',47699,2918)", []).unwrap();
    conn
}

fn assert_request_schema(conn: &Connection) {
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        schema::SCHEMA_VERSION
    );
    for (column, expected) in [
        ("cache_creation_tokens", ("INTEGER".to_string(), 0, None)),
        ("upstream_credits", ("REAL".to_string(), 0, None)),
        ("is_test", ("INTEGER".to_string(), 1, Some("0".to_string()))),
    ] {
        let definition: (String, i64, Option<String>) = conn.query_row(
            "SELECT type, \"notnull\", dflt_value FROM pragma_table_info('requests') WHERE name=?1",
            [column], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(definition, expected, "{column}");
    }
    for (index, column) in [("idx_requests_ts", "ts"), ("idx_requests_id", "id")] {
        let indexed: String = conn
            .query_row("SELECT name FROM pragma_index_info(?1)", [index], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(indexed, column);
    }
}

fn assert_columns_and_data(conn: &Connection) {
    assert_request_schema(conn);
    let old: (i64, String, i64, i64) = conn
        .query_row(
            "SELECT row_id,model,prompt_tokens,completion_tokens FROM requests WHERE id='old'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(old, (1, "fixture".to_string(), 47699, 2918));
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .unwrap(),
        1
    );
}

#[test]
fn migration_upstream_v7_preserves_test_origin_and_counts() {
    let conn = legacy_v6();
    conn.execute_batch("ALTER TABLE requests ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0; UPDATE requests SET is_test=1; PRAGMA user_version=7;").unwrap();
    schema::migrate(&conn).unwrap();
    schema::migrate(&conn).unwrap();
    assert_columns_and_data(&conn);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT is_test FROM requests", [], |r| r.get(0))
            .unwrap(),
        1
    );
}

#[test]
fn migration_local_v7_v8_v9_preserves_zero_null_and_precision() {
    for version in [7, 8, 9] {
        let conn = legacy_v6();
        conn.execute_batch("ALTER TABLE requests ADD COLUMN cache_creation_tokens INTEGER; UPDATE requests SET cache_creation_tokens=0;").unwrap();
        if version >= 8 {
            conn.execute_batch("ALTER TABLE requests ADD COLUMN upstream_credits REAL; UPDATE requests SET upstream_credits=0.012345;").unwrap();
        }
        if version >= 9 {
            conn.execute_batch(
                "ALTER TABLE requests ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0;",
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", version).unwrap();
        schema::migrate(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        assert_columns_and_data(&conn);
        let values: (Option<i64>, Option<f64>, i64) = conn
            .query_row(
                "SELECT cache_creation_tokens,upstream_credits,is_test FROM requests",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            values,
            (Some(0), if version >= 8 { Some(0.012345) } else { None }, 0)
        );
    }
}

#[test]
fn migration_repairs_already_merged_v9_missing_local_column() {
    let conn = legacy_v6();
    conn.execute_batch("ALTER TABLE requests ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0; ALTER TABLE requests ADD COLUMN upstream_credits REAL; PRAGMA user_version=9;").unwrap();
    schema::migrate(&conn).unwrap();
    assert_columns_and_data(&conn);
}

#[test]
fn migration_new_database_has_nullable_counters_and_zero_test_default() {
    let conn = Connection::open_in_memory().unwrap();
    schema::migrate(&conn).unwrap();
    schema::migrate(&conn).unwrap();
    assert_request_schema(&conn);
    conn.execute("INSERT INTO requests(ts,model) VALUES (1,'fixture')", [])
        .unwrap();
    let values: (Option<i64>, Option<f64>, i64) = conn
        .query_row(
            "SELECT cache_creation_tokens,upstream_credits,is_test FROM requests",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(values, (None, None, 0));
}

#[test]
fn migration_failed_repair_keeps_version_and_can_retry() {
    let conn = legacy_v6();
    // 生成列不出现在 table_info：第三个 ALTER 冲突，前两个已执行的 DDL 必须回滚。
    conn.execute_batch("ALTER TABLE requests ADD COLUMN is_test INTEGER GENERATED ALWAYS AS (0) VIRTUAL; PRAGMA user_version=9;").unwrap();
    let error = schema::migrate(&conn).unwrap_err();
    assert!(
        error.to_string().contains("duplicate column name: is_test"),
        "{error}"
    );
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        9
    );
    let columns: i64 = conn.query_row("SELECT COUNT(*) FROM pragma_table_info('requests') WHERE name IN ('cache_creation_tokens','upstream_credits')", [], |r| r.get(0)).unwrap();
    assert_eq!(columns, 0);
    assert!(conn.is_autocommit());
    conn.execute_batch("ALTER TABLE requests DROP COLUMN is_test;")
        .unwrap();
    schema::migrate(&conn).unwrap();
    schema::migrate(&conn).unwrap();
    assert_columns_and_data(&conn);
}

#[test]
fn request_stats_preserves_zero_null_fractional_and_failed_charge() {
    use agent2api_server::server::db::Db;
    use agent2api_server::server::request_stats::{
        NewRequestEntry, RequestQuery, RequestStats, Retention,
    };
    let stats = RequestStats::with_db(
        Some(Db::open(std::path::Path::new(":memory:")).unwrap()),
        || Retention {
            request_days: 30,
            daily_days: 400,
        },
    );
    for (id, status, creation, credits) in [
        ("missing", 200, None, None),
        ("zero", 200, Some(0), Some(0.0)),
        ("fractional", 200, Some(20), Some(0.012345)),
        ("failed", 502, Some(20), Some(1.25)),
    ] {
        let mut entry = NewRequestEntry::new("fixture", status);
        entry.id = id.to_string();
        entry.cache_creation_tokens = creation;
        entry.upstream_credits = credits;
        stats.record(entry);
    }
    let result = stats.query_requests(&RequestQuery::default());
    assert_eq!(result["total"], 4);
    let entries = result["entries"].as_array().unwrap();
    for (id, creation, credits) in [
        ("missing", Value::Null, Value::Null),
        ("zero", json!(0), json!(0.0)),
        ("fractional", json!(20), json!(0.012345)),
        // 失败 token 归零，但真实积分回执必须保留。
        ("failed", json!(0), json!(1.25)),
    ] {
        let row = entries.iter().find(|row| row["id"] == id).unwrap();
        assert_eq!(row["cacheCreationTokens"], creation);
        assert_eq!(row["upstreamCredits"], credits);
    }
}

#[test]
fn routing_never_selects_disabled_cooled_busy_or_excluded_accounts() {
    let account =
        |id: &str| json!({"id":id,"provider":"lobsterai","enabled":true,"maxConcurrent":1});
    let mut disabled = account("disabled");
    disabled["enabled"] = json!(false);
    let mut cooled = account("cooled");
    cooled["rateLimits"] = json!({"fixture-model":{"resetAt":2000}});
    let accounts = vec![
        disabled,
        cooled,
        account("busy"),
        account("excluded"),
        account("good"),
    ];
    let counts = HashMap::from([("busy".to_string(), 1)]);
    let excluded = vec!["excluded".to_string()];
    let keys = routing::CooldownKeys::new("fixture-model");
    assert_eq!(
        routing::pick_account_peek(&accounts, &keys, &counts, &excluded, 1000).unwrap()["id"],
        "good"
    );
    assert!(routing::pick_account_peek(&accounts[..4], &keys, &counts, &excluded, 1000).is_none());
}

#[test]
fn retry_defaults_keep_402_405_and_respect_explicit_empty_list() {
    use agent2api_server::server::config::RetrySettings;
    let defaults = RetrySettings::default();
    assert!(defaults.no_retry(402));
    assert!(defaults.no_retry(405));
    assert!(!defaults.no_retry(500));
    let explicit = RetrySettings {
        no_retry_codes: Arc::from([]),
        ..defaults
    };
    assert!(!explicit.no_retry(402));
    assert!(!explicit.no_retry(405));
}

#[test]
fn native_reward_providers_remain_registered_and_not_custom_profiles() {
    for id in ["minimax-code", "lobsterai", "workbuddy-intl"] {
        assert!(providers::kind_from_id(id).is_some());
        assert!(checkin::supports_checkin(
            &json!({"provider":id,"edition":"intl"})
        ));
    }
    assert!(!checkin::supports_checkin(
        &json!({"provider":"custom-fixture"})
    ));
    assert!(!agent2api_server::server::core::reward_profiles::is_known_profile("minimax-code"));
    assert!(!agent2api_server::server::core::reward_profiles::is_known_profile("lobsterai"));
}

#[test]
fn workbuddy_split_migration_preserves_ids_credentials_and_unknown_fields() {
    use agent2api_server::server::core::account_store::AccountStore;
    use agent2api_server::server::db::Db;
    let db = Db::open(std::path::Path::new(":memory:")).unwrap();
    for (id, edition, priority) in [("cn", "cn", 1), ("intl", "intl", 2)] {
        let data = json!({"id":id,"provider":"workbuddy","edition":edition,
            "uid":"same-user","accessToken":"fixture-token","customField":"keep",
            "priority":priority,"enabled":true,"addedAt":1});
        db.with(|conn| conn.execute(
            "INSERT INTO accounts(id,provider,priority,enabled,added_at,data) VALUES (?1,'workbuddy',?2,1,1,?3)",
            rusqlite::params![id,priority,data.to_string()],
        ).unwrap()).unwrap();
    }
    let store = AccountStore::with_db(Some(db.clone()));
    store.migrate_startup();
    store.migrate_startup();
    for (id, provider) in [("cn", "workbuddy"), ("intl", "workbuddy-intl")] {
        let (projection, raw): (String, String) = db
            .with(|conn| {
                conn.query_row(
                    "SELECT provider,data FROM accounts WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
            })
            .unwrap();
        let data: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(projection, provider);
        assert_eq!(data["provider"], provider);
        assert_eq!(data["id"], id);
        assert_eq!(data["accessToken"], "fixture-token");
        assert_eq!(data["customField"], "keep");
    }
    assert_eq!(
        db.with(|conn| conn
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap())
            .unwrap(),
        2
    );
}
