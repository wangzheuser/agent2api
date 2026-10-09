//! 合并恢复的管理接口必须实际经过路由及鉴权，而不只保留未接线的处理函数。
use agent2api_server::server::{config, http, ServerState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

#[tokio::test]
async fn restored_management_routes_preserve_auth_validation_and_selection() {
    let dir = std::env::temp_dir().join(format!("management-restore-{}", std::process::id()));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    std::env::set_var("AGENT2API_PROXY_API_KEY", "fixture-key");
    let state = ServerState::bootstrap(0, "127.0.0.1".parse().unwrap()).unwrap();
    assert!(config::set_api_key(Some("fixture-key".into())));
    let app = http::panel_router(state.clone());
    let cases = [
        ("GET", "/api/account-selection", "", 200),
        (
            "PUT",
            "/api/account-selection",
            "{\"accountSelection\":\"priority\"}",
            200,
        ),
        ("GET", "/api/reward-providers", "", 200),
        ("POST", "/api/rewards/configure", "{}", 400),
        ("GET", "/api/rewards/status", "", 400),
        ("POST", "/api/rewards/claim", "{}", 400),
    ];
    for (method, path, body, status) in cases {
        let request = |authorized: bool| {
            let mut builder = Request::builder().method(method).uri(path);
            if authorized {
                builder = builder.header("x-api-key", "fixture-key");
            }
            builder.body(Body::from(body)).unwrap()
        };
        let unauthorized = app.clone().oneshot(request(false)).await.unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED, "{path}");
        let response = app.clone().oneshot(request(true)).await.unwrap();
        assert_eq!(response.status().as_u16(), status, "{path}");
        let bytes = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["success"], status == 200, "{path}: {value}");
    }
    assert_eq!(config::account_selection().as_str(), "priority");
    let invalid = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/account-selection")
                .header("x-api-key", "fixture-key")
                .body(Body::from("{\"accountSelection\":\"unknown\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(config::account_selection().as_str(), "priority");

    // 国际版迁移后仍能读取同身份策略，但不开放国内个人成长领取。
    let result = state.store().add_account(&json!({"edition":"intl", "auth":{"accessToken":"fixture-only"}, "account":{"uid":"fixture-intl"}}), None, Some("workbuddy-intl")).unwrap();
    let id = result["id"].as_str().unwrap();
    let target =
        agent2api_server::server::core::workbuddy_growth::target(&state.store(), id).unwrap();
    assert!(!target.supported);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/accounts/workbuddy/policy?id={id}"))
                .header("x-api-key", "fixture-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["data"]["identity"], target.identity);
}
