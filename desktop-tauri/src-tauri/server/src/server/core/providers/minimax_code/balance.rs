//! MiniMax Code 额度查询与分桶。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{auth, refresh};

const CREDIT_PATH: &str = "/minimax-cloud/api/v1/credit/details";

pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let (id, _, _) = refresh::snapshot(store, account_id)?;
    let credentials = refresh::ensure_fresh(store, &id, false).await?;
    let mut result = query_usage_once(store, &id, &credentials).await;
    if result
        .as_ref()
        .err()
        .is_some_and(|error| error.status_code == 401)
        && credentials.can_refresh()
    {
        let fresh = refresh::ensure_fresh(store, &id, true).await?;
        result = query_usage_once(store, &id, &fresh).await;
    }
    let payload = result?;
    Ok(normalize(&payload, logging::now_ms()))
}

async fn query_usage_once(
    store: &AccountStore,
    account_id: &str,
    credentials: &super::credentials::Credentials,
) -> Result<Value, GatewayError> {
    let (_, session, _) = refresh::snapshot(store, account_id)?;
    let proxy = auth::account_proxy(&session)?;
    let url = format!(
        "{}{CREDIT_PATH}?timezone_id=Asia%2FShanghai",
        auth::base_url()
    );
    let response = auth::request_json(
        "GET",
        &url,
        None,
        Some(&credentials.access_token),
        proxy.as_ref(),
    )
    .await?;
    auth::payload(response, "额度查询")
}

fn number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(value)) => value.as_f64().unwrap_or(0.0),
        Some(Value::String(value)) => value.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn positive(value: Option<&Value>) -> f64 {
    let value = number(value);
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn timestamp_ms(value: Option<&Value>) -> Option<i64> {
    let raw = number(value) as i64;
    if raw <= 0 {
        None
    } else if raw < 100_000_000_000 {
        raw.checked_mul(1000)
    } else {
        Some(raw)
    }
}

/// MiniMax 的 `credit_type` 在不同版本接口里出现过数字和字符串两种形态。
/// 统一成稳定的分桶键，避免把 Code 赠予、付费和开放平台余额合并显示。
fn credit_type_key(value: Option<&Value>) -> String {
    let raw = match value {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.trim().to_ascii_lowercase(),
        _ => String::new(),
    };
    match raw.as_str() {
        // 当前 credit/details 的枚举：1=付费，2=Code 赠予，3=开放平台。
        "1" | "paid" | "pay" | "payment" | "subscription" => "paid".to_string(),
        "2" | "gift" | "bonus" | "grant" | "赠予" | "赠送" => "gift".to_string(),
        "3" | "platform" | "open_platform" | "open-platform" | "openapi" => "platform".to_string(),
        "" => "gift".to_string(),
        other => format!("type-{other}"),
    }
}

fn credit_type_display_name(key: &str) -> String {
    match key {
        "gift" => "MiniMax Code 赠予额度".to_string(),
        "paid" => "MiniMax Code 付费额度".to_string(),
        "platform" => "开放平台余额".to_string(),
        other => format!("MiniMax Code 额度（{other}）"),
    }
}

/// 上游金额是字符串；已耗尽项与缺失到期时间的项不进入钱包明细。
pub fn normalize(payload: &Value, now_ms: i64) -> Value {
    let details = payload
        .get("details")
        .or_else(|| payload.get("data").and_then(|data| data.get("details")))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut by_type: Map<String, Value> = Map::new();
    for item in details {
        let Some(object) = item.as_object() else {
            continue;
        };
        let remain = positive(
            object
                .get("remaining_amount")
                .or_else(|| object.get("remainingAmount")),
        );
        let expires_at = timestamp_ms(
            object
                .get("expire_at_ms")
                .or_else(|| object.get("expireAt")),
        );
        if remain <= 0.0 || expires_at.is_none() {
            continue;
        }
        if expires_at.is_some_and(|value| value <= now_ms) {
            continue;
        }
        let credit_type = credit_type_key(
            object
                .get("credit_type")
                .or_else(|| object.get("creditType")),
        );
        let display_name = credit_type_display_name(&credit_type);
        let entry = by_type.entry(credit_type.clone()).or_insert_with(|| {
            json!({
                "type": credit_type,
                "displayName": display_name,
                "balance": 0.0,
                "unit": "credits",
                "expiresAt": expires_at,
                "packages": [],
            })
        });
        if let Some(balance) = entry.get_mut("balance") {
            *balance = Value::from(balance.as_f64().unwrap_or(0.0) + remain);
        }
        if let Some(packages) = entry.get_mut("packages").and_then(Value::as_array_mut) {
            packages.push(json!({
                "remain": remain,
                "size": positive(object.get("granted_amount").or_else(|| object.get("grantedAmount"))),
                "consumed": positive(object.get("consumed_amount").or_else(|| object.get("consumedAmount"))),
                "expiresAt": expires_at,
            }));
        }
    }
    let mut wallets: Vec<Value> = by_type.into_values().collect();
    for wallet in &mut wallets {
        let earliest = wallet
            .get("packages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|package| package.get("expiresAt").and_then(Value::as_i64))
            .min();
        if let Some(earliest) = earliest {
            wallet["expiresAt"] = Value::from(earliest);
        }
    }
    let available: f64 = wallets.iter().map(|item| number(item.get("balance"))).sum();
    let soon: f64 = wallets
        .iter()
        .flat_map(|item| {
            item.get("packages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|package| {
            let expires_at = package.get("expiresAt").and_then(Value::as_i64)?;
            let remain = number(package.get("remain"));
            (expires_at > now_ms && expires_at - now_ms <= 7 * 24 * 60 * 60 * 1000)
                .then_some(remain)
        })
        .sum();
    json!({
        "available": available,
        "unit": "credits",
        "wallets": wallets,
        "expiringSoon": soon,
        "raw": payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_amounts_are_grouped_and_exhausted_items_are_filtered() {
        let result = normalize(
            &json!({"details": [
                {"credit_type":"gift", "remaining_amount":"2.50", "granted_amount":"3", "expire_at_ms":1800000000000i64},
                {"credit_type":"gift", "remaining_amount":"0", "expire_at_ms":1800000000000i64},
                {"credit_type":"paid", "remaining_amount":"4", "expire_at_ms":1800000000000i64}
            ]}),
            1700000000000,
        );
        assert_eq!(result["available"], 6.5);
        let wallets = result["wallets"].as_array().expect("wallets");
        assert_eq!(wallets.len(), 2);
        assert!(wallets
            .iter()
            .any(|item| item["displayName"] == "MiniMax Code 赠予额度"));
        assert!(wallets
            .iter()
            .any(|item| item["displayName"] == "MiniMax Code 付费额度"));
    }

    #[test]
    fn numeric_credit_types_keep_gift_paid_and_platform_separate() {
        let result = normalize(
            &json!({"details": [
                {"credit_type": 1, "remaining_amount":"1", "expire_at_ms":1800000000000i64},
                {"credit_type": 2, "remaining_amount":"2", "expire_at_ms":1800000000000i64},
                {"credit_type": 3, "remaining_amount":"3", "expire_at_ms":1800000000000i64}
            ]}),
            1700000000000,
        );
        let wallets = result["wallets"].as_array().expect("wallets");
        assert_eq!(wallets.len(), 3);
        assert_eq!(
            wallets.iter().find(|item| item["type"] == "paid").unwrap()["balance"],
            1.0
        );
        assert_eq!(
            wallets.iter().find(|item| item["type"] == "gift").unwrap()["balance"],
            2.0
        );
        assert_eq!(
            wallets
                .iter()
                .find(|item| item["type"] == "platform")
                .unwrap()["balance"],
            3.0
        );
    }

    #[test]
    fn expiry_is_aggregated_per_package_and_expired_rows_are_ignored() {
        let now = 1_700_000_000_000i64;
        let result = normalize(
            &json!({"details": [
                {"credit_type":"gift", "remaining_amount":"2", "expire_at_ms":1700001000000i64},
                {"credit_type":"gift", "remaining_amount":"3", "expire_at_ms":1700700000000i64},
                {"credit_type":"gift", "remaining_amount":"9", "expire_at_ms":1699999999000i64}
            ]}),
            now,
        );
        let wallet = &result["wallets"][0];
        assert_eq!(wallet["balance"], 5.0);
        assert_eq!(wallet["expiresAt"], 1700001000000i64);
        assert_eq!(result["expiringSoon"], 2.0);
    }
}
