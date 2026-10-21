//! 独立测试进程内只初始化一次 config / SQLite；真实分派只访问本地脚本上游。
use agent2api_server::server::{
    config,
    core::{
        account_store::AccountStore,
        auth::AuthService,
        auto_checkin,
        billing::{checkin, BillingService},
        checkin_records,
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
    checkin_records::install(Some(db.clone()));
    let store = AccountStore::with_db(Some(db.clone()));
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
        checkin::run_checkin(&store, &billing, &["loomy".into()], None, "fixture-loomy", "manual")
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
        "manual",
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
    let records = checkin_records::query(&Default::default(), 100, 0);
    assert_eq!(records["total"], 4, "批量入口每个账号只落一条明细");
    let latest = checkin_records::latest_by_account();
    assert_eq!(latest["user-done"]["outcome"], "already");
    assert_eq!(latest["user-expired"]["outcome"], "failed");
    assert_eq!(latest["user-done"]["source"], "manual");
    assert_eq!(latest["user-done"]["provider"], "workbuddy");
    assert_eq!(latest["user-done"]["reason"], "fixture-workbuddy");
    assert_workbuddy_http200_completion(&store, &billing).await;
    assert_activity_ledger(&db);
    let reopened = rusqlite::Connection::open_with_flags(
        dir.join("fixture.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).unwrap();
    let count: i64 = reopened.query_row("SELECT COUNT(*) FROM checkin_records", [], |row| row.get(0)).unwrap();
    assert_eq!(json!(count), checkin_records::query(&Default::default(), 100, 0)["total"]);
}

fn assert_activity_ledger(db: &Db) {
    let activity = json!({
        "id": "fixture-intl", "name": "International",
        "claim": {"success": false, "msg": "活跃保活完成"},
        "activity": {"pokeSucceeded": true}
    });
    checkin_records::record_activity(&activity, "workbuddy-intl", "keepalive");
    let latest = checkin_records::latest_by_account();
    assert_eq!(latest["fixture-intl"]["outcome"], "pending");
    assert_eq!(latest["fixture-intl"]["ok"], false);
    assert_eq!(latest["fixture-intl"]["keptActive"], true);
    assert_eq!(latest["fixture-intl"]["activityCompletedToday"], true);

    let claim = json!({"id": "fixture-intl", "claim": {"success": false, "msg": "当前没有可领取的奖励"}});
    checkin_records::record_activity(&claim, "workbuddy-intl", "claim");
    let latest = checkin_records::latest_by_account();
    assert_eq!(latest["fixture-intl"]["keptActive"], false);
    assert_eq!(latest["fixture-intl"]["activityCompletedToday"], true, "只领取不能覆盖当天保活事实");
    assert_eq!(latest["fixture-intl"]["reason"], "手动领取活动");

    let mut failure = activity.clone();
    failure["claim"] = json!({"status": "auth_expired", "msg": "登录态已过期"});
    checkin_records::record_activity(&failure, "workbuddy-intl", "keepalive");
    assert_eq!(checkin_records::latest_by_account()["fixture-intl"]["outcome"], "failed");
    assert_eq!(checkin_records::latest_by_account()["fixture-intl"]["error"], "登录态已过期");

    db.with_mut(|conn| conn.execute(
        "UPDATE checkin_records SET date = '2000-01-01' WHERE account_id = 'fixture-intl'", [],
    ).unwrap()).unwrap();
    assert_eq!(checkin_records::latest_by_account()["fixture-intl"]["activityCompletedToday"], false);
    assert_eq!(checkin_records::latest_by_account()["fixture-intl"]["outcome"], "failed");
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
            "single",
        )
        .await
        .unwrap();
        assert_eq!(result["total"], 1);
        let latest = checkin_records::latest_by_account();
        assert_eq!(latest[&id]["source"], "single");
        assert_eq!(latest[&id]["outcome"], if completed { "already" } else { "failed" });
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
