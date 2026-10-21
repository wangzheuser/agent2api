use agent2api_server::server::{
    core::{account_store::AccountStore, auth::AuthService, billing::{BillingService, WorkbuddyActivity}, task_state, workbuddy_safety, workbuddy_policy},
    db::Db,
};
use axum::{http::{StatusCode, Uri}, response::IntoResponse, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn activity_stops_after_rejection_and_persists_daily_and_uncertain_boundaries() {
    let dir = std::env::temp_dir().join(format!("workbuddy-activity-safety-{}-{}", std::process::id(), chrono::Utc::now().timestamp_millis()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("fixture.db")).unwrap();
    task_state::install(Some(db.clone()));
    let store = AccountStore::with_db(Some(db.clone()));
    let billing = BillingService::new(AuthService::for_store(store.clone()));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let response = Arc::new(Mutex::new((String::new(), 200u16, Value::Null)));
    let calls = seen.clone();
    let reply = response.clone();
    let app = Router::new().fallback(move |uri: Uri| {
        let calls = calls.clone();
        let reply = reply.clone();
        async move {
            let path = uri.path().to_string();
            calls.lock().unwrap().push(path.clone());
            let (target, status, body) = reply.lock().unwrap().clone();
            if path == target {
                if let Some(text) = body.as_str() {
                    return (StatusCode::from_u16(status).unwrap(), [("content-type", "text/event-stream")], text.to_string()).into_response();
                }
                return (StatusCode::from_u16(status).unwrap(), Json(body)).into_response();
            }
            if path.ends_with("get-user-resource") {
                return Json(json!({"code":0,"data":{"accounts":[]}})).into_response();
            }
            if path.ends_with("checkin-activity-status") {
                return Json(json!({"code":0,"data":{"active":target.ends_with("daily-checkin"),"todayCheckedIn":false}})).into_response();
            }
            (StatusCode::BAD_REQUEST, Json(json!({"code":"unsupported_model","msg":"model not supported"}))).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let session = |uid: &str| json!({"provider":"workbuddy-intl","edition":"intl","endpoint":base,
        "account":{"uid":uid},"auth":{"accessToken":"fixture-only"}});
    for (index, path, status, body, paused) in [
        (0, "/v2/billing/meter/get-user-resource", 403, json!({"code":11140,"msg":"request illegal"}), true),
        (1, "/billing/meter/checkin-activity-status", 401, json!({"msg":"expired"}), false),
        (2, "/billing/meter/checkin-activity-status", 403, json!({"msg":"denied"}), false),
        (3, "/v2/chat/completions", 429, json!({"code":6021,"msg":"queue.full"}), false),
        (4, "/v2/chat/completions", 200, json!({"code":"11140","msg":"request illegal"}), true),
        (5, "/v2/chat/completions", 400, json!({"code":"11-128","msg":"unapproved channel"}), false),
        (6, "/v2/chat/completions", 401, json!({"msg":"expired"}), false),
        (7, "/v2/chat/completions", 403, json!({"msg":"denied"}), false),
        (8, "/billing/meter/checkin-activity-status", 200, json!({"code":10105,"msg":"not available"}), false),
        (9, "/v2/chat/completions", 200, json!("data: {\"code\":11140,\"msg\":\"request illegal\"}\n\n"), true),
        (10, "/v2/billing/meter/daily-checkin", 200, json!({"code":9876,"msg":"fixture business rejected"}), false),
        (11, "/v2/billing/meter/daily-checkin", 200, json!({"code":"unknown_business_failure","msg":"fixture business rejected"}), false),
    ] {
        seen.lock().unwrap().clear();
        *response.lock().unwrap() = (path.into(), status, body);
        let account = session(&format!("reject-{index}"));
        let result = billing.workbuddy_daily_activity(&account, WorkbuddyActivity::Full).await;
        assert_eq!(result["activity"]["stopped"], true, "{result}");
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.last().unwrap(), path);
        assert_eq!(calls.iter().filter(|call| call.as_str() == path).count(), 1, "{calls:?}");
        assert_eq!(workbuddy_safety::status(&store, &account).unwrap()["paused"], paused);
        if path.ends_with("daily-checkin") {
            assert_eq!(calls.len(), 3, "{calls:?}");
            assert_eq!(result["claim"]["success"], false);
            assert_eq!(result["claim"]["msg"], "fixture business rejected");
            assert_eq!(result["claim"]["code"], if index == 10 { 9876 } else { -1 });
        }
        if paused {
            let before = calls.len();
            assert!(billing.query_usage(Some(&account), None).await.is_err());
            assert_eq!(seen.lock().unwrap().len(), before);
        }
    }
    for (index, path, status, body) in [
        (0, "/v2/chat/completions", 429, json!({"msg":"rate limited"})),
        (1, "/v2/billing/meter/daily-checkin", 200, json!({"code":9876,"msg":"fixture business rejected"})),
    ] {
        seen.lock().unwrap().clear();
        *response.lock().unwrap() = (path.into(), status, body);
        let account = session(&format!("combined-reject-{index}"));
        let result = billing.checkin_and_report(Some(&account), None).await.unwrap();
        assert_eq!(result["activity"]["stopped"], true, "{result}");
        assert_eq!(result["usage"]["status"], "not_queried");
        assert!(result["usage"]["error"].is_string());
        assert!(result["usage"].get("totalLeft").is_none());
        assert_eq!(workbuddy_safety::status(&store, &account).unwrap()["paused"], false);
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls.last().unwrap(), path);
        assert_eq!(calls.iter().filter(|path| path.ends_with("get-user-resource")).count(), 1);
    }
    for (index, body) in [
        (0, json!({"code":0,"data":{"credit":5}})),
        (1, json!({"code":9877,"msg":"今日已签到"})),
    ] {
        seen.lock().unwrap().clear();
        *response.lock().unwrap() = ("/v2/billing/meter/daily-checkin".into(), 200, body);
        let result = billing.workbuddy_daily_activity(&session(&format!("claim-completed-{index}")), WorkbuddyActivity::Claim).await;
        assert_ne!(result["activity"]["stopped"], true, "{result}");
        assert_eq!(result["claim"][if index == 0 { "success" } else { "alreadyCompleted" }], true);
        assert_eq!(seen.lock().unwrap().as_slice(), [
            "/v2/billing/meter/get-user-resource", "/billing/meter/checkin-activity-status",
            "/v2/billing/meter/daily-checkin", "/v2/billing/meter/get-user-resource"
        ]);
    }
    for (index, active, checked_in) in [(0, false, false), (1, true, true)] {
        seen.lock().unwrap().clear();
        *response.lock().unwrap() = ("/billing/meter/checkin-activity-status".into(), 200,
            json!({"code":0,"data":{"active":active,"todayCheckedIn":checked_in}}));
        let result = billing.workbuddy_daily_activity(&session(&format!("claim-normal-skip-{index}")), WorkbuddyActivity::Claim).await;
        assert_ne!(result["activity"]["stopped"], true, "{result}");
        assert_eq!(result["claim"]["alreadyCompleted"] == true, checked_in);
        assert_eq!(seen.lock().unwrap().as_slice(), [
            "/v2/billing/meter/get-user-resource", "/billing/meter/checkin-activity-status",
            "/v2/billing/meter/get-user-resource"
        ]);
    }
    seen.lock().unwrap().clear();
    *response.lock().unwrap() = ("/v2/chat/completions".into(), 400, json!({"code":"unsupported_model","msg":"model not supported"}));
    let result = billing.workbuddy_daily_activity(&session("unsupported"), WorkbuddyActivity::Keepalive).await;
    assert_eq!(result["activity"]["stopped"], true);
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.iter().filter(|path| path.ends_with("/chat/completions")).count(), agent2api_server::server::core::billing::keepalive::models().len());
    assert!(!calls.iter().any(|path| path.contains("checkin-activity-status")));
    for phase in ["pending_create", "conversation_created", "uncertain"] {
        seen.lock().unwrap().clear();
        let account = session(phase);
        task_state::store_value(&format!("workbuddy-web-activity:{}", workbuddy_policy::identity(&account).unwrap()), json!({"day":"2020-01-01","phase":phase})).unwrap();
        assert_eq!(billing.workbuddy_daily_activity(&account, WorkbuddyActivity::Full).await["activity"]["stopped"], true);
        assert!(seen.lock().unwrap().is_empty());
    }
    seen.lock().unwrap().clear();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let completed = json!({"day":today,"phase":"success","outcome":{
        "success":true,"conversationId":"fixture","status":"completed","outputChunks":1,"turnCompleted":true,"elapsedMs":1,"error":null}});
    task_state::store_value(&format!("workbuddy-web-activity:{}", workbuddy_policy::identity(&session("completed")).unwrap()), completed).unwrap();
    let restarted = BillingService::new(AuthService::for_store(AccountStore::with_db(Some(Db::open(&dir.join("fixture.db")).unwrap()))));
    let result = restarted.workbuddy_daily_activity(&session("completed"), WorkbuddyActivity::Keepalive).await;
    assert_eq!(result["activity"]["pokeSucceeded"], true, "{result}");
    assert!(seen.lock().unwrap().iter().all(|path| path.ends_with("get-user-resource")));

    seen.lock().unwrap().clear();
    let lease = match task_state::claim(&format!("workbuddy-daily-activity:{}", workbuddy_policy::identity(&session("concurrent")).unwrap()), 0, true, task_state::ManualBackoff::Respect, 0).unwrap() {
        task_state::Claim::Acquired(lease) => lease,
        _ => panic!("fixture lease unavailable"),
    };
    assert_eq!(billing.workbuddy_daily_activity(&session("concurrent"), WorkbuddyActivity::Keepalive).await["activity"]["stopped"], true);
    assert!(seen.lock().unwrap().is_empty());
    drop(lease);
    db.with_mut(|connection| connection.execute("DROP TABLE kv", [])).unwrap().unwrap();
    assert_eq!(billing.workbuddy_daily_activity(&session("database-failed"), WorkbuddyActivity::Keepalive).await["activity"]["stopped"], true);
    assert!(seen.lock().unwrap().is_empty());
    server.abort();
}
