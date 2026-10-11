//! 真实模型测试入口的亲和排除回归；loopback 上游与独立子进程隔离全局配置。
use super::{ForwardOutcome, ForwardRequest};
use crate::server::api::pipeline::{record_entry, RecordContext};
use crate::server::core::key_scope::{KeyScope, RoutingPrincipal};
use crate::server::core::providers::catalog::WireTarget;
use crate::server::{config, logging, ServerState};
use axum::http::HeaderMap;
use axum::{extract::Json, routing::post, Router};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[tokio::test]
async fn model_test_entry_does_not_bind_reliable_session() {
    const TEST: &str = "server::core::upstream::model_test_affinity_tests::model_test_entry_does_not_bind_reliable_session";
    const CHILD: &str = "AGENT2API_MODEL_TEST_AFFINITY_CHILD";
    const PROVIDER: &str = "custom-model-test-affinity-fixture";
    const MODEL: &str = "fixture-disabled-model";
    if std::env::var(CHILD).as_deref() != Ok("1") {
        let home = std::env::temp_dir().join(format!(
            "model-test-affinity-{}-{}",
            std::process::id(),
            logging::now_ms()
        ));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENT2API_PROXY_HOME", home)
            .env("AGENT2API_PROXY_API_KEY", "fixture-gateway-key")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "child must execute exactly one test: {stdout}"
        );
        return;
    }
    let state = ServerState::bootstrap(0, "127.0.0.1".parse().unwrap()).unwrap();
    assert!(config::set_account_selection(
        config::AccountSelectionStrategy::CacheAffinity
    ));
    assert_eq!(
        config::account_selection(),
        config::AccountSelectionStrategy::CacheAffinity
    );
    let affinity = state.upstream().affinity.clone();
    assert_eq!(affinity.test_snapshot(), (0, 0, 0, 0));
    let calls = Arc::new(AtomicUsize::new(0));
    let received = calls.clone();
    let observed = affinity.clone();
    let upstream = Router::new().route("/chat/completions", post(move |Json(body): Json<Value>| {
        let received = received.clone();
        let observed = observed.clone();
        async move {
            assert_eq!(observed.test_snapshot(), (0, 0, 0, 0), "model test must not preoccupy during send");
            received.fetch_add(1, Ordering::SeqCst);
            assert_eq!(body["model"], MODEL);
            let content = json!({"model":MODEL,"choices":[{"index":0,"delta":{"content":"fixture-answer"},"finish_reason":null}]});
            let final_frame = json!({"model":MODEL,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":31,"completion_tokens":7,"total_tokens":38}});
            ([("content-type", "text/event-stream")], format!("data: {content}\n\ndata: {final_frame}\n\ndata: [DONE]\n\n"))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    assert!(config::update_raw_field(
        "customProviders",
        json!([{
            "id":PROVIDER,"name":"fixture","protocol":"chat_completions","baseUrl":base,"enabled":true,
            "models":[{"id":MODEL,"enabled":false}],
            "mappings":[{"alias":MODEL,"target":MODEL,"enabled":false,"reasoning":"high"}]
        }])
    ));
    state
        .store()
        .add_custom_account(
            PROVIDER,
            &json!({"apiKey":"fixture-upstream-key"}),
            Some("fixture"),
        )
        .unwrap();
    let principal = RoutingPrincipal::from_environment_key("fixture-gateway-key");
    let body = json!({"model":MODEL,"conversation_id":"fixture-reliable-session", "reasoning_effort":"medium", "messages":[{"role":"user","content":"fixture question"}]});
    let session = super::route_session::RouteSession::from_body(&body, Some(&principal)).unwrap();
    for forced in [None, Some("off")] {
        let telemetry = Arc::new(super::usage::RequestTelemetry::with_id());
        let result = state
            .upstream()
            .forward_model_test(
                ForwardRequest {
                    body: body.clone(),
                    stream: false,
                    dedupe_key: String::new(),
                    client_headers: HeaderMap::new(),
                    telemetry: telemetry.clone(),
                    allowed_providers: Some(KeyScope::provider_only(PROVIDER)),
                    pinned_account: None,
                    route_session: Some(session.clone()),
                    ignore_model_gate: false,
                },
                WireTarget {
                    model: MODEL.into(),
                    reasoning: None,
                    reasoning_override: forced.map(str::to_string),
                },
            )
            .await;
        let (status, error) = if forced.is_none() {
            let response = match result {
                Ok(ForwardOutcome::Completion { body }) => body,
                Ok(ForwardOutcome::Stream { .. }) => panic!("non-stream model test must aggregate"),
                Err(error) => panic!("fixture model test failed: {error:?}"),
            };
            assert_eq!(
                response["choices"][0]["message"]["content"],
                "fixture-answer"
            );
            assert_eq!(response["usage"]["prompt_tokens"], 31);
            assert_eq!(response["usage"]["completion_tokens"], 7);
            (200, None)
        } else {
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("invalid forced reasoning must fail"),
            };
            assert_eq!(error.status_code, 400);
            assert_eq!(
                error.code.as_deref(),
                Some(crate::server::core::model_rules::OVERRIDE_ERROR_CODE)
            );
            (error.status_code as i64, Some(error.message))
        };
        assert!(!telemetry.observes_affinity());
        assert_eq!(
            affinity.test_snapshot(),
            (0, 0, 0, 0),
            "no binding/preoccupation/send/allocation traces before settle"
        );
        record_entry(
            &RecordContext {
                stats: state.request_stats(),
                telemetry,
                started_at: logging::now_ms(),
                model: MODEL.into(),
                client_model: MODEL.into(),
                client_reasoning: "medium".into(),
                status,
                raw_request: None,
                raw_response: None,
                is_test: true,
            },
            error,
        );
        assert_eq!(
            affinity.test_snapshot(),
            (0, 0, 0, 0),
            "successful/error terminal settlement must not bind"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "invalid force must not send another upstream request"
        );
    }
    server.abort();
}
