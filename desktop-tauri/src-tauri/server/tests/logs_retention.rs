use agent2api_server::server::{
    config,
    core::scheduled_tasks,
    db::Db,
    http, logging,
    logs_store::{NewEntry, Query},
    ServerState,
};
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, path: &str, body: Value) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-api-key", "fixture-log-key")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = to_bytes(response.into_body(), 2_000_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn retention_api_persists_immediately_and_never_prunes_on_failed_save() {
    let dir = std::env::temp_dir().join(format!("agent2api-log-api-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    std::env::set_var("AGENT2API_PROXY_API_KEY", "fixture-log-key");
    // 只用无账号临时库；关掉所有真实后台业务与历史客户端导入。
    std::fs::write(dir.join("accounts.json"), "{\"accounts\":[]}").unwrap();
    let db = Db::open(&dir.join("agent2api.db")).unwrap();
    let tasks: serde_json::Map<String, Value> = scheduled_tasks::TASKS
        .iter()
        .map(|t| (t.id.into(), json!({"enabled":false})))
        .collect();
    db.with(|conn| {
        conn.execute(
            "INSERT INTO kv(key,value) VALUES('scheduledTasks',?1)",
            [json!(tasks).to_string()],
        )
    })
    .unwrap()
    .unwrap();
    let state = ServerState::bootstrap(0, "127.0.0.1".parse().unwrap()).unwrap();
    agent2api_server::server::access::attach_db(state.db().cloned());
    let app = http::panel_router(state.clone());
    assert_eq!(
        call(&app, "GET", "/api/retention", Value::Null).await.1["data"]["logMaxEntries"],
        1000
    );
    let store = logging::store_ref().unwrap();
    for limit in [0, 100001, -1] {
        assert_eq!(
            call(
                &app,
                "PUT",
                "/api/retention",
                json!({"logMaxEntries":limit})
            )
            .await
            .0,
            400
        );
    }
    for value in [json!(1.5), json!("1000"), json!(true)] {
        assert_eq!(
            call(
                &app,
                "PUT",
                "/api/retention",
                json!({"logMaxEntries":value})
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        call(
            &app,
            "PUT",
            "/api/retention",
            json!({"logMaxEntries":100000})
        )
        .await
        .0,
        200
    );
    for n in 0..1200 {
        store
            .append(NewEntry {
                level: "info",
                category: "server",
                message: &format!("fixture {n}"),
                data: None,
                ts: None,
            })
            .unwrap();
    }
    let count = store.len();
    assert!(count > 1000);
    let saved = call(&app, "PUT", "/api/retention", json!({"logMaxEntries":1000})).await;
    assert_eq!(saved.0, 200);
    assert!(saved.1["data"]["removedLogs"].as_u64().unwrap() > 0);
    assert_eq!(store.len(), 1000);
    config::reload(); // 从持久化配置重装，而不是只读内存快照。
    assert_eq!(config::retention_settings().log_max_entries, 1000);
    assert_eq!(
        call(&app, "PUT", "/api/retention", json!({})).await.1["data"]["logMaxEntries"],
        1000
    );
    // 复现升级 DB → CONFIG 与保留设置并发；两条路径必须使用一致锁顺序。
    let concurrent_db = state.db().unwrap().clone();
    let (locked, ready) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        concurrent_db.with(|_| {
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            config::retention_settings()
        })
    });
    ready.recv().unwrap();
    let concurrent_save = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        call(&app, "PUT", "/api/retention", json!({"logMaxEntries":1000})),
    )
    .await
    .unwrap();
    assert_eq!(concurrent_save.0, 200);
    reader.join().unwrap();
    let records = store
        .query(&Query {
            limit: Some(500),
            ..Query::default()
        })
        .entries
        .iter()
        .map(|r| r.id)
        .collect::<Vec<_>>();
    state.db().unwrap().with(|conn| conn.execute_batch("CREATE TRIGGER reject_retention BEFORE INSERT ON kv WHEN NEW.key='logMaxEntries' BEGIN SELECT RAISE(ABORT,'fixture'); END;")).unwrap().unwrap();
    assert_eq!(
        call(&app, "PUT", "/api/retention", json!({"logMaxEntries":1}))
            .await
            .0,
        500
    );
    assert_eq!(config::retention_settings().log_max_entries, 1000);
    assert_eq!(
        store
            .query(&Query {
                limit: Some(500),
                ..Query::default()
            })
            .entries
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        records
    );
    state.db().unwrap().with(|conn| conn.execute_batch("DROP TRIGGER reject_retention; CREATE TRIGGER reject_log_delete BEFORE DELETE ON logs BEGIN SELECT RAISE(ABORT,'fixture'); END;")).unwrap().unwrap();
    let partial = call(&app, "PUT", "/api/retention", json!({"logMaxEntries":1})).await;
    assert_eq!(partial.0, 500);
    assert!(partial.1["error"].as_str().unwrap().contains("配置已保存"));
    assert_eq!(config::retention_settings().log_max_entries, 1);
    state
        .db()
        .unwrap()
        .with(|conn| conn.execute_batch("DROP TRIGGER reject_log_delete"))
        .unwrap()
        .unwrap();
    assert_eq!(
        call(&app, "PUT", "/api/retention", json!({"logMaxEntries":1000}))
            .await
            .0,
        200
    );

    let task = scheduled_tasks::TASK_ZCODE_AUTO_CLAIM;
    let result = scheduled_tasks::run_now(state.store(), state.update(), task)
        .await
        .unwrap();
    assert!(result.contains("检查 0 个账号"));
    let logs = call(
        &app,
        "GET",
        &format!("/api/logs?taskId={task}&limit=1"),
        Value::Null,
    )
    .await;
    assert_eq!(logs.0, 200);
    assert!(logs.1["data"]["matched"].as_u64().unwrap() >= 2);
    assert_eq!(logs.1["data"]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(logs.1["data"]["entries"][0]["taskId"], task);
    let before = logs.1["data"]["matched"].clone();
    assert!(
        scheduled_tasks::run_now(state.store(), state.update(), task)
            .await
            .is_err()
    );
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("/api/logs?taskId={task}"),
            Value::Null
        )
        .await
        .1["data"]["matched"],
        before
    );
    assert!(logs.1["data"]["tasks"].get("logsAutoRefresh").is_none());
    let exported = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/logs/download")
                .header("x-api-key", "fixture-log-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exported.status(), 200);
    let text = String::from_utf8(
        to_bytes(exported.into_body(), 2_000_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text.lines().count() > 500);
    assert!(text
        .lines()
        .all(|s| serde_json::from_str::<Value>(s).is_ok()));
    let removed = call(
        &app,
        "DELETE",
        &format!("/api/logs?taskId={task}"),
        Value::Null,
    )
    .await;
    assert_eq!(removed.1["data"]["removed"], before);
    assert_eq!(call(&app, "DELETE", "/api/logs", Value::Null).await.0, 400);
    for filter in [
        "level=unknown",
        "category=unknown",
        "start=not-a-time",
        "end=NaN",
        "all=1&level=unknown",
        "taskId=zcodeAutoClaim&start=bad",
    ] {
        let count = store.len();
        assert_eq!(
            call(&app, "DELETE", &format!("/api/logs?{filter}"), Value::Null)
                .await
                .0,
            400,
            "{filter}"
        );
        assert_eq!(store.len(), count);
    }
    // 旧版 SQL 备份恢复会前向迁移、回填 Unicode 搜索，并立即应用备份中的容量。
    let legacy = Db::open(&dir.join("legacy.db")).unwrap();
    legacy.with(|conn| {
        conn.execute_batch("DROP INDEX idx_logs_task_id; ALTER TABLE logs DROP COLUMN task_id;
            ALTER TABLE logs DROP COLUMN message_search; PRAGMA user_version=16;
            INSERT INTO kv(key,value) VALUES('logMaxEntries','1000');").unwrap();
        for id in 1..=1200 {
            conn.execute("INSERT INTO logs(id,ts,level,category,message) VALUES(?1,?2,'info','server','Ä 中文 restored')",
                rusqlite::params![id, logging::now_ms()]).unwrap();
        }
    }).unwrap();
    let backup = dir.join("legacy.sql");
    agent2api_server::server::db::backup::export_to_file(&legacy, &backup, true).unwrap();
    let old = store.query(&Query::default());
    agent2api_server::server::db::backup::import_from_file(state.db().unwrap(), &backup, &dir)
        .unwrap();
    assert_eq!(store.len(), 1000);
    assert_eq!(config::retention_settings().log_max_entries, 1000);
    let restored = store.query(&Query {
        keyword: Some("ä 中".into()),
        snapshot_epoch: Some(old.snapshot_epoch),
        ..Default::default()
    });
    assert!(restored.snapshot_reset && restored.matched >= 998);
    assert!(restored.entries.iter().all(|entry| entry.task_id.is_none()));
    legacy
        .with(|conn| conn.execute("UPDATE kv SET value='100000' WHERE key='logMaxEntries'", []))
        .unwrap()
        .unwrap();
    let larger = dir.join("restore-larger-capacity.sql");
    agent2api_server::server::db::backup::export_to_file(&legacy, &larger, true).unwrap();
    agent2api_server::server::db::backup::import_from_file(state.db().unwrap(), &larger, &dir)
        .unwrap();
    assert_eq!(config::retention_settings().log_max_entries, 100000);
    assert_eq!(
        store
            .query(&Query {
                keyword: Some("ä 中".into()),
                ..Default::default()
            })
            .matched,
        1200
    );
    let large_page = call(&app, "GET", "/api/logs?limit=10000", Value::Null).await;
    assert!(large_page.1["data"]["entries"].as_array().unwrap().len() > 1000);
    // 恢复已发生但日志清理失败时，原有访问会话与刷新链仍必须失效。
    legacy.with(|conn| conn.execute_batch(
        "UPDATE kv SET value='1' WHERE key='logMaxEntries';
         CREATE TRIGGER restore_reject_log_delete BEFORE DELETE ON logs BEGIN SELECT RAISE(ABORT,'fixture'); END;"
    )).unwrap().unwrap();
    let fault = dir.join("restore-prune-failure.sql");
    agent2api_server::server::db::backup::export_to_file(&legacy, &fault, true).unwrap();
    let access = agent2api_server::server::access::IssuedSession::new_session();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-panel-token", access.access_token.parse().unwrap());
    headers.insert("x-panel-refresh", access.refresh_token.parse().unwrap());
    assert!(agent2api_server::server::access::session_valid(&headers));
    let error =
        agent2api_server::server::db::backup::import_from_file(state.db().unwrap(), &fault, &dir)
            .unwrap_err();
    assert!(error.contains("数据库已恢复") && error.contains("日志裁剪失败"));
    assert!(!agent2api_server::server::access::session_valid(&headers));
    assert!(agent2api_server::server::access::rotate_session(&headers).is_none());
    state
        .db()
        .unwrap()
        .with(|conn| conn.execute_batch("DROP TRIGGER restore_reject_log_delete"))
        .unwrap()
        .unwrap();
    let unauthorized = app
        .oneshot(
            Request::builder()
                .uri("/api/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
}
