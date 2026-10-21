use agent2api_server::server::{
    config,
    core::{account_store::AccountStore, billing::growth::GrowthError, task_state, workbuddy_growth},
    db::Db,
};
use serde_json::json;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

#[tokio::test]
async fn legacy_and_new_growth_share_a_lease_and_keep_uncertain_writes() {
    let dir = std::env::temp_dir().join(format!(
        "workbuddy-legacy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("fixture.db")).unwrap();
    config::init(Some(db.clone()));
    task_state::install(Some(db.clone()));
    let store = AccountStore::with_db(Some(db));
    let account = store.add_account(&json!({
        "account":{"uid":"legacy-fixture"},"auth":{"accessToken":"fixture","expiresAt":4_000_000_000_000i64},
        "edition":"cn","endpoint":"http://127.0.0.1:1"
    }), None, Some("workbuddy")).unwrap();
    let id = account["id"].as_str().unwrap().to_string();
    let identity = workbuddy_growth::target(&store, &id).unwrap().identity;
    let calls = Arc::new(AtomicUsize::new(0));
    let (started, waiting) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let first_store = store.clone();
    let first_id = id.clone();
    let first_calls = calls.clone();
    let job = tokio::spawn(async move {
        workbuddy_growth::legacy_action(first_store, first_id, json!({
            "action":"legacy_travel","clientToken":"legacy-first"
        }), move || async move {
            first_calls.fetch_add(1, Ordering::SeqCst);
            started.send(()).unwrap();
            released.await.unwrap();
            Ok(json!({"outcome":"done","credit":0}))
        }).await.unwrap()
    });
    waiting.await.unwrap();
    let other_calls = calls.clone();
    assert_eq!(workbuddy_growth::legacy_action(store.clone(), id.clone(), json!({
        "action":"legacy_travel","clientToken":"legacy-second"
    }), move || async move {
        other_calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"outcome":"done"}))
    }).await.unwrap_err().status, 409);
    let new_result = workbuddy_growth::action(store.clone(), json!({
        "id":id,"action":"claim_available","clientToken":"new-request","expectedIdentity":identity
    })).await;
    assert!(new_result.is_err() || new_result.unwrap()["status"] == "busy");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    assert_eq!(job.await.unwrap()["outcome"], "done");

    tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
    let result = workbuddy_growth::legacy_action(store.clone(), id.clone(), json!({
        "action":"legacy_travel","clientToken":"uncertain-request"
    }), || async { Err(GrowthError::new(502, "fixture response lost")) }).await;
    assert!(result.is_err());
    let saved = task_state::read(&format!("workbuddyGrowth:{identity}")).unwrap();
    assert_eq!(saved.value.unwrap()["legacyPending"]["clientToken"], "uncertain-request");
    let pending = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        workbuddy_growth::state(&store, &id),
    ).await.expect("local recovery state must not wait for upstream").unwrap();
    assert_eq!(pending["legacyPending"]["clientToken"], "uncertain-request");
    assert!(pending["capabilities"].as_array().unwrap().is_empty());
    assert_eq!(pending["errors"][0]["code"], "uncertain");
    assert!(workbuddy_growth::action(store.clone(), json!({
        "id":id,"action":"claim_available","clientToken":"must-not-retry","expectedIdentity":identity
    })).await.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
    assert!(workbuddy_growth::resolve_legacy(&store, &id, &json!({
        "confirmed":true,"expectedIdentity":identity,"clientToken":"stale"
    })).is_err());
    let restored = workbuddy_growth::resolve_legacy(&store, &id, &json!({
        "confirmed":true,"expectedIdentity":identity,"clientToken":"uncertain-request"
    })).unwrap();
    assert_eq!(restored["resolved"], true);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    for drawn in [0, 1] {
        tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
        let token = format!("lottery-unknown-{drawn}");
        let result = workbuddy_growth::legacy_action(store.clone(), id.clone(), json!({
            "action":"legacy_school","clientToken":token
        }), move || async move {
            Ok(json!({"results":[],"lottery":{"success":false,"drawn":drawn,
                "message":"fixture response lost after draw"}}))
        }).await.unwrap();
        assert_eq!(result["lottery"]["success"], false);
        let pending = workbuddy_growth::state(&store, &id).await.unwrap();
        assert_eq!(pending["legacyPending"]["clientToken"], token);
        workbuddy_growth::resolve_legacy(&store, &id, &json!({
            "confirmed":true,"expectedIdentity":identity,"clientToken":token
        })).unwrap();
    }
}
