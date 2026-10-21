use agent2api_server::server::{
    config,
    core::{
        account_store::AccountStore, auth::AuthService, models::ModelCatalog,
        providers::workbuddy::Region, workbuddy_safety,
    },
    db::Db,
};
use axum::{
    extract::State,
    http::{StatusCode, Uri},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn catalog_rejection_stops_enterprise_fallback_and_persists_restrictions() {
    let dir = std::env::temp_dir().join(format!(
        "workbuddy-catalog-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    let path = dir.join("fixture.db");
    let db = Db::open(&path).unwrap();
    config::init(Some(db.clone()));
    let store = AccountStore::with_db(Some(db));
    let auth = AuthService::for_store(store.clone());
    let state = Arc::new(Mutex::new((
        StatusCode::FORBIDDEN,
        Value::Null,
        Vec::<String>::new(),
    )));
    let router = Router::new().fallback(
        |State(state): State<Arc<Mutex<(StatusCode, Value, Vec<String>)>>>, uri: Uri| async move {
            let mut state = state.lock().unwrap();
            state.2.push(uri.path().to_string());
            if uri.path() != "/v3/config" {
                if let Some(enterprise) = state.1.get("enterprise") {
                    return (StatusCode::OK, Json(enterprise.clone()));
                }
            }
            (state.0, Json(state.1.clone()))
        },
    ).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    for region in [Region::Cn, Region::Intl] {
        for (index, (status, body, paused)) in [
            (
                403,
                json!({"code":11140,"msg":"account has been restricted"}),
                true,
            ),
            (
                200,
                json!({"code":"11140","msg":"account has been restricted"}),
                true,
            ),
            (429, json!({"code":6021,"msg":"queue.full.title"}), false),
            (403, json!({"code":"11-128","msg":"request illegal"}), false),
        ]
        .into_iter()
        .enumerate()
        {
            *state.lock().unwrap() = (StatusCode::from_u16(status).unwrap(), body, Vec::new());
            let account = store.add_account(&json!({
                "provider":region.provider_id(),"edition":region.id(),"endpoint":endpoint,
                "account":{"uid":format!("{}-{index}",region.id()),"enterpriseId":"fixture-enterprise"},
                "auth":{"accessToken":"fixture-token","expiresAt":4_000_000_000_000i64}
            }), None, Some(region.provider_id())).unwrap();
            let id = account["id"].as_str().unwrap();
            let catalog = ModelCatalog::new(region);
            let outcome = catalog
                .refresh_with_current_account(&store, &auth, id)
                .await;
            assert!(!outcome.refreshed);
            assert_eq!(state.lock().unwrap().2, vec!["/v3/config"]);
            let session = store.get_session_by_id(id).unwrap().session;
            let reopened = AccountStore::with_db(Some(Db::open(&path).unwrap()));
            assert_eq!(
                workbuddy_safety::status(&reopened, &session).unwrap()["paused"],
                paused
            );
            if paused {
                assert!(
                    !catalog
                        .refresh_with_current_account(&store, &auth, id)
                        .await
                        .refreshed
                );
                assert_eq!(
                    state.lock().unwrap().2.len(),
                    1,
                    "paused catalog must send nothing"
                );
            }
        }
        for first_status in [200, 404, 500] {
            *state.lock().unwrap() = (
                StatusCode::from_u16(first_status).unwrap(),
                json!({"code":0,"data":{"models":[]},"enterprise":{
                    "code":0,"data":[{"id":"custom:fixture","name":"Fixture","maxOutputTokens":32000}]
                }}),
                Vec::new(),
            );
            let account = store.add_account(&json!({
                "provider":region.provider_id(),"edition":region.id(),"endpoint":endpoint,
                "account":{"uid":format!("{}-fallback-{first_status}",region.id()),"enterpriseId":"fixture-enterprise"},
                "auth":{"accessToken":"fixture-token","expiresAt":4_000_000_000_000i64}
            }), None, Some(region.provider_id())).unwrap();
            let id = account["id"].as_str().unwrap();
            let outcome = ModelCatalog::new(region).refresh_with_current_account(&store, &auth, id).await;
            assert!(outcome.refreshed, "ordinary catalog fallback must remain: {}", outcome.reason);
            assert_eq!(outcome.source, "enterprise");
            assert_eq!(state.lock().unwrap().2, vec![
                "/v3/config", "/console/enterprises/fixture-enterprise/config/models"
            ]);
        }
    }
    server.abort();
}
