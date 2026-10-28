use super::*;
use axum::{
    body::Body, http::{StatusCode, Uri}, response::Response as HttpResponse, routing::get, Router,
};
use tokio::sync::mpsc;
use std::sync::{Arc, Mutex};

#[test]
fn web_activity_daily_success_and_uncertainty_survive_date_rollover() {
    let completed = json!({"day":"2026-10-21","phase":"success","outcome":{
        "success":true,"conversationId":"fixture","status":"completed","outputChunks":1,
        "turnCompleted":true,"elapsedMs":5,"error":null
    }});
    let restored: Value = serde_json::from_str(&completed.to_string()).unwrap();
    assert!(saved_outcome(Some(&restored), "2026-10-21").unwrap().unwrap().success);
    assert!(saved_outcome(Some(&restored), "2026-10-22").unwrap().is_none());
    assert!(saved_outcome(Some(&restored), "2026-10-20").is_err());
    for phase in ["pending_create", "conversation_created", "uncertain"] {
        let state = json!({"day":"2026-10-21","phase":phase,"conversationId":"fixture"});
        for day in ["2026-10-21", "2026-10-22"] {
            assert!(saved_outcome(Some(&state), day).is_err());
        }
    }
    let mut invalid = completed.clone();
    invalid["outcome"]["outputChunks"] = json!(0);
    assert!(saved_outcome(Some(&invalid), "2026-10-21").is_err());
    let mut invalid = completed;
    invalid["outcome"].as_object_mut().unwrap().remove("turnCompleted");
    assert!(saved_outcome(Some(&invalid), "2026-10-21").is_err());
    let rejected = json!({"phase":"rejected","outcome":{"status":"CREATE_REJECTED","conversationId":null}});
    assert!(saved_outcome(Some(&rejected), "2026-10-21").unwrap().is_none());
}

#[test]
fn only_rejections_without_a_conversation_stay_retryable() {
    for status in ["CREATE_REJECTED", "ACTIVE_LIMIT", "RECONCILED_FAILED"] {
        let state = json!({"day":"2026-10-21","phase":"rejected",
            "outcome":{"status":status,"conversationId":null}});
        assert!(
            saved_outcome(Some(&state), "2026-10-21").unwrap().is_none(),
            "{status} 没留下会话，不能占掉当天机会"
        );
        // 只要记录里带会话 ID，就必须继续按「待核对」处理，不能重复创建。
        let with_id = json!({"day":"2026-10-21","phase":"rejected",
            "outcome":{"status":status,"conversationId":"fixture"}});
        assert!(saved_outcome(Some(&with_id), "2026-10-21").is_err(), "{status}");
    }
    let unknown = json!({"day":"2026-10-21","phase":"rejected",
        "outcome":{"status":"SOMETHING_NEW","conversationId":null}});
    assert!(saved_outcome(Some(&unknown), "2026-10-21").is_err());
}

#[test]
fn web_activity_identity_separates_regions_and_enterprises_not_credentials() {
    let original = json!({"provider":"workbuddy","account":{"uid":"same","enterpriseId":"a"},"auth":{"accessToken":"old"}});
    let key = state_key(&original).unwrap();
    let mut other = original.clone();
    other["auth"]["accessToken"] = json!("new");
    assert_eq!(state_key(&other).unwrap(), key);
    other["provider"] = json!("workbuddy-intl");
    assert_ne!(state_key(&other).unwrap(), key);
    other = original;
    other["account"]["enterpriseId"] = json!("b");
    assert_ne!(state_key(&other).unwrap(), key);
}

#[tokio::test]
async fn web_activity_persists_before_create_and_never_recreates_unknown_result() {
    static DATABASE: std::sync::OnceLock<(crate::server::db::Db, crate::server::db::test_temp::TempDb)> = std::sync::OnceLock::new();
    let (db, _) = DATABASE.get_or_init(|| crate::server::db::test_temp::TempDb::open("web-activity-state"));
    task_state::install(Some(db.clone()));
    let store = AccountStore::with_db(Some(db.clone()));
    for (name, status, payload, phase, first_hits, paused) in [
        ("unknown-502", 502, json!({"msg":"unknown"}), "uncertain", 1, false),
        ("missing-id", 200, json!({"code":0,"data":{}}), "uncertain", 1, false),
        ("created-id", 200, json!({"code":0,"data":{"id":"fixture-created-id"}}), "uncertain", 2, false),
        ("error-with-id", 429, json!({"code":6021,"data":{"id":"fixture-created-id"}}), "uncertain", 1, false),
        ("reject-429", 429, json!({"msg":"rate limited"}), "rejected", 1, false),
        ("reject-403", 403, json!({"msg":"denied"}), "rejected", 1, false),
        ("reject-405", 405, json!({"msg":"method rejected"}), "rejected", 1, false),
        ("restricted", 200, json!({"code":"11140","msg":"account has been restricted"}), "rejected", 1, true),
    ] {
        let session = json!({"provider":"workbuddy-intl","account":{"uid":name},"auth":{"accessToken":"fixture"}});
        let key = state_key(&session).unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        let persisted_key = key.clone();
        let app = Router::new().fallback(move |method: axum::http::Method, uri: Uri| {
            let seen = seen.clone();
            let key = persisted_key.clone();
            let payload = payload.clone();
            async move {
                if uri.path().ends_with("/stop") || uri.path().ends_with("/delete") {
                    return (StatusCode::OK, json!({"code":0,"data":{}}).to_string());
                }
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let state = task_state::read(&key).unwrap().value.unwrap();
                if method == axum::http::Method::GET {
                    assert_eq!(state["phase"], "conversation_created");
                    assert_eq!(state["conversationId"], "fixture-created-id");
                    (StatusCode::BAD_GATEWAY, json!({"msg":"session unavailable"}).to_string())
                } else {
                    assert_eq!(state["phase"], "pending_create");
                    (StatusCode::from_u16(status).unwrap(), payload.to_string())
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(!run_with_origin(&store, &session, "fixture", &origin).await.success, "{name}");
        let value = task_state::read(&key).unwrap().value.unwrap();
        assert_eq!(value["phase"], phase, "{name}");
        if name.contains("with-id") || name == "created-id" {
            assert_eq!(value["outcome"]["conversationId"], "fixture-created-id");
        }
        assert_eq!(crate::server::core::workbuddy_safety::status(&store, &session).unwrap()["paused"], paused);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), first_hits, "{name}");
        task_state::clear_cooldown(&key).unwrap();
        assert!(!run_with_origin(&store, &session, "fixture", &origin).await.success);
        task_state::clear_cooldown(&key).unwrap();
        let restored_store = AccountStore::with_db(Some(db.clone()));
        assert!(!run_with_origin(&restored_store, &session, "fixture", &origin).await.success);
        let expected_hits = if paused {
            first_hits
        } else if phase == "uncertain" && matches!(name, "unknown-502" | "missing-id") {
            first_hits
        } else if phase == "uncertain" {
            first_hits + 2
        } else {
            3
        };
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), expected_hits, "{name}");
        server.abort();
    }
}

#[tokio::test]
async fn web_activity_reconciles_completed_saved_conversation_without_recreating() {
    static DATABASE: std::sync::OnceLock<(crate::server::db::Db, crate::server::db::test_temp::TempDb)> = std::sync::OnceLock::new();
    let (db, _) = DATABASE.get_or_init(|| crate::server::db::test_temp::TempDb::open("web-activity-reconcile"));
    task_state::install(Some(db.clone()));
    let store = AccountStore::with_db(Some(db.clone()));
    let session = json!({"provider":"workbuddy-intl","account":{"uid":"reconcile-success"},"auth":{"accessToken":"fixture"}});
    let key = state_key(&session).unwrap();
    let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
    task_state::store_value(
        &key,
        json!({"day":day,"phase":"conversation_created","conversationId":"fixture-completed-id"}),
    )
    .unwrap();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = hits.clone();
    let persisted_key = key.clone();
    let app = Router::new().fallback(move |method: axum::http::Method, uri: Uri| {
        let seen = seen.clone();
        let key = persisted_key.clone();
        async move {
            if uri.path().ends_with("/stop") || uri.path().ends_with("/delete") {
                return (StatusCode::OK, json!({"code":0,"data":{}}).to_string());
            }
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(method, axum::http::Method::GET);
            let state = task_state::read(&key).unwrap().value.unwrap();
            assert_eq!(state["phase"], "conversation_created");
            assert_eq!(state["conversationId"], "fixture-completed-id");
            (
                StatusCode::OK,
                json!({"code":0,"data":{"status":"completed"}}).to_string(),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let outcome = run_with_origin(&store, &session, "fixture", &origin).await;

    assert!(outcome.success);
    assert!(outcome.reconciled);
    assert_eq!(outcome.status, "completed");
    assert_eq!(outcome.conversation_id.as_deref(), Some("fixture-completed-id"));
    assert_eq!(outcome.output_chunks, 0);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    let value = task_state::read(&key).unwrap().value.unwrap();
    assert_eq!(value["phase"], "success");
    assert_eq!(value["outcome"]["reconciled"], true);
    assert_eq!(value["outcome"]["outputChunks"], 0);
    server.abort();
}

#[test]
fn acp_only_counts_real_answer_and_matching_result() {
    let mut chunks = 0;
    for update in [
        json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}),
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" "}}),
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"image"}}),
    ] {
        assert!(process_event(
            &json!({"method":"session/update","params":{"update":update}}),
            3,
            &mut chunks
        )
        .unwrap()
        .is_none());
    }
    assert_eq!(chunks, 0);
    process_event(
        &json!({"method":"session/update","params":{"update":{
        "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"OK"}}}}),
        3,
        &mut chunks,
    )
    .unwrap();
    assert_eq!(chunks, 1);
    assert!(process_event(&json!({"id":2,"result":{}}), 3, &mut chunks)
        .unwrap()
        .is_none());
    assert_eq!(
        process_event(
            &json!({"id":3,"result":{"stopReason":"end_turn"}}),
            3,
            &mut chunks
        )
        .unwrap(),
        Some(json!({"stopReason":"end_turn"}))
    );
    assert!(process_event(
        &json!({"id":3,"error":{"message":"secret"}}),
        3,
        &mut chunks
    )
    .unwrap_err()
    .contains("业务错误"));
    assert!(process_event(
        &json!({"id":99,"method":"session/request_permission"}),
        3,
        &mut chunks
    )
    .is_err());
}

#[test]
fn validates_console_ids_and_sandbox_addresses() {
    assert_eq!(
        conversation_id(&json!(2101201130699665408u64)).unwrap(),
        "2101201130699665408"
    );
    for id in ["../session", "id?x=y", "", "a/b"] {
        assert!(conversation_id(&json!(id)).is_err());
    }
    assert!(sandbox_url("https://sandbox.workbuddy.ai/acp?route=1").is_ok());
    for url in [
        "http://sandbox.workbuddy.ai/acp",
        "https://127.0.0.1/acp",
        "https://user:pass@example.com/acp",
        "https://localhost/acp",
        "https://example.com/acp#secret",
    ] {
        assert!(sandbox_url(url).is_err(), "{url}");
    }
}

#[test]
fn detects_only_active_conversation_limit_rejections() {
    assert!(active_conversation_limit(
        429,
        &json!({"code":10105,"msg":"maximum number of active conversations(10)"})
    ));
    assert!(active_conversation_limit(
        429,
        &json!({"msg":"you have reached the maximum number of active conversations"})
    ));
    assert!(!active_conversation_limit(
        429,
        &json!({"code":6021,"msg":"rate limited"})
    ));
    assert!(!active_conversation_limit(
        500,
        &json!({"code":10105,"msg":"maximum number of active conversations"})
    ));
}

// 真正用 HTTP/SSE 验证：POST 202 后从长连接读取 RPC 结果，严格按握手顺序发送。
#[tokio::test]
async fn acp_waits_for_rpc_ack_and_consumes_fragmented_sse() {
    let (tx, rx) = mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(32);
    let rx = std::sync::Arc::new(Mutex::new(Some(rx)));
    let methods = std::sync::Arc::new(Mutex::new(Vec::new()));
    let captured = methods.clone();
    let app = Router::new().route(
        "/acp",
        get(move || {
            let rx = rx.clone();
            async move {
                let stream =
                    tokio_stream::wrappers::ReceiverStream::new(rx.lock().unwrap().take().unwrap());
                HttpResponse::builder()
                    .header("Content-Type", "text/event-stream")
                    .header("Acp-Connection-Id", "test-connection")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        })
        .post(
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let tx = tx.clone();
                let captured = captured.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer sandbox-token");
                    assert!(!headers.contains_key("x-user-id"));
                    assert_eq!(headers["acp-connection-id"], "test-connection");
                    captured
                        .lock()
                        .unwrap()
                        .push(body["method"].as_str().unwrap().to_string());
                    let mut data = String::new();
                    if body["id"] == 4 {
                        data.push_str(&format!(
                            "data: {}\r\n\r\n",
                            json!({"method":"session/update","params":{"update":{
                    "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"好"}}}})
                        ));
                    }
                    let result = match body["id"].as_u64().unwrap() {
                        1 => json!({"protocolVersion":1}),
                        2 => json!({}),
                        _ => json!({"stopReason":"end_turn"}),
                    };
                    data.push_str(&format!(
                        "data: {}\n\n",
                        json!({"jsonrpc":"2.0","id":body["id"],"result":result})
                    ));
                    // 实际沙箱的 session/prompt 在 POST 上返回 200 SSE，不走 GET 事件流。
                    if body["id"] == 4 {
                        return HttpResponse::builder()
                            .header("content-type", "text/event-stream")
                            .body(Body::from(data))
                            .unwrap();
                    }
                    // 每字节分片，覆盖 CRLF 与 UTF-8 边界。
                    tokio::spawn(async move {
                        for byte in data.bytes() {
                            if tx.send(Ok(bytes::Bytes::from(vec![byte]))).await.is_err() {
                                break;
                            }
                        }
                    });
                    // 部分沙箱用空 200 确认接收，结果仍在 GET SSE 上返回。
                    HttpResponse::builder()
                        .status(if body["id"] == 2 {
                            StatusCode::OK
                        } else {
                            StatusCode::ACCEPTED
                        })
                        .body(Body::empty())
                        .unwrap()
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let link = url::Url::parse(&format!("http://{}/acp", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::builder().no_proxy().build().unwrap();
    let response = client.get(link.clone()).send().await.unwrap();
    let mut channel = AcpChannel {
        client: &client,
        link,
        token: "sandbox-token",
        connection: "test-connection".into(),
        events: Events::new(response),
        output_chunks: 0,
        safety: None,
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        channel.rpc(1, "initialize", json!({})).await.unwrap();
        channel.rpc(2, "session/load", json!({})).await.unwrap();
        channel
            .rpc(
                3,
                "session/set_model",
                json!({"sessionId":"fixture-session","modelId":"deepseek-v4.1-flash"}),
            )
            .await
            .unwrap();
        assert_eq!(
            channel.rpc(4, "session/prompt", json!({})).await.unwrap()["stopReason"],
            "end_turn"
        );
    })
    .await
    .unwrap();
    assert_eq!(channel.output_chunks, 1);
    assert_eq!(
        *methods.lock().unwrap(),
        vec!["initialize", "session/load", "session/set_model", "session/prompt"]
    );
    server.abort();
}

#[tokio::test]
async fn broken_sse_and_redirects_fail_closed() {
    let app = Router::new()
        .route(
            "/truncated",
            get(|| async { "data: {\"id\":3,\"result\":{}}" }),
        )
        .route("/error", get(|| async { "event: error\ndata: {}\n\n" }))
        .route("/oversized", get(|| async { "a".repeat(MAX_BYTES + 1) }))
        .route(
            "/redirect",
            get(|| async { axum::response::Redirect::temporary("/truncated") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = egress::client_without_redirects(None).unwrap();
    for path in ["truncated", "error", "oversized"] {
        let response = client.get(format!("{base}/{path}")).send().await.unwrap();
        assert!(Events::new(response).next().await.is_err(), "{path}");
    }
    assert!(send(client.get(format!("{base}/redirect")), "test")
        .await
        .is_err());
    server.abort();
}

#[tokio::test]
async fn missing_login_fails_without_network_and_releases_reservation() {
    let session = json!({"account":{"uid":"fixture"}});
    let store = AccountStore::with_db(None);
    let first = run(&store, &session, "fixture-model").await;
    assert!(!first.success);
    assert_eq!(first.error.as_deref(), Some("账号未登录"));
    assert_eq!(run(&store, &session, "fixture-model").await.error, first.error);
}
