use super::*;
use axum::{
    body::Body, http::StatusCode, response::Response as HttpResponse, routing::get, Router,
};
use tokio::sync::mpsc;

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
                    if body["id"] == 3 {
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
                    if body["id"] == 3 {
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
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        channel.rpc(1, "initialize", json!({})).await.unwrap();
        channel.rpc(2, "session/load", json!({})).await.unwrap();
        assert_eq!(
            channel.rpc(3, "session/prompt", json!({})).await.unwrap()["stopReason"],
            "end_turn"
        );
    })
    .await
    .unwrap();
    assert_eq!(channel.output_chunks, 1);
    assert_eq!(
        *methods.lock().unwrap(),
        vec!["initialize", "session/load", "session/prompt"]
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
    let first = run(&session, "fixture-model").await;
    assert!(!first.success);
    assert_eq!(first.error.as_deref(), Some("账号未登录"));
    assert_eq!(run(&session, "fixture-model").await.error, first.error);
}
