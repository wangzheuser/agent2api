//! 独立测试进程内只初始化一次 config / SQLite；真实分派只访问本地脚本上游。
use agent2api_server::server::{
    config,
    core::{
        account_store::AccountStore,
        auth::AuthService,
        auto_checkin,
        billing::{checkin, BillingService},
    },
    db::Db,
};
use serde_json::{json, Value};

#[path = "support/checkin_http.rs"]
mod http_fixture;
use http_fixture::MockUpstream;

fn checkin_at(store: &AccountStore, id: &str) -> i64 {
    store.list_accounts()["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["id"] == id)
        .unwrap()["checkinAt"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn loomy_and_workbuddy_dispatch_preserve_completion_and_failures() {
    let dir = std::env::temp_dir().join(format!(
        "checkin-dispatch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    let db = Db::open(&dir.join("fixture.db")).unwrap();
    config::init(Some(db.clone()));
    let store = AccountStore::with_db(Some(db));
    let billing = BillingService::new(AuthService::for_store(store.clone()));
    let loomy = MockUpstream::spawn(vec![
        (
            200,
            json!({"code": "000000", "data": {"dailyBalance": 5000}}),
        ),
        (401, json!({"code": "expired"})),
    ])
    .await;
    std::env::set_var("LOOMY_POINTS_BASE_URL", &loomy.base);
    for uid in ["good", "bad"] {
        store
            .add_loomy_account(
                &json!({"userId": uid, "session": format!("fixture-loomy-{uid}")}),
                Some(uid),
            )
            .unwrap();
    }
    let wb = MockUpstream::spawn(vec![
        (400, json!({"code": 1001, "msg": "今日已签到"})),
        (401, json!({"code": 1001, "msg": "expired"})),
    ])
    .await;
    for uid in ["done", "expired"] {
        store.add_account(&json!({
            "auth": {"accessToken": format!("fixture-wb-{uid}"), "expiresAt": 4_000_000_000_000i64},
            "account": {"uid": uid}, "edition": "cn", "endpoint": wb.base,
        }), Some(uid), Some("workbuddy")).unwrap();
    }

    let loomy_result =
        checkin::run_checkin(&store, &billing, &["loomy".into()], None, "fixture-loomy")
            .await
            .unwrap();
    assert_eq!(loomy_result["total"], 2);
    assert_eq!(loomy_result["skipped"], 2);
    assert_eq!(loomy_result["succeeded"], 1);
    assert_eq!(
        auto_checkin::completed_account_ids(&loomy_result),
        ["loomy-good"]
    );
    assert_eq!(auto_checkin::failed_account_labels(&loomy_result).len(), 1);
    assert!(checkin_at(&store, "loomy-good") > 0);
    assert_eq!(checkin_at(&store, "loomy-bad"), 0);
    assert_eq!(checkin_at(&store, "user-done"), 0, "未选中账号不标记完成");
    assert!(wb.requests().is_empty(), "Loomy 分派不触发 WorkBuddy");
    for row in loomy_result["results"].as_array().unwrap() {
        assert_eq!(row["provider"], "loomy");
    }
    let requests = loomy.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v1/points/first-login");
        assert_eq!(
            serde_json::from_str::<Value>(&request.body).unwrap(),
            json!({})
        );
    }
    assert_eq!(requests[0].headers["token"], "fixture-loomy-good");
    assert_eq!(requests[1].headers["token"], "fixture-loomy-bad");
    loomy.assert_drained();

    let mut wb_result = checkin::run_checkin(
        &store,
        &billing,
        &["workbuddy".into()],
        None,
        "fixture-workbuddy",
    )
    .await
    .unwrap();
    assert_eq!(wb_result["total"], 2);
    assert_eq!(wb_result["succeeded"], 1);
    assert_eq!(
        auto_checkin::completed_account_ids(&wb_result),
        ["user-done"]
    );
    assert_eq!(auto_checkin::failed_account_labels(&wb_result).len(), 1);
    assert_eq!(wb_result["results"][0]["claim"]["code"], 1001);
    assert_eq!(wb_result["results"][0]["claim"]["alreadyCompleted"], true);
    for row in wb_result["results"].as_array().unwrap() {
        assert_eq!(row["provider"], "workbuddy");
    }
    assert!(checkin_at(&store, "user-done") > 0);
    assert_eq!(checkin_at(&store, "user-expired"), 0);
    // 在实际分派失败行上叠加保活成功，认证错误仍须保留。
    wb_result["results"][1]["activity"] = json!({"pokeSucceeded": true});
    assert_eq!(auto_checkin::failed_account_labels(&wb_result).len(), 1);
    assert_eq!(
        auto_checkin::completed_account_ids(&wb_result),
        ["user-done"]
    );
    let requests = wb.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v2/billing/meter/daily-checkin");
    }
    wb.assert_drained();
    assert_workbuddy_http200_completion(&store, &billing).await;
}

// 沿用本测试进程唯一的 config / SQLite fixture，避免另一个测试初始化全局配置。
async fn assert_workbuddy_http200_completion(store: &AccountStore, billing: &BillingService) {
    for (index, (body, completed)) in [
        (
            json!({"code": 1001, "msg": "今日已签到", "requestId": "fixture-today"}),
            true,
        ),
        (json!({"code": 1001, "msg": "今天已签到，请明天再来"}), true),
        (json!({"code": 401, "msg": "登录态已失效"}), false),
        (json!({"code": 403, "msg": "认证被拒绝"}), false),
        (json!({"code": 1001, "msg": "风控拦截"}), false),
    ]
    .into_iter()
    .enumerate()
    {
        let mock = MockUpstream::spawn(vec![(200, body.clone())]).await;
        let uid = format!("http200-{index}");
        let id = format!("user-{uid}");
        store.add_account(&json!({
            "auth": {"accessToken": format!("fixture-{uid}"), "expiresAt": 4_000_000_000_000i64},
            "account": {"uid": uid}, "edition": "cn", "endpoint": mock.base,
        }), Some(&uid), Some("workbuddy")).unwrap();
        let result = checkin::run_checkin(
            store,
            billing,
            &["workbuddy".into()],
            Some(&id),
            "fixture-http200",
        )
        .await
        .unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["succeeded"], if completed { 1 } else { 0 });
        let row = &result["results"][0];
        assert_eq!(row["id"], id);
        assert_eq!(row["provider"], "workbuddy");
        assert!(row["error"].is_null());
        assert_eq!(row["claim"]["success"], false);
        assert_eq!(row["claim"]["code"], body["code"]);
        assert_eq!(row["claim"]["msg"], body["msg"]);
        assert_eq!(row["claim"]["requestId"], body["requestId"]);
        if completed {
            assert_eq!(row["claim"]["alreadyCompleted"], true);
            assert_eq!(auto_checkin::completed_account_ids(&result), [id.clone()]);
            assert!(auto_checkin::failed_account_labels(&result).is_empty());
            assert!(checkin_at(store, &id) > 0);
        } else {
            assert!(row["claim"].get("alreadyCompleted").is_none());
            assert!(auto_checkin::completed_account_ids(&result).is_empty());
            assert_eq!(auto_checkin::failed_account_labels(&result).len(), 1);
            assert_eq!(checkin_at(store, &id), 0);
        }
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/v2/billing/meter/daily-checkin");
        mock.assert_drained();
    }
}
