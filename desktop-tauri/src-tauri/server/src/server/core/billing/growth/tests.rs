use super::{normalize, transport, valid_code};
use serde_json::{json, Value};

#[test]
fn business_errors_and_zero_rewards_do_not_become_claimed_credit() {
    assert!(transport::decode(json!({"code":400,"data":{"credit":300}})).is_err());
    assert!(transport::decode(json!({"data":{"credit":300}})).is_err());
    assert!(transport::decode(json!({"code":0,"data":null})).is_ok());
    assert_eq!(
        normalize::rewards(&json!({"reward_credit":0,"reward_energy":2}))["credits"],
        0.0
    );
    assert_eq!(
        normalize::rewards(&json!({"credit":0.125}))["credits"],
        0.125
    );
    assert!(normalize::rewards(&json!({}))["credits"].is_null());
    assert!(normalize::number(Some(&json!(-1))).is_none());
    assert!(normalize::number(Some(&json!("NaN"))).is_none());
    assert!(!valid_code("../../claim"));
    assert!(!valid_code("task?uid=1"));
}

#[test]
fn task_capability_follows_official_progress_and_conditions() {
    let rows=normalize::tasks(&json!({"tasks":[
        {"task_code":"unknown","progress":{"current":1,"target":1},"reward_credit":0,"accept_status":"accepted"},
        {"task_code":"claimed","progress":{"current":1,"target":1},"accept_status":"claimed"},
        {"task_code":"locked","progress":{"current":1,"target":1},"locked":true},
        {"task_code":"expired","progress":{"current":1,"target":1},"valid_end":"2020-01-01T00:00:00Z"},
        {"task_code":"Sequential_Tasks_3","progress":null,"accept_status":"not_accepted"}
    ]}),"growth").unwrap();
    assert_eq!(rows[0]["canClaim"], true);
    assert_eq!(rows[0]["canExecute"], false);
    assert_eq!(rows[0]["rewards"]["credits"], 0.0);
    for row in &rows[1..4] {
        assert_eq!(row["canClaim"], false);
        assert_eq!(row["canExecute"], false);
    }
    assert!(rows[4]["target"].is_null());
    assert_eq!(rows[4]["canClaim"], false);
    assert!(normalize::tasks(&json!({}), "growth").is_err());
}

#[test]
fn travel_requires_real_record_buddy_and_destination_data() {
    let config = json!({"locations":[{"id":1,"name":"森林","duration_hours_min":1,"duration_hours_max":4,"reward_credit_min":5,"reward_credit_max":10}]});
    let buddy = json!({"buddy":{"instance_id":42,"name":"猫"}});
    let state = normalize::travel(
        &json!({"state":"idle","daily_limit_reached":false,"buddy_id":42,"record_id":0,"arrive_at":0}),
        &config,
        &buddy,
    );
    assert_eq!(state["canDepart"], true);
    assert_eq!(state["canAdopt"], false);
    assert!(state["arrivesAt"].is_null());
    assert_eq!(state["locations"][0]["durationSecondsMax"], 14400.0);
    assert_eq!(
        normalize::travel(&json!({"state":"idle"}), &Value::Null, &buddy)["canDepart"],
        false
    );
    assert_eq!(
        normalize::travel(
            &json!({"state":"idle","daily_limit_reached":true}),
            &config,
            &buddy
        )["canDepart"],
        false
    );
    let arrived = normalize::travel(
        &json!({"state":"arrived","record_id":99,"arrive_at":1790985600}),
        &config,
        &buddy,
    );
    assert_eq!(arrived["canClaim"], true);
    assert_eq!(arrived["arrivesAt"], 1790985600000_i64);
    assert_eq!(
        normalize::travel(&json!({"state":"arrived"}), &config, &buddy)["canClaim"],
        false
    );
}

#[test]
fn streak_makeup_uses_past_missed_days_and_preserves_zero_tier_credit() {
    let state = normalize::streak(
        &json!({"timezone":"Asia/Shanghai","launch_date":"2026-09-01","streak":{"days":7,"makeup_dates":["2026-09-29"]},"makeup_cards":{"balance":1},"redemption_status":{"tier_7d_status":"locked","tiers":[{"tier":"7d","days":7,"credit":0,"energy":2,"cards":1,"chances":1}]}}),
        &json!({"today":{"date":"2026-10-03"},"cells":[{"date":"2026-09-29","score":0},{"date":"2026-10-02","score":0},{"date":"2026-10-03","score":0},{"date":"2026-10-01","score":1}]}),
    );
    assert_eq!(state["missedDates"], json!(["2026-10-02"]));
    assert_eq!(state["tiers"][0]["claimable"], false);
    assert_eq!(state["tiers"][0]["rewards"]["credits"], 0.0);
    assert_eq!(state["tiers"][0]["rewards"]["makeupCards"], 1.0);
    assert_eq!(
        normalize::streak(
            &json!({}),
            &json!({"today":{"date":"2026-10-03"},"cells":[{"date":"2026-10-02","score":0}]})
        )["missedDates"],
        json!([])
    );
}
