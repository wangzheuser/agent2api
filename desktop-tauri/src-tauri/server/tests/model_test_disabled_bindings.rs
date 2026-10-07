//! 管理测试使用原始模型；绑定开关、指定账号和三协议仍经过真实路由与本地上游。
use agent2api_server::server::request_stats::RequestQuery;
use agent2api_server::server::{config, http, ServerState};
use axum::{
    body::Body,
    extract::OriginalUri,
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

fn response_stream(path: &str, model: &str) -> String {
    let frames = if path.ends_with("responses") {
        vec![
            json!({"type":"response.created","response":{"id":"fixture","model":model}}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"fixture OK"}),
            json!({"type":"response.completed","response":{"id":"fixture","model":model,"status":"completed","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}),
        ]
    } else if path.ends_with("messages") {
        vec![
            json!({"type":"message_start","message":{"id":"fixture","model":model,"role":"assistant","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"fixture OK"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ]
    } else {
        vec![
            json!({"id":"fixture","model":model,"choices":[{"index":0,"delta":{"content":"fixture OK"},"finish_reason":null}]}),
            json!({"id":"fixture","model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}),
        ]
    };
    let mut stream = frames
        .into_iter()
        .map(|frame| {
            let event = frame
                .get("type")
                .and_then(Value::as_str)
                .map(|kind| format!("event: {kind}\n"))
                .unwrap_or_default();
            format!("{event}data: {frame}\n\n")
        })
        .collect::<String>();
    if path.ends_with("completions") {
        stream.push_str("data: [DONE]\n\n");
    }
    stream
}

async fn request(app: &Router, path: &str, body: Value, authorized: bool) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if authorized {
        builder = builder.header("x-api-key", "fixture-management-key");
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn model_tests_ignore_binding_switches_without_opening_public_routes() {
    let dir = std::env::temp_dir().join(format!(
        "model-tests-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    std::env::set_var("AGENT2API_PROXY_API_KEY", "fixture-management-key");
    let state = ServerState::bootstrap(0, "127.0.0.1".parse().unwrap()).unwrap();
    assert!(config::set_api_key(Some("fixture-management-key".into())));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let received = seen.clone();
    let upstream = Router::new().route(
        "/{*path}",
        post(
            move |OriginalUri(uri): OriginalUri, headers: HeaderMap, Json(body): Json<Value>| {
                let received = received.clone();
                async move {
                    let key = headers
                        .get("authorization")
                        .or_else(|| headers.get("x-api-key"))
                        .and_then(|header| header.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    received.lock().unwrap().push((
                        uri.path().to_string(),
                        key.clone(),
                        body.clone(),
                    ));
                    if key.ends_with("fixture-fail") {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({"error":{"message":"fixture quota exhausted"}})),
                        )
                            .into_response();
                    }
                    (
                        [("content-type", "text/event-stream")],
                        response_stream(uri.path(), body["model"].as_str().unwrap()),
                    )
                        .into_response()
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let mut providers = vec![];
    let mut accounts = vec![];
    for (i, protocol) in ["chat_completions", "responses", "anthropic"]
        .into_iter()
        .enumerate()
    {
        let provider = format!("custom-test-{i}");
        let model = format!("Raw-{i}");
        providers.push(json!({"id":provider,"name":provider,"protocol":protocol,"baseUrl":base,"enabled":true,
            "models":[{"id":model,"enabled":false}],
            "mappings":[{"alias":model,"target":model,"enabled":false,"reasoning":"high"},{"alias":format!("alias-{i}"),"target":model,"enabled":i==0}]}));
        let account = state
            .store()
            .add_custom_account(
                &provider,
                &json!({"apiKey":format!("fixture-upstream-{i}")}),
                Some("被测账号"),
            )
            .unwrap();
        accounts.push(account["id"].as_str().unwrap().to_string());
    }
    assert!(config::update_raw_field(
        "customProviders",
        json!(providers)
    ));
    let app = http::router(state.clone());
    let before = config::current()
        .raw()
        .get("customProviders")
        .cloned()
        .unwrap();
    let sample = json!({"provider":"custom-test-0","model":"Raw-0","account_id":accounts[0]});
    assert_eq!(
        request(&app, "/api/models/test", sample.clone(), false)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for (i, account) in accounts.iter().enumerate() {
        for (path, mut public) in [
            (
                "/v1/chat/completions",
                json!({"messages":[{"role":"user","content":"fixture"}]}),
            ),
            ("/v1/responses", json!({"input":"fixture"})),
            (
                "/v1/messages",
                json!({"messages":[{"role":"user","content":"fixture"}],"max_tokens":32}),
            ),
        ] {
            public["model"] = json!(format!("Raw-{i}"));
            public["test_target"] = json!({"model":format!("Raw-{i}")});
            assert_eq!(
                request(&app, path, public, true).await.0,
                StatusCode::NOT_FOUND,
                "公开协议不能接受测试覆盖目标: {path}"
            );
        }
        let report_requests =
            state.request_stats().usage_summary("today")["overview"]["requests"].clone();
        for stream in [false, true] {
            let payload = json!({"provider":format!("custom-test-{i}"),"model":format!("Raw-{i}"),"account_id":account,"stream":stream,"test_id":format!("fixture-{i}-{stream}")});
            let (status, value) = request(&app, "/api/models/test", payload, true).await;
            assert_eq!(status, StatusCode::OK);
            let result = &value["data"];
            assert_eq!(result["status"], 200, "{value}");
            assert_eq!(result["reply"], "fixture OK", "{value}");
            assert_eq!(result["account_id"], *account);
            assert_eq!(result["upstream_model"], format!("Raw-{i}"));
            assert_eq!(result["prompt_tokens"], 2);
            assert_eq!(result["completion_tokens"], 3);
        }
        assert_eq!(
            state.request_stats().usage_summary("today")["overview"]["requests"],
            report_requests,
            "测试请求不能折入报表"
        );
    }
    assert_eq!(seen.lock().unwrap().len(), 6);
    for (path, _, body) in seen.lock().unwrap().iter() {
        if path.ends_with("responses") {
            assert_eq!(body["reasoning"]["effort"], "high");
        } else if path.ends_with("messages") {
            assert_eq!(body["thinking"]["type"], "enabled");
        } else {
            assert_eq!(body["reasoning_effort"], "high");
        }
    }
    let (_, explicit) = request(&app, "/api/models/test", json!({"provider":"custom-test-0","model":"Raw-0","account_id":accounts[0],"reasoning":"low"}), true).await;
    assert_eq!(explicit["data"]["status"], 200);
    assert_eq!(
        seen.lock().unwrap().last().unwrap().2["reasoning_effort"],
        "low"
    );
    for (path, alias) in [
        (
            "/v1/chat/completions",
            json!({"model":"alias-0","messages":[{"role":"user","content":"fixture"}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"alias-0","input":"fixture"}),
        ),
        (
            "/v1/messages",
            json!({"model":"alias-0","messages":[{"role":"user","content":"fixture"}],"max_tokens":32}),
        ),
    ] {
        assert_eq!(
            request(&app, path, alias, true).await.0,
            StatusCode::OK,
            "已开启的别名保持可用: {path}"
        );
    }
    let models = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let advertised: Value = serde_json::from_slice(
        &axum::body::to_bytes(models.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    let names: Vec<_> = advertised["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();
    assert!(names.contains(&"alias-0"));
    assert!(
        names.iter().all(|name| !name.starts_with("Raw-")),
        "测试不能让关闭的原始名进入广告列表"
    );
    let count = seen.lock().unwrap().len();
    assert_eq!(
        request(
            &app,
            "/api/models/test",
            json!({"provider":"custom-test-0","model":"absent","account_id":accounts[0]}),
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, wrong) = request(
        &app,
        "/api/models/test",
        json!({"provider":"custom-test-0","model":"Raw-0","account_id":accounts[1]}),
        true,
    )
    .await;
    assert_ne!(wrong["data"]["status"], 200);
    assert_eq!(seen.lock().unwrap().len(), count);
    let failed = state
        .store()
        .add_custom_account(
            "custom-test-0",
            &json!({"apiKey":"fixture-fail"}),
            Some("失败账号"),
        )
        .unwrap();
    let (_, failure) = request(
        &app,
        "/api/models/test",
        json!({"provider":"custom-test-0","model":"Raw-0","account_id":failed["id"]}),
        true,
    )
    .await;
    assert_eq!(failure["data"]["status"], 429);
    assert_eq!(
        seen.lock().unwrap().len(),
        count + 1,
        "指定账号失败不能换号"
    );
    let records = state
        .request_stats()
        .query_requests(&RequestQuery::default());
    for i in 0..3 {
        assert!(records["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["model"] == format!("Raw-{i}")
                && entry["isTest"] == true
                && entry["status"] == 200));
    }
    assert_eq!(
        config::current().raw().get("customProviders"),
        Some(&before)
    );
    println!("PASS: disabled bindings, three upstream protocols, both stream modes, reasoning, public 404, auth, pinned-account isolation and test-only accounting");
    server.abort();
}
