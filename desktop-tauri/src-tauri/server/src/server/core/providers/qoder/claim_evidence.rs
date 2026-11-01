//! Pure evidence rules: a campaign acknowledgement is not a credit receipt.
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct QuotaSnapshot {
    pub total: f64,
    pub remaining: f64,
}

fn data(value: &Value) -> &Value {
    value.get("data").filter(|v| v.is_object()).unwrap_or(value)
}

pub(super) fn credit_reward(value: &Value) -> Option<f64> {
    let benefit = data(value).get("benefit")?;
    if benefit.get("kind").and_then(Value::as_str) != Some("CREDITS") {
        return None;
    }
    let amount = benefit.get("amount")?.as_f64()?;
    (amount.is_finite() && amount > 0.0).then_some(amount)
}

pub(super) fn already_claimed(value: &Value) -> bool {
    let value = data(value);
    let has_code = |code: &str| {
        ["result", "errorCode", "failureCode", "status"].iter().any(|key| {
            value.get(*key).and_then(Value::as_str)
                .is_some_and(|s| s.trim().eq_ignore_ascii_case(code))
        })
    };
    if has_code("SAME_PERSON_ALREADY_CLAIMED") || has_code("CAMPAIGN_NOT_ACTIVE") {
        return false;
    }
    has_code("ALREADY_CLAIMED")
        || (value.get("status").and_then(Value::as_str) == Some("CLAIMED")
            && value.get("replayed").and_then(Value::as_bool) == Some(true))
}

pub(super) fn quota_snapshot(value: &Value) -> Option<QuotaSnapshot> {
    let bucket = data(value).get("addOnQuota")?;
    if !bucket.get("unit").and_then(Value::as_str)
        .is_some_and(|u| u.eq_ignore_ascii_case("credits")) {
        return None;
    }
    let total = bucket.get("total")?.as_f64()?;
    let remaining = bucket.get("remaining")?.as_f64()?;
    (total.is_finite() && total >= 0.0 && remaining.is_finite() && remaining >= 0.0)
        .then_some(QuotaSnapshot { total, remaining })
}

pub(super) fn observed_delta(before: Option<QuotaSnapshot>, after: Option<QuotaSnapshot>) -> Option<f64> {
    Some(after?.total - before?.total)
}

/// Conservative correlation, not cryptographic proof or a server grant receipt.
/// Missing replay metadata, benefit kind, unit, or baseline remains unknown.
pub(super) fn credit_increase_observed(
    claim: &Value,
    campaign_confirmed: bool,
    before: Option<QuotaSnapshot>,
    after: Option<QuotaSnapshot>,
) -> bool {
    if !campaign_confirmed || already_claimed(claim)
        || data(claim).get("replayed").and_then(Value::as_bool) != Some(false) {
        return false;
    }
    match (credit_reward(claim), observed_delta(before, after)) {
        (Some(amount), Some(delta)) => delta >= amount,
        _ => false,
    }
}

/// Keep an acknowledged POST from falling through to another candidate attempt.
pub(super) fn stops_claim_attempts(value: &Value) -> bool {
    ["success", "alreadyCompleted", "claimAccepted", "samePersonLimited", "deviceLimited"]
        .iter().any(|key| value.get(*key).and_then(Value::as_bool) == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claim() -> Value {
        json!({"status":"CLAIMED","replayed":false,"benefit":{"kind":"CREDITS","amount":100}})
    }
    fn quota(total: f64, remaining: f64) -> Option<QuotaSnapshot> {
        quota_snapshot(&json!({"addOnQuota":{"total":total,"remaining":remaining,"unit":"credits"}}))
    }

    #[test]
    fn credit_reward_requires_explicit_positive_credit_benefit() {
        for value in [json!({}), json!({"benefit":{"amount":100}}),
            json!({"benefit":{"kind":"COUPON","amount":100}}),
            json!({"benefit":{"kind":"CREDITS","amount":"100"}}),
            json!({"benefit":{"kind":"CREDITS","amount":0}}),
            json!({"benefit":{"kind":"CREDITS","amount":-1}})] {
            assert_eq!(credit_reward(&value), None);
        }
        assert_eq!(credit_reward(&json!({"data":claim()})), Some(100.0));
    }

    #[test]
    fn already_claimed_2xx_body_is_not_new_reward_without_replayed() {
        let value = json!({"data":{"status":"CLAIMED","result":"ALREADY_CLAIMED"}});
        assert!(already_claimed(&value));
        assert!(!credit_increase_observed(&value, true, quota(0.0,0.0), quota(100.0,100.0)));
        assert!(!already_claimed(&json!({"failureCode":"CAMPAIGN_NOT_ACTIVE"})));
    }

    #[test]
    fn blocked_replayed_response_does_not_mean_this_account_claimed() {
        let blocked = json!({"status":"BLOCKED","replayed":true,"failureCode":"SAME_PERSON_ALREADY_CLAIMED"});
        assert!(!already_claimed(&blocked));
        assert!(!already_claimed(&json!({"result":"ALREADY_CLAIMED","failureCode":"CAMPAIGN_NOT_ACTIVE"})));
    }

    #[test]
    fn missing_quota_or_unit_stays_unknown_not_zero() {
        for value in [json!({}), json!({"userQuota":{}}),
            json!({"addOnQuota":{"total":0,"remaining":0}}),
            json!({"addOnQuota":{"total":0,"remaining":0,"unit":"requests"}})] {
            assert_eq!(quota_snapshot(&value),None);
        }
        assert_eq!(observed_delta(None,quota(100.0,100.0)),None);
        assert!(!credit_increase_observed(&claim(),true,None,quota(100.0,100.0)));
    }

    #[test]
    fn claimed_without_balance_increase_is_not_credited() {
        let before = quota(100.0,80.0);
        assert!(!credit_increase_observed(&claim(),true,before,before));
        assert!(!credit_increase_observed(&claim(),false,before,quota(200.0,180.0)));
        assert!(credit_increase_observed(&claim(),true,before,quota(200.0,180.0)));
        assert_eq!(observed_delta(before,quota(200.0,180.0)),Some(100.0));
    }

    #[test]
    fn accepted_without_credit_evidence_ends_candidate_iteration() {
        for accepted in [
            json!({"success":false,"claimAccepted":true,"claimUnconfirmed":true}),
            json!({"success":false,"claimAccepted":true,"claimConfirmed":true,
                "creditIncreaseObserved":false,"status":"claimed"}),
        ] {
            let mut attempts = 0;
            let mut returned = None;
            for response in [accepted.clone(), json!({"success":true})] {
                attempts += 1;
                if stops_claim_attempts(&response) {
                    returned = Some(response);
                    break;
                }
            }
            assert_eq!(attempts, 1);
            assert_eq!(returned, Some(accepted));
        }
        assert!(!stops_claim_attempts(&json!({"success":false,"msg":"not accepted"})));
    }

    #[test]
    fn missing_replay_metadata_and_replays_never_report_new_credit() {
        for value in [json!({"status":"CLAIMED","benefit":{"kind":"CREDITS","amount":100}}),
            json!({"status":"CLAIMED","replayed":true,"benefit":{"kind":"CREDITS","amount":100}})] {
            assert!(!credit_increase_observed(&value,true,quota(0.0,0.0),quota(100.0,100.0)));
        }
    }
}
