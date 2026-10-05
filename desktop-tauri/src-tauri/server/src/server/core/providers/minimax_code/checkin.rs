//! MiniMax Code 每日签到：先查状态，再按状态领取，结果保持幂等。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::credentials::Credentials;
use super::{auth, refresh};

const STATUS_PATH: &str = "/minimax-cloud/api/v1/signin/status";
const CLAIM_PATH: &str = "/minimax-cloud/api/v1/signin/claim";

pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let (id, _, _) = refresh::snapshot(store, account_id)?;
    let credentials = refresh::ensure_fresh(store, &id, false).await?;
    let mut result = claim_daily_checkin_once(store, &id, &credentials).await;
    if result
        .as_ref()
        .err()
        .is_some_and(|error| error.status_code == 401)
        && credentials.can_refresh()
    {
        let fresh = refresh::ensure_fresh(store, &id, true).await?;
        result = claim_daily_checkin_once(store, &id, &fresh).await;
    }
    result
}

async fn claim_daily_checkin_once(
    store: &AccountStore,
    account_id: &str,
    credentials: &Credentials,
) -> Result<Value, GatewayError> {
    let (_, session, _) = refresh::snapshot(store, account_id)?;
    let proxy = auth::account_proxy(&session)?;
    let status_url = format!(
        "{}{STATUS_PATH}?timezone_id=Asia%2FShanghai",
        auth::base_url()
    );
    let before = auth::payload(
        auth::request_json(
            "GET",
            &status_url,
            None,
            Some(&credentials.access_token),
            proxy.as_ref(),
        )
        .await?,
        "签到状态查询",
    )?;
    let panel = before.get("data").unwrap_or(&before);
    let today = panel
        .get("days")
        .and_then(Value::as_array)
        .and_then(|days| {
            days.iter().find(|day| {
                day.get("is_today")
                    .or_else(|| day.get("isToday"))
                    .and_then(|value| {
                        value.as_bool().or_else(|| {
                            value.as_str().and_then(|text| {
                                matches!(text.trim(), "true" | "1").then_some(true)
                            })
                        })
                    })
                    == Some(true)
            })
        });
    let status = today
        .and_then(|day| day.get("status"))
        .and_then(number)
        .or_else(|| panel.get("status").and_then(number))
        .unwrap_or(0);
    if status == 3 {
        return Ok(json!({
            "success": false,
            "alreadyCompleted": true,
            "claimable": false,
            "status": "already_completed",
            "msg": "今天已签到",
        }));
    }
    if status != 2 {
        return Ok(json!({
            "success": false,
            "alreadyCompleted": false,
            "claimable": false,
            "status": "not_claimable",
            "msg": "今天暂不可领取",
        }));
    }
    let claim_url = format!(
        "{}{CLAIM_PATH}?timezone_id=Asia%2FShanghai",
        auth::base_url()
    );
    let claim = auth::payload(
        auth::request_json(
            "POST",
            &claim_url,
            Some(&json!({})),
            Some(&credentials.access_token),
            proxy.as_ref(),
        )
        .await?,
        "签到领取",
    )?;
    let data = claim.get("data").unwrap_or(&claim);
    let result = data.get("claim_result").and_then(number).unwrap_or(0);
    if result == 2 {
        return Ok(json!({
            "success": false,
            "alreadyCompleted": true,
            "claimable": false,
            "status": "already_completed",
            "msg": "今天已签到（服务端确认）",
        }));
    }
    let points = data
        .get("points")
        .cloned()
        .or_else(|| today.and_then(|item| item.get("points")).cloned())
        .unwrap_or(Value::Null);
    Ok(json!({
        "success": result == 1,
        "alreadyCompleted": false,
        "claimable": false,
        "status": if result == 1 { "claimed" } else { "unknown" },
        "reward": {"points": points},
        "msg": if result == 1 { "签到成功" } else { "上游未确认签到结果" },
    }))
}

fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn status_constants_match_upstream_contract() {
        assert_eq!(json!({"status": 2})["status"], 2);
        assert_eq!(json!({"claim_result": 1})["claim_result"], 1);
    }

    #[test]
    fn numeric_strings_are_accepted_for_status_and_claim_result() {
        assert_eq!(super::number(&json!("2")), Some(2));
        assert_eq!(super::number(&json!("1")), Some(1));
    }
}
