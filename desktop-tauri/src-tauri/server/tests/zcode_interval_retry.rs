use agent2api_server::server::{
    core::task_state::{self, Claim, ManualBackoff, TaskState},
    db::Db,
    logging,
};
use serde_json::json;

#[tokio::test]
async fn zcode_interval_retry_recovers_old_backoff_without_losing_business_windows_or_leases() {
    let dir = std::env::temp_dir().join(format!(
        "zcode-interval-{}-{}",
        std::process::id(),
        logging::now_ms()
    ));
    std::env::set_var("AGENT2API_PROXY_HOME", &dir);
    let db = Db::open(&dir.join("fixture.db")).unwrap();
    task_state::install(Some(db));
    let interval = 600_000;
    let now = logging::now_ms();
    let key = "zcodeClaim:fixture";
    let mut old = TaskState::default();
    old.last_attempt_at = now - 2 * interval;
    old.last_run_at = now - 2 * interval;
    old.retry_at = now + 320 * 60_000;
    old.failures = 6;
    task_state::initialize_schedule(key, &old, interval).unwrap();
    let windows = json!({"risk": now + 6 * 3_600_000, "quota": now + 3_600_000});
    task_state::store_value(key, windows.clone()).unwrap();

    // Other tasks retain their original exponential-backoff behavior.
    task_state::initialize_schedule("fixture-other", &old, interval).unwrap();
    task_state::reschedule("fixture-other", interval).unwrap();
    assert_eq!(
        task_state::read("fixture-other").unwrap().retry_at,
        old.retry_at
    );

    task_state::reschedule_on_interval(key, interval).unwrap();
    let recovered = task_state::read(key).unwrap();
    assert_eq!(recovered.retry_at, 0);
    assert!(recovered.due_at() <= logging::now_ms());
    assert_eq!(recovered.value, Some(windows.clone()));
    let Claim::Acquired(guard) =
        task_state::claim(key, interval, false, ManualBackoff::Bypass, 60_000).unwrap()
    else {
        panic!("old failures must not block the next scheduled scan");
    };
    task_state::reschedule_on_interval(key, interval).unwrap();
    task_state::note_external_success(key, "manual success".into(), interval).unwrap();
    assert!(task_state::read(key).unwrap().running());
    assert!(matches!(
        task_state::claim(key, interval, true, ManualBackoff::Bypass, 60_000).unwrap(),
        Claim::Deferred(_)
    ));
    let failed = guard
        .finish_on_interval(false, "preview failed".into(), None, interval)
        .unwrap();
    assert_eq!(failed.failures, 7);
    assert_eq!(failed.last_error.as_deref(), Some("preview failed"));
    assert_eq!(failed.retry_at, 0);
    assert_eq!(failed.next_run_at, failed.last_run_at + interval);
    assert_eq!(failed.value, Some(windows.clone()));
    assert!(!failed.running());
    assert!(matches!(
        task_state::claim(key, interval, true, ManualBackoff::Bypass, 60_000).unwrap(),
        Claim::Deferred(_)
    ));

    // Manual retry bypasses the ordinary interval, not an active lease or minimum gap.
    let manual_key = "zcodeClaim:manual-fixture";
    old.last_attempt_at = now - 20_000;
    old.last_run_at = now - 20_000;
    task_state::initialize_schedule(manual_key, &old, interval).unwrap();
    let Claim::Acquired(guard) =
        task_state::claim(manual_key, interval, true, ManualBackoff::Bypass, 1_000).unwrap()
    else {
        panic!("manual retry must bypass old generic failures");
    };
    guard
        .finish_on_interval(true, "checked".into(), None, interval)
        .unwrap();

    task_state::note_external_success(key, "claimed fixture".into(), interval).unwrap();
    let success = task_state::read(key).unwrap();
    assert_eq!(success.failures, 0);
    assert_eq!(success.last_error, None);
    assert_eq!(success.last_result.as_deref(), Some("claimed fixture"));
    assert_eq!(success.retry_at, 0);
    assert_eq!(success.next_run_at, success.last_run_at + interval);
    assert_eq!(success.value, Some(windows));
    assert_eq!(
        task_state::read("fixture-other").unwrap().retry_at,
        old.retry_at
    );
}
