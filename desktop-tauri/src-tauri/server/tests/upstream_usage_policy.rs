use agent2api_server::server::{config, db::Db, logging};
use agent2api_server::server::core::{account_store::AccountStore, scheduled_tasks, task_state, usage_query, usage_records};
use serde_json::{json, Value};
use std::collections::HashMap;

#[test]
fn upstream_usage_identity_and_unknown_balance_do_not_block_unrelated_accounts() {
    let account = json!({"id":"fixture","provider":"workbuddy","uid":"one","edition":"cn","addedAt":1,
        "lowBalance":{"mode":"skip","threshold":1}});
    let mut facts = HashMap::new();
    facts.insert("fixture".into(), usage_records::BalanceFact { identity: usage_records::identity(&account), remaining: Some(0.0) });
    assert!(usage_records::balance_blocked(&account, &facts));
    let mut replacement = account.clone(); replacement["uid"] = json!("two");
    assert!(!usage_records::balance_blocked(&replacement, &facts));
    replacement = account.clone(); replacement["addedAt"] = json!(2);
    assert!(!usage_records::balance_blocked(&replacement, &facts));
    replacement = account.clone(); replacement["accessToken"] = json!("rotated-fixture");
    assert_eq!(usage_records::identity(&replacement), usage_records::identity(&account));
    let mut cline = json!({"id":"cline-free-desktop","provider":"cline-free","account":" usr-a ","addedAt":1});
    let cline_identity = usage_records::identity(&cline);
    cline["account"] = json!("usr-a");
    assert_eq!(usage_records::identity(&cline), cline_identity);
    cline["account"] = json!("usr-b");
    assert_ne!(usage_records::identity(&cline), cline_identity);
    replacement.as_object_mut().unwrap().remove("lowBalance");
    assert!(!usage_records::balance_blocked(&replacement, &facts));
    assert_eq!(usage_records::extract_remaining(&json!({"available":null})), (None, false));
    assert_eq!(usage_records::extract_remaining(&json!({"totalLeft":null,"available":0})), (None, false));
    assert_eq!(usage_records::extract_remaining(&json!({"available":0,"unlimited":true})), (None, true));
    facts.get_mut("fixture").unwrap().remaining = Some(1.0);
    assert!(!usage_records::balance_blocked(&account, &facts));
}

#[tokio::test]
async fn upstream_usage_records_and_schedule_preserve_local_controls() {
    // 单独 integration target 隔离 OnceLock；不启动后台网络任务。
    let dir = std::env::temp_dir().join(format!("upstream-usage-{}-{}", std::process::id(), logging::now_ms()));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    let db = Db::open(&dir.join("fixture.db")).unwrap();
    config::init(Some(db.clone()));
    task_state::install(Some(db.clone()));
    usage_records::install(Some(db.clone()));
    let store = AccountStore::with_db(Some(db.clone()));
    let account = store.add_account(&json!({"account":{"uid":"usage-fixture"},"auth":{"accessToken":"fixture"},"edition":"cn"}), Some("fixture"), Some("workbuddy")).unwrap();
    let id = account["id"].as_str().unwrap();
    let success = |remaining| usage_records::UsageOutcome { usage: Some(json!({"available":remaining})), error: None, code: None };
    let failure = usage_records::UsageOutcome { usage: None, error: Some("fixture failure".into()), code: None };
    let now = logging::now_ms();
    assert!(usage_records::write_result(&account, &success(5), now));
    assert!(usage_records::write_result(&account, &failure, now + 2));
    assert_eq!(usage_records::load(id).remaining, Some(5.0));
    assert_eq!(usage_records::load(id).last_success_at, now);
    assert!(!usage_records::write_result(&account, &success(0), now + 1));
    let snapshot = usage_query::snapshot(&store);
    assert_eq!(snapshot["results"][0]["usage"], Value::Null);
    assert_eq!(snapshot["results"][0]["error"], "fixture failure");
    // 另一实例查询后，本实例的周期同步也能恢复路由，不要求再发上游请求。
    let other = rusqlite::Connection::open(db.file()).unwrap();
    other.execute("UPDATE account_usage_records SET remaining=17,usage='{\"available\":17}' WHERE account_id=?1",[id]).unwrap();
    usage_records::sync_facts();
    assert_eq!(usage_records::balance_facts()[id].remaining,Some(17.0));
    let mut replaced = account.clone(); replaced["uid"] = json!("another-user");
    assert!(usage_records::write_result(&replaced, &failure, now + 3));
    assert_eq!(usage_records::load(id).remaining, None);
    assert_eq!(usage_query::snapshot(&store)["results"], json!([]));

    assert_eq!(usage_records::query_settings(None)["interval"], 600);
    scheduled_tasks::configure(&store, "usageQuery", config::IntervalTaskPatch { enabled: Some(false), interval: None }).unwrap();
    assert_eq!(usage_records::query_interval_of(&json!({"usageQuery":{"enabled":true,"interval":30}})), None);
    scheduled_tasks::configure(&store, "usageQuery", config::IntervalTaskPatch { enabled: Some(true), interval: Some(7) }).unwrap();
    assert_eq!(usage_records::query_settings(None)["interval"], 420);
    assert_eq!(usage_records::query_interval_of(&json!({"usageQuery":{"enabled":true,"interval":30}})), Some(30));
    assert!(store.update_account(id, &json!({"usageQuery":{"enabled":"false"}})).is_err());
    assert!(store.update_account(id, &json!({"usageQuery":{"enabled":true,"interval":29}})).is_err());
    let account_key = format!("account-usage:{}",usage_records::identity(&account));
    db.with(|conn| {
        let raw:String=conn.query_row("SELECT value FROM kv WHERE key='backgroundTaskState'",[],|row|row.get(0)).unwrap();
        let mut states:Value=serde_json::from_str(&raw).unwrap();
        states[&account_key]=json!({"lastAttemptAt":now+120_000,"lastRunAt":now+120_000,"nextRunAt":now+540_000,"retryAt":now+140_000,"intervalMs":420_000});
        conn.execute("UPDATE kv SET value=?1 WHERE key='backgroundTaskState'",[states.to_string()]).unwrap();
    }).unwrap();
    let status=scheduled_tasks::task_by_id(&store,"usageQuery");
    assert!(!task_state::read(&account_key).unwrap().clock_needs_adjustment());
    assert!(status["retryAt"].as_i64().unwrap() <= logging::now_ms()+21_000);
    task_state::clear_cooldown(&account_key).unwrap();
    assert_eq!(scheduled_tasks::task_by_id(&store,"usageQuery")["retryAt"],Value::Null);
    store.update_account(id, &json!({"usageQuery":{"enabled":false,"interval":"ignored"}})).unwrap();
    assert_eq!(scheduled_tasks::task_by_id(&store, "usageQuery")["nextRunAt"], Value::Null);

    let key = "fixture-usage-schedule";
    let mut old = task_state::TaskState::default();
    old.last_attempt_at = now - 20_000; old.last_run_at = now - 10_000;
    old.retry_at = now + 120_000; old.failures = 2;
    task_state::initialize_schedule(key, &old, 30_000).unwrap();
    assert_eq!(task_state::read(key).unwrap().due_at(), old.retry_at);
    assert!(matches!(task_state::claim(key, 30_000, false, task_state::ManualBackoff::Respect, 1000).unwrap(), task_state::Claim::Deferred(_)));
    let task_state::Claim::Acquired(guard) = task_state::claim(key, 30_000, true, task_state::ManualBackoff::Bypass, 1000).unwrap() else { panic!("manual retry") };
    assert!(matches!(task_state::claim(key, 30_000, true, task_state::ManualBackoff::Bypass, 1000).unwrap(), task_state::Claim::Deferred(_)));
    task_state::initialize_schedule(key, &old, 60_000).unwrap();
    assert!(task_state::read(key).unwrap().running());
    guard.finish(false, "fixture failure".into(), None, 0, 30_000).unwrap();
    let state = task_state::read(key).unwrap();
    assert_eq!(state.failures, 3);
    assert!(!state.running());
    task_state::initialize_schedule(key, &old, 60_000).unwrap();
    assert!(task_state::read(key).unwrap().due_at() >= state.retry_at);
}
