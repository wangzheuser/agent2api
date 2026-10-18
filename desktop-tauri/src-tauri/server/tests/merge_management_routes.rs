use agent2api_server::server::{config, http, ServerState};
use axum::{body::{Body, to_bytes}, http::Request};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn merged_management_routes_are_registered_and_authenticated() {
    let dir = std::env::temp_dir().join(format!("merge-management-{}", std::process::id()));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    std::env::set_var("AGENT2API_PROXY_API_KEY", "fixture-merge-key");
    let state = ServerState::bootstrap(0, "127.0.0.1".parse().unwrap()).unwrap();
    assert!(config::set_api_key(Some("fixture-merge-key".into())));
    assert!(config::current().raw()["apiKey"].as_str().unwrap().starts_with("enc1:"));
    let provider = agent2api_server::server::core::custom_providers::create(&serde_json::json!({
        "name":"balance-fixture", "protocol":"chat_completions", "baseUrl":"https://fixture.invalid/v1", "balanceScheme":"new_api", "rewardProfile":"astudio"
    })).unwrap();
    let provider_id = provider["id"].as_str().unwrap();
    assert_eq!(provider["balanceScheme"], "new_api");
    assert_eq!(provider["rewardProfile"], "astudio");
    let account = state.store().add_custom_account(provider_id, &serde_json::json!({
        "apiKey":"fixture-model", "balanceQuery":{"accessToken":"fixture-balance-secret"}, "rewardCredential":"fixture-reward-secret"
    }), None).unwrap();
    assert_eq!(account["balanceQuery"]["hasAccessToken"], true);
    assert!(!account.to_string().contains("fixture-balance-secret"));
    assert!(!account.to_string().contains("fixture-reward-secret"));
    let account_id = account["id"].as_str().unwrap();
    state.store().update_custom_balance_query(account_id, &serde_json::json!({"username":"old-user", "password":"old-password"})).unwrap();
    state.store().update_custom_balance_query(account_id, &serde_json::json!({"username":"new-user", "password":"new-password"})).unwrap();
    assert!(state.store().store_custom_balance_jwt(account_id, "stale-jwt", 999999, "old-user", "old-password").is_err());
    assert!(state.store().custom_balance_query_by_id(account_id).unwrap().sensenova_jwt.is_empty());
    let app = http::router(state);
    for path in ["/api/assistant/config", "/api/proxies/settings", "/api/developer-role-compat"] {
        let denied = app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(denied.status(), 401, "{path}");
        let response = app.clone().oneshot(Request::builder().uri(path).header("x-api-key", "fixture-merge-key").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap();
        assert!(value.is_object(), "{path}: {value}");
    }
    let response = app.clone().oneshot(Request::builder().method("POST").uri("/api/proxies/pool/enable").header("x-api-key", "fixture-merge-key").header("content-type", "application/json").body(Body::from("{}")).unwrap()).await.unwrap();
    assert_eq!(response.status(), 400);
    std::env::remove_var("AGENT2API_PROXY_API_KEY");
    assert!(config::replace_api_keys(serde_json::json!([{ "id":"broken", "key":"enc1:invalid-fixture", "enabled":true }])));
    for key in [None, Some("enc1:invalid-fixture"), Some("fixture-merge-key")] {
        let mut request = Request::builder().uri("/api/accounts");
        if let Some(key) = key { request = request.header("x-api-key", key); }
        let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), 401);
    }
}
