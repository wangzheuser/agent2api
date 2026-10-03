use super::*;

const NOW: i64 = 1_790_985_600_000; // 2026-10-03 UTC

fn details(items: Value) -> Value {
    personal(items.as_array().unwrap(), NOW, vec![])
}

#[test]
fn zero_priority_fractions_unknown_total_and_resource_identity() {
    let data = details(json!([
        {"ResourceId":"zero","CycleCapacityRemainPrecise":0,"CapacityRemainPrecise":500},
        {"ResourceId":"a","PackageCode":"same","CycleCapacityRemainPrecise":"0.75"},
        {"ResourceId":"b","PackageCode":"same","CycleCapacityRemainPrecise":0.75}
    ]));
    assert_eq!(data["remaining"], 1.5);
    assert_eq!(data["complete"], true);
    assert_eq!(data["segments"].as_array().unwrap().len(), 3);
    assert_eq!(data["segments"][0]["total"], Value::Null);
    let zero = data["segments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|segment| segment["resourceId"] == "zero")
        .unwrap();
    assert_eq!(zero["remaining"], 0.0);
    assert_eq!(zero["state"], "exhausted");
}

#[test]
fn cycle_end_intersects_entitlement_and_seconds_millis_rfc3339_agree() {
    let expires = 1_793_462_399_000_i64;
    for end in [
        json!(expires),
        json!(expires / 1000),
        json!((expires / 1000).to_string()),
        json!("2026-10-31T23:59:59+08:00"),
    ] {
        let data = details(
            json!([{"Remaining":80.25,"CycleEndTime":end,"DeductionEndTime":"2034-10-31T23:59:59+08:00"}]),
        );
        assert_eq!(data["segments"][0]["expiresAt"], expires);
        assert_eq!(data["segments"][0]["expiryStatus"], "known");
        assert!(data["segments"][0]["entitlementEndsAt"].as_i64().unwrap() > expires);
    }
}

#[test]
fn sampled_cn_intl_naive_times_match_epoch_and_preserve_amount_precision() {
    for (text, epoch) in [
        ("2026-10-14 14:49:21", 1_791_960_561_000_i64),
        ("2027-03-23 10:39:36", 1_805_769_576_000_i64),
    ] {
        let data = details(
            json!([{"CycleCapacityRemainPrecise":"1442.76000034","CycleCapacitySizePrecise":"1500","CycleEndTime":text,"DeductionEndTime":epoch}]),
        );
        assert_eq!(data["remaining"], 1442.76000034);
        assert_eq!(data["segments"][0]["expiresAt"], epoch);
        assert_eq!(data["segments"][0]["expiryStatus"], "known");
        assert_eq!(data["complete"], true);
    }
}

#[test]
fn naive_dates_are_never_host_local_and_arbitrary_text_does_not_leak() {
    let data = details(json!([
        {"Remaining":2,"CycleEndTime":"2026-10-31","DeductionEndTime":"2034-10-31T23:59:59Z","accessToken":"secret"},
        {"Remaining":2,"EndTime":"secret-token"},
        {"Remaining":2,"EndTime":0},
        {"Remaining":2,"EndTime":"1970-01-01T00:00:00Z"}
    ]));
    let item = &data["segments"][0];
    assert_eq!(item["expiresAt"], Value::Null);
    assert_eq!(item["expiresAtText"], "2026-10-31");
    assert_eq!(item["expiryStatus"], "timezone_unverified");
    assert!(!data.to_string().contains("secret"));
    assert_eq!(data["complete"], true);
}

#[test]
fn missing_child_amounts_use_parent_only_once() {
    let data = details(
        json!([{"ResourceId":"r","Remaining":100,"SlicePeriodUsageDetails":[{"SlicePeriodEndTime":"2026-10-31"},{"SlicePeriodEndTime":"2026-11-30"}]}]),
    );
    assert_eq!(data["segments"].as_array().unwrap().len(), 1);
    assert_eq!(data["remaining"], 100.0);
    assert_eq!(data["unattributedRemaining"], 100.0);
    assert_eq!(data["complete"], false);
}

#[test]
fn unresolved_slice_allocation_does_not_advertise_long_term_entitlement_as_credit_expiry() {
    let data = details(
        json!([{"Remaining":100,"DeductionEndTime":"2034-10-31T23:59:59+08:00","SlicePeriodUsageDetails":[{"SlicePeriodEndTime":"2026-10-31 23:59:59"},{"SlicePeriodEndTime":"2026-11-30 23:59:59"}]}]),
    );
    let item = &data["segments"][0];
    assert_eq!(data["remaining"], 100.0);
    assert_eq!(item["expiresAt"], Value::Null);
    assert_eq!(item["expiryStatus"], "unknown");
    assert!(item["entitlementEndsAt"].as_i64().unwrap() > NOW);
    assert_eq!(data["complete"], false);
}

#[test]
fn child_positive_difference_is_unattributed_negative_is_not_scaled() {
    for (parent, count, mismatch) in [(100, 3, false), (50, 2, true)] {
        let data = details(
            json!([{"Remaining":parent,"SlicePeriodUsageDetails":[{"Remaining":30},{"Remaining":40}]}]),
        );
        assert_eq!(data["segments"].as_array().unwrap().len(), count);
        assert_eq!(data["remaining"].as_f64().unwrap(), parent as f64);
        assert_eq!(data["complete"], !mismatch);
        assert_eq!(data["segments"][0]["remaining"], 30.0);
        assert_eq!(data["segments"][1]["remaining"], 40.0);
        if !mismatch {
            assert_eq!(data["unattributedRemaining"], 30.0);
        }
    }
}

#[test]
fn independent_slices_do_not_inherit_parent_total_or_longer_window() {
    let data = details(
        json!([{"Remaining":3,"Amount":100,"CycleEndTime":"2026-10-10T00:00:00Z","SlicePeriodUsageDetails":[{"Remaining":1,"CycleEndTime":"2026-11-10T00:00:00Z"},{"Remaining":2}]}]),
    );
    for segment in data["segments"].as_array().unwrap() {
        assert_eq!(segment["total"], Value::Null);
        assert_eq!(segment["expiresAt"], 1_791_590_400_000_i64);
    }
    assert_ne!(data["segments"][0]["id"], data["segments"][1]["id"]);
}

#[test]
fn resource_and_period_ids_survive_reordering_and_duplicate_periods_remain_distinct() {
    let a = json!({"ResourceId":"a","Remaining":3,"SlicePeriodUsageDetails":[{"Remaining":1,"SlicePeriodEndTime":"2026-10-10T00:00:00Z"},{"Remaining":2,"SlicePeriodEndTime":"2026-11-10T00:00:00Z"}]});
    let b = json!({"ResourceId":"b","Remaining":3});
    let first = details(json!([a, b]));
    let second = details(json!([b, a]));
    assert_eq!(first["segments"], second["segments"]);
    let duplicate = details(
        json!([{"ResourceId":"d","Remaining":2,"SlicePeriodUsageDetails":[{"Remaining":1,"SlicePeriodEndTime":"2026-10-10T00:00:00Z"},{"Remaining":1,"SlicePeriodEndTime":"2026-10-10T00:00:00Z"}]}]),
    );
    assert_ne!(
        duplicate["segments"][0]["id"],
        duplicate["segments"][1]["id"]
    );
}

#[test]
fn invalid_negative_conflicting_total_and_truncation_are_explicit() {
    let data = details(json!([{"Remaining":-1},{"Remaining":"NaN"},{"Remaining":5,"Amount":2}]));
    assert_eq!(data["remaining"], Value::Null);
    assert_eq!(data["complete"], false);
    assert!(data["issues"]
        .as_array()
        .unwrap()
        .contains(&json!("balance_mismatch")));
    let partial = personal(&[json!({"Remaining":10})], NOW, vec!["truncated".into()]);
    assert_eq!(partial["remaining"], Value::Null);
    assert_eq!(partial["segments"][0]["remaining"], 10.0);
    assert_eq!(details(json!([]))["remaining"], 0.0);
}

#[test]
fn known_expired_and_future_periods_do_not_contribute_to_available() {
    let data = details(json!([
        {"Remaining":10,"EndTime":"2020-01-01T00:00:00Z"},
        {"Remaining":20,"StartTime":"2030-01-01T00:00:00Z"},
        {"Remaining":30,"NeverExpire":true}
    ]));
    assert_eq!(data["remaining"], 30.0);
    assert_eq!(data["segments"][0]["state"], "expired");
    assert!(data["segments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|segment| segment["state"] == "not_started"));
    assert!(data["segments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|segment| segment["expiryStatus"] == "never"));
}

#[test]
fn enterprise_finite_exhausted_unlimited_and_missing_amount() {
    for (data, expected, unlimited, complete) in [
        (
            json!({"limitNum":100,"credit":25.25}),
            json!(74.75),
            false,
            true,
        ),
        (
            json!({"limitNum":100,"credit":150}),
            json!(0.0),
            false,
            true,
        ),
        (json!({"limitNum":-1}), Value::Null, true, true),
        (json!({"limitNum":100}), Value::Null, false, false),
    ] {
        let result = enterprise(&data, NOW);
        assert_eq!(result["remaining"], expected);
        assert_eq!(result["unlimited"], unlimited);
        assert_eq!(result["complete"], complete);
    }
}
