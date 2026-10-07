//! 独立进程、临时数据库和 loopback 上游：真实三入口、选路与最终记账验收。
use agent2api_server::server::api::pipeline::{record_entry, RecordContext};
use agent2api_server::server::core::custom_providers;
use agent2api_server::server::core::routing::{self, CooldownKeys};
use agent2api_server::server::core::upstream::{
    route_session::RouteSession, usage::RequestTelemetry, ForwardOutcome, ForwardRequest,
};
use agent2api_server::server::request_stats::RequestQuery;
use agent2api_server::server::{config, http, logging, ServerState};
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const KEY: &str = "fixture-gateway-key";

#[derive(Clone, Copy, Debug)]
enum Mode {
    Complete,
    NoUsage,
    InputOnly,
    OutputOnly,
    Empty,
    Error,
    Truncated,
    Tool,
    InvalidTool,
    ZeroUsage,
}

fn frame(kind: &str, value: Value) -> String {
    if kind.is_empty() {
        format!("data: {value}\n\n")
    } else {
        format!("event: {kind}\ndata: {value}\n\n")
    }
}

fn upstream_sse(protocol: &str, model: &str, mode: Mode) -> String {
    let (input, output) = if matches!(mode, Mode::ZeroUsage) {
        (0, 0)
    } else {
        (31, 7)
    };
    if protocol == "chat_completions" {
        let delta = match mode {
            Mode::Empty => json!({}),
            Mode::Tool => {
                json!({"tool_calls":[{"index":0,"id":"fixture-tool-call","type":"function","function":{"name":"fixture_tool","arguments":"{}"}}]})
            }
            Mode::InvalidTool => {
                json!({"tool_calls":[{"index":0,"id":"fixture-tool-call","type":"function","function":{"arguments":"{"}}]})
            }
            _ => json!({"content":"fixture-answer"}),
        };
        let mut result = frame(
            "",
            json!({"id":"fixture","model":model,"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        );
        if matches!(mode, Mode::Error) {
            result += &frame(
                "",
                json!({"error":{"message":"fixture-business-error","type":"upstream_error"}}),
            );
        }
        if !matches!(mode, Mode::NoUsage) {
            let usage = match mode {
                Mode::InputOnly => json!({"prompt_tokens":input}),
                Mode::OutputOnly => json!({"completion_tokens":output}),
                _ => {
                    json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output,"prompt_tokens_details":{"cached_tokens":0}})
                }
            };
            result += &frame(
                "",
                json!({"id":"fixture","model":model,"choices":[],"usage":usage}),
            );
        }
        if !matches!(mode, Mode::Truncated) {
            result += &frame(
                "",
                json!({"id":"fixture","model":model,"choices":[{"index":0,"delta":{},"finish_reason":if matches!(mode,Mode::Tool|Mode::InvalidTool){"tool_calls"}else{"stop"}}]}),
            );
            result += "data: [DONE]\n\n";
        }
        result
    } else if protocol == "anthropic" {
        let mut message =
            json!({"id":"fixture","type":"message","role":"assistant","model":model,"content":[]});
        match mode {
            Mode::NoUsage => {}
            Mode::OutputOnly => message["usage"] = json!({"output_tokens":0}),
            _ => message["usage"] = json!({"input_tokens":input,"output_tokens":0}),
        }
        let mut result = frame(
            "message_start",
            json!({"type":"message_start","message":message}),
        );
        let tool = matches!(mode, Mode::Tool | Mode::InvalidTool);
        let block = if tool {
            json!({"type":"tool_use","id":"fixture-tool-call","name":if matches!(mode,Mode::Tool){"fixture_tool"}else{""},"input":{}})
        } else {
            json!({"type":"text","text":""})
        };
        result += &frame(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":block}),
        );
        let delta = if tool {
            json!({"type":"input_json_delta","partial_json":if matches!(mode,Mode::Tool){"{}"}else{"{"}})
        } else {
            json!({"type":"text_delta","text":if matches!(mode,Mode::Empty){""}else{"fixture-answer"}})
        };
        result += &frame(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":delta}),
        );
        result += &frame(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        );
        if matches!(mode, Mode::Error) {
            result += &frame(
                "error",
                json!({"type":"error","error":{"type":"upstream_error","message":"fixture-business-error"}}),
            );
        }
        if !matches!(mode, Mode::Truncated) {
            let mut delta = json!({"type":"message_delta","delta":{"stop_reason":if tool{"tool_use"}else{"end_turn"}}});
            if !matches!(mode, Mode::NoUsage | Mode::InputOnly) {
                delta["usage"] = json!({"output_tokens":output});
            }
            result += &frame("message_delta", delta);
            result += &frame("message_stop", json!({"type":"message_stop"}));
        }
        result
    } else {
        let mut sequence = 0;
        let mut frame = |kind: &str, mut value: Value| {
            value["sequence_number"] = json!(sequence);
            sequence += 1;
            self::frame(kind, value)
        };
        let tool = matches!(mode, Mode::Tool | Mode::InvalidTool);
        let text = if matches!(mode, Mode::Empty) {
            ""
        } else {
            "fixture-answer"
        };
        let message = if tool {
            json!({"id":"fixture-tool-call","call_id":"fixture-call","type":"function_call","name":if matches!(mode,Mode::Tool){"fixture_tool"}else{""},"arguments":if matches!(mode,Mode::Tool){"{}"}else{"{"}})
        } else {
            json!({"id":"fixture-message","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]})
        };
        let mut result = frame(
            "response.created",
            json!({"type":"response.created","response":{"id":"fixture","model":model,"status":"in_progress","output":[]}}),
        );
        let opening_item = if tool {
            let mut opening = message.clone();
            opening["arguments"] = json!("");
            opening
        } else {
            json!({"id":"fixture-message","type":"message","role":"assistant","content":[]})
        };
        result += &frame(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":0,"item":opening_item}),
        );
        if tool {
            result += &frame(
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fixture-tool-call","output_index":0,"delta":message["arguments"]}),
            );
            result += &frame(
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","item_id":"fixture-tool-call","output_index":0,"name":message["name"],"arguments":message["arguments"]}),
            );
        } else {
            result += &frame(
                "response.content_part.added",
                json!({"type":"response.content_part.added","item_id":"fixture-message","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
            );
            result += &frame(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","item_id":"fixture-message","output_index":0,"content_index":0,"delta":text}),
            );
            result += &frame(
                "response.output_text.done",
                json!({"type":"response.output_text.done","item_id":"fixture-message","output_index":0,"content_index":0,"text":text}),
            );
        }
        result += &frame(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":0,"item":message}),
        );
        if matches!(mode, Mode::Error) {
            result += &frame(
                "error",
                json!({"type":"error","message":"fixture-business-error","error":{"message":"fixture-business-error"}}),
            );
        }
        if !matches!(mode, Mode::Truncated) {
            let mut response =
                json!({"id":"fixture","model":model,"status":"completed","output":[message]});
            match mode {
                Mode::NoUsage => {}
                Mode::InputOnly => response["usage"] = json!({"input_tokens":input}),
                Mode::OutputOnly => response["usage"] = json!({"output_tokens":output}),
                _ => {
                    response["usage"] = json!({"input_tokens":input,"output_tokens":output,"total_tokens":input+output})
                }
            }
            result += &frame(
                "response.completed",
                json!({"type":"response.completed","response":response}),
            );
        }
        result
    }
}

fn request_body(path: &str, model: &str, session: &str, turn: &str, stream: bool) -> Value {
    let mut body = json!({"model":model,"stream":stream,"conversation_id":session});
    if path == "/v1/responses" {
        body["input"] = json!(turn);
    } else {
        body["messages"] = json!([{"role":"user","content":turn}]);
    }
    if path == "/v1/messages" {
        body["max_tokens"] = json!(64);
    }
    body
}

async fn send(app: &Router, path: &str, body: &Value) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("x-api-key", KEY)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn drain(response: Response) -> String {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    text
}

fn last_row(state: &ServerState, model: &str) -> Value {
    state.request_stats().query_requests(&RequestQuery {
        model: Some(model.into()),
        limit: Some(1),
        ..Default::default()
    })["entries"][0]
        .clone()
}

fn notice(row: &Value) -> String {
    row["attemptDetails"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|attempt| attempt["notice"].as_str())
        .collect::<Vec<_>>()
        .join(";")
}

async fn set_strategy(app: &Router, strategy: &str) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/account-selection")
                .header("x-api-key", KEY)
                .header("content-type", "application/json")
                .body(Body::from(json!({"accountSelection":strategy}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    drain(response).await;
}

#[tokio::test]
async fn session_affinity_real_forward_protocols_lifecycle_and_strategy_rollback() {
    let dir = std::env::temp_dir().join(format!(
        "session-affinity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    std::env::set_var("AGENT2API_PROXY_API_KEY", KEY);
    let state = ServerState::bootstrap(0, "127.0.0.1".parse().unwrap()).unwrap();
    assert!(config::set_api_key(Some(KEY.into())));
    let mode = Arc::new(Mutex::new(Mode::Complete));
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let received = seen.clone();
    let mock_mode = mode.clone();
    let mock =
        Router::new().fallback_service(post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let received = received.clone();
            let mode = *mock_mode.lock().unwrap();
            async move {
                let model = body["model"].as_str().unwrap();
                let protocol = if model.ends_with("anthropic") {
                    "anthropic"
                } else if model.ends_with("responses") {
                    "responses"
                } else {
                    "chat_completions"
                };
                let key = headers
                    .get("authorization")
                    .or_else(|| headers.get("x-api-key"))
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                received
                    .lock()
                    .unwrap()
                    .push(json!({"model":model,"account":key,"body":body}));
                (
                    [("content-type", "text/event-stream")],
                    upstream_sse(protocol, model, mode),
                )
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let mut fixtures = Vec::new();
    for protocol in ["chat_completions", "anthropic", "responses"] {
        let model = format!("fixture-{protocol}");
        let provider =
            custom_providers::create(&json!({"name":model,"protocol":protocol,"baseUrl":base}))
                .unwrap();
        let provider_id = provider["id"].as_str().unwrap();
        custom_providers::set_models(provider_id, json!([{"id":model,"enabled":true}]), json!([]))
            .unwrap();
        let ids: Vec<String> = (0..3)
            .map(|index| {
                let account = state
                    .store()
                    .add_custom_account(
                        provider_id,
                        &json!({"apiKey":format!("fixture-{protocol}-{index}")}),
                        Some(&format!("fixture-{index}")),
                    )
                    .unwrap();
                let id = account["id"].as_str().unwrap().to_string();
                state
                    .store()
                    .update_account(&id, &json!({"maxConcurrent":1}))
                    .unwrap();
                id
            })
            .collect();
        fixtures.push((model, ids));
    }
    let app = http::router(state.clone());
    set_strategy(&app, "cacheAffinity").await;

    // 三种真实上游协议 × 三种入口 × 流式/非流式：最终 usage、正文和终态都经过生产链。
    for (model, _) in &fixtures {
        for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
            for stream in [false, true] {
                state.upstream().reset_affinity();
                let session = format!("{model}-{path}-{stream}");
                let text = drain(
                    send(
                        &app,
                        path,
                        &request_body(path, model, &session, "first", stream),
                    )
                    .await,
                )
                .await;
                assert!(
                    text.contains("fixture-answer"),
                    "{model} {path} {stream}: {text}"
                );
                let first = last_row(&state, model);
                assert_eq!(first["promptTokens"], 31, "{first}");
                assert_eq!(first["completionTokens"], 7, "{first}");
                assert!(notice(&first).contains("new_session"), "{first}");
                drain(
                    send(
                        &app,
                        path,
                        &request_body(path, model, &session, "second", stream),
                    )
                    .await,
                )
                .await;
                let second = last_row(&state, model);
                assert_eq!(second["accountId"], first["accountId"]);
                assert!(
                    notice(&second).contains("sticky"),
                    "{model} {path}: {second}"
                );
            }
        }
    }

    let (model, ids) = &fixtures[0];
    let path = "/v1/chat/completions";
    // 缺失不冒充显式零；错误、空流和原始截断即使客户端看到补发 DONE 也不确认。
    for (model, _) in &fixtures {
        for failed in [
            Mode::NoUsage,
            Mode::InputOnly,
            Mode::OutputOnly,
            Mode::Empty,
            Mode::Error,
            Mode::Truncated,
            Mode::InvalidTool,
        ] {
            for stream in [false, true] {
                state.upstream().reset_affinity();
                *mode.lock().unwrap() = failed;
                let session = format!("failure-{failed:?}-{stream}");
                let response = send(
                    &app,
                    path,
                    &request_body(path, model, &session, "first", stream),
                )
                .await;
                let _ = axum::body::to_bytes(response.into_body(), 1_000_000).await;
                *mode.lock().unwrap() = Mode::Complete;
                drain(
                    send(
                        &app,
                        path,
                        &request_body(path, model, &session, "after-failure", stream),
                    )
                    .await,
                )
                .await;
                let after = last_row(&state, model);
                assert!(
                    notice(&after).contains("new_session"),
                    "{model} {failed:?} {stream}: {after}"
                );
                assert!(!notice(&after).contains("sticky"), "{after}");
            }
        }
    }
    for (model, _) in &fixtures {
        for success in [Mode::Tool, Mode::ZeroUsage] {
            for stream in [false, true] {
                state.upstream().reset_affinity();
                *mode.lock().unwrap() = success;
                let session = format!("success-{success:?}");
                let mut body = request_body(path, model, &session, "first", stream);
                if matches!(success, Mode::Tool) {
                    body["tools"] = json!([{"type":"function","function":{"name":"fixture_tool","parameters":{"type":"object","properties":{}}}}]);
                }
                let returned = drain(send(&app, path, &body).await).await;
                if matches!(success, Mode::Tool) && !stream {
                    let completion: Value = serde_json::from_str(&returned).unwrap();
                    let calls = completion["choices"][0]["message"]["tool_calls"]
                        .as_array()
                        .expect("合法工具必须下发完整 tool_calls");
                    assert_eq!(calls.len(), 1, "{model}: {returned}");
                    assert_eq!(
                        calls[0]["function"]["name"], "fixture_tool",
                        "{model}: {returned}"
                    );
                    let arguments: Value =
                        serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap())
                            .unwrap();
                    assert_eq!(arguments, json!({}), "{model}: {returned}");
                    if model.ends_with("responses") {
                        assert_eq!(calls[0]["id"], "fixture-call", "item_id 不得冒充 call_id");
                    }
                }
                let first = last_row(&state, model);
                *mode.lock().unwrap() = Mode::Complete;
                drain(
                    send(
                        &app,
                        path,
                        &request_body(path, model, &session, "second", false),
                    )
                    .await,
                )
                .await;
                let second = last_row(&state, model);
                assert_eq!(first["accountId"], second["accountId"]);
                assert!(
                    notice(&second).contains("sticky"),
                    "{model} {success:?} {stream}: {second}"
                );
            }
        }
    }

    // 旧版严格式与真实 JSON 编码身份均从认证 HTTP 入口提取，正文追加不换绑定。
    for encoded in [
        format!("user_{}_account__session_12345678-1234-1234-1234-123456789abc", "a".repeat(64)),
        json!({"device_id":"87654321-1234-1234-1234-123456789abc","account_uuid":"","session_id":"12345678-1234-1234-1234-123456789abc"}).to_string(),
    ] {
        state.upstream().reset_affinity();
        for (turn, expected) in [("first", "new_session"), ("different appended user content", "sticky")] {
            let mut body = request_body(path,model,"removed",turn,false);
            body.as_object_mut().unwrap().remove("conversation_id");
            body["metadata"] = json!({"user_id":encoded});
            drain(send(&app,path,&body).await).await;
            assert!(notice(&last_row(&state,model)).contains(expected));
        }
    }
    for encoded in [
        "ordinary-user-id",
        "user_short_account__session_12345678-1234-1234-1234-123456789abc",
        "{malformed-json",
    ] {
        state.upstream().reset_affinity();
        let mut body = request_body(path, model, "removed", "unreliable", false);
        body.as_object_mut().unwrap().remove("conversation_id");
        body["metadata"] = json!({"user_id":encoded});
        drain(send(&app, path, &body).await).await;
        assert!(notice(&last_row(&state, model)).contains("no_session"));
    }

    // 真实长流持有连接：健康 owner 繁忙只 overflow；旧 owner 迟到成功不撤回迁移。
    state.upstream().reset_affinity();
    let session = "busy-migration";
    drain(
        send(
            &app,
            path,
            &request_body(path, model, session, "bind", false),
        )
        .await,
    )
    .await;
    let owner = last_row(&state, model)["accountId"]
        .as_str()
        .unwrap()
        .to_string();
    let held = send(
        &app,
        path,
        &request_body(path, model, session, "held-stream", true),
    )
    .await;
    assert!(state
        .upstream()
        .connections()
        .snapshot()
        .iter()
        .any(|(id, count)| id == &owner && *count == 1));
    drain(
        send(
            &app,
            path,
            &request_body(path, model, session, "overflow", false),
        )
        .await,
    )
    .await;
    let overflow = last_row(&state, model);
    assert_ne!(overflow["accountId"], owner);
    assert!(notice(&overflow).contains("busy_overflow"), "{overflow}");
    drain(held).await;
    drain(
        send(
            &app,
            path,
            &request_body(path, model, session, "owner-again", false),
        )
        .await,
    )
    .await;
    assert_eq!(last_row(&state, model)["accountId"], owner);

    let held = send(
        &app,
        path,
        &request_body(path, model, session, "stale-owner", true),
    )
    .await;
    state
        .store()
        .update_account(&owner, &json!({"enabled":false}))
        .unwrap();
    drain(
        send(
            &app,
            path,
            &request_body(path, model, session, "migrate", false),
        )
        .await,
    )
    .await;
    let migrated = last_row(&state, model);
    assert_ne!(migrated["accountId"], owner);
    assert!(notice(&migrated).contains("failover_pending"), "{migrated}");
    state
        .store()
        .update_account(&owner, &json!({"enabled":true}))
        .unwrap();
    drain(held).await;
    drain(
        send(
            &app,
            path,
            &request_body(path, model, session, "after-stale", false),
        )
        .await,
    )
    .await;
    assert_eq!(last_row(&state, model)["accountId"], migrated["accountId"]);

    // Pinned 经真实 forward 与 record_entry，不参与绑定；预览不推进新会话分配。
    let telemetry = Arc::new(RequestTelemetry::with_id());
    let result = state
        .upstream()
        .forward(ForwardRequest {
            body: request_body(path, model, session, "pinned", false),
            stream: false,
            dedupe_key: String::new(),
            client_headers: HeaderMap::new(),
            telemetry: telemetry.clone(),
            allowed_providers: None,
            pinned_account: Some(ids[2].clone()),
            route_session: Some(RouteSession {
                key: session.into(),
            }),
        })
        .await
        .unwrap();
    assert!(matches!(result, ForwardOutcome::Completion { .. }));
    assert_eq!(telemetry.snapshot().account_id, ids[2]);
    record_entry(
        &RecordContext {
            stats: state.request_stats(),
            telemetry: telemetry.clone(),
            started_at: logging::now_ms(),
            model: model.clone(),
            client_model: model.clone(),
            client_reasoning: String::new(),
            status: 200,
            raw_request: None,
            raw_response: None,
            is_test: true,
        },
        None,
    );
    assert!(!notice(&last_row(&state, model)).contains("会话均衡亲和"));
    drain(
        send(
            &app,
            path,
            &request_body(path, model, session, "after-pinned", false),
        )
        .await,
    )
    .await;
    assert_eq!(last_row(&state, model)["accountId"], migrated["accountId"]);
    state.upstream().reset_affinity();
    let pool = state.store().list_accounts();
    for _ in 0..100 {
        let _ = routing::pick_account_peek(
            pool["accounts"].as_array().unwrap(),
            &CooldownKeys::new(model),
            &Default::default(),
            &[],
            logging::now_ms(),
        );
    }
    drain(
        send(
            &app,
            path,
            &request_body(path, model, "after-preview", "first", false),
        )
        .await,
    )
    .await;
    assert_eq!(last_row(&state, model)["accountId"], ids[0]);

    // 静态凭据替换使同一账号的旧身份失效，迁移只由真实完整请求确认。
    state.upstream().reset_affinity();
    let credential_session = "credential-replacement";
    drain(
        send(
            &app,
            path,
            &request_body(path, model, credential_session, "original-key", false),
        )
        .await,
    )
    .await;
    let original = last_row(&state, model);
    assert_eq!(original["accountId"], ids[0]);
    assert!(notice(&original).contains("new_session"), "{original}");
    drain(
        send(
            &app,
            path,
            &request_body(path, model, "credential-pressure", "occupy-b", false),
        )
        .await,
    )
    .await;
    assert_eq!(last_row(&state, model)["accountId"], ids[1]);
    let changes = state
        .store()
        .update_custom_credentials(
            &ids[0],
            &json!({"apiKey":"fixture-chat_completions-0-replaced"}),
        )
        .unwrap();
    assert!(!changes.is_empty(), "必须通过已有存储接口替换 fixture 凭据");
    drain(
        send(
            &app,
            path,
            &request_body(path, model, credential_session, "replacement-key", false),
        )
        .await,
    )
    .await;
    let replacement = last_row(&state, model);
    assert_eq!(replacement["accountId"], ids[2], "{replacement}");
    assert_ne!(replacement["accountId"], original["accountId"]);
    assert!(
        notice(&replacement).contains("failover_pending"),
        "{replacement}"
    );
    assert!(!notice(&replacement).contains("sticky"), "{replacement}");
    assert_eq!(replacement["promptTokens"], 31, "{replacement}");
    assert_eq!(replacement["completionTokens"], 7, "{replacement}");
    drain(
        send(
            &app,
            path,
            &request_body(path, model, credential_session, "confirmed-owner", false),
        )
        .await,
    )
    .await;
    let confirmed = last_row(&state, model);
    assert_eq!(confirmed["accountId"], replacement["accountId"]);
    assert!(notice(&confirmed).contains("sticky"), "{confirmed}");

    // 客户端取消释放未完成预占；切回 balanced 后旧流结束也不复活先前 owner。
    state.upstream().reset_affinity();
    let cancelled = send(
        &app,
        path,
        &request_body(path, model, "cancelled", "first", true),
    )
    .await;
    drop(cancelled);
    drain(
        send(
            &app,
            path,
            &request_body(path, model, "cancelled", "second", false),
        )
        .await,
    )
    .await;
    assert!(notice(&last_row(&state, model)).contains("new_session"));
    assert!(state.upstream().connections().snapshot().is_empty());
    let stale = send(
        &app,
        path,
        &request_body(path, model, "before-rollback", "held", true),
    )
    .await;
    set_strategy(&app, "balanced").await;
    drain(stale).await;
    drain(
        send(
            &app,
            path,
            &request_body(path, model, "before-rollback", "balanced", false),
        )
        .await,
    )
    .await;
    assert!(!notice(&last_row(&state, model)).contains("会话均衡亲和"));
    set_strategy(&app, "cacheAffinity").await;
    drain(
        send(
            &app,
            path,
            &request_body(path, model, "before-rollback", "re-enabled", false),
        )
        .await,
    )
    .await;
    assert!(notice(&last_row(&state, model)).contains("new_session"));
    assert!(state.upstream().connections().snapshot().is_empty());
    assert!(
        seen.lock().unwrap().len() >= 36,
        "必须真正请求 loopback 上游"
    );
    server.abort();
}
