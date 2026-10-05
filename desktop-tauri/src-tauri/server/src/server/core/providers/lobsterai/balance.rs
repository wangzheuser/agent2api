//! LobsterAI 额度：`profile-summary` 的总额与 creditItems 分桶。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::{auth, refresh};

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    }
}

fn expiry(value: Option<&Value>) -> Option<Value> {
    let value = value?.clone();
    if value.is_null() {
        None
    } else {
        Some(value)
    }
}

pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let (resolved_id, session, credentials) = refresh::snapshot(store, account_id)?;
    let proxy = auth::account_proxy(&session)?;
    let mut response = auth::request(
        "GET",
        &format!("{}/api/user/profile-summary", auth::base_url()),
        None,
        Some(&credentials.access_token),
        proxy.as_ref(),
    )
    .await?;
    if response.status == 401 && credentials.can_refresh() {
        let fresh = refresh::ensure_fresh(store, &resolved_id, true).await?;
        response = auth::request(
            "GET",
            &format!("{}/api/user/profile-summary", auth::base_url()),
            None,
            Some(&fresh.access_token),
            proxy.as_ref(),
        )
        .await?;
    }
    let raw = auth::payload(response, "额度查询")?;
    let total = number(raw.get("totalCreditsRemaining"));
    let mut wallets = Vec::new();
    if let Some(items) = raw.get("creditItems").and_then(Value::as_array) {
        for item in items {
            let Some(remaining) = number(item.get("creditsRemaining")) else {
                continue;
            };
            let kind = item
                .get("type")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("unknown");
            let label = item
                .get("label")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(kind);
            wallets.push(json!({
                "type": format!("credit_{kind}"),
                "displayName": label,
                "balance": remaining,
                "expiresAt": expiry(item.get("expiresAt")),
            }));
        }
    }
    let mut subscription = raw
        .get("subscription")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !subscription.is_object() {
        subscription = json!({"value": subscription});
    }
    if let Some(expires_at) = raw.get("expiresAt") {
        if let Some(object) = subscription.as_object_mut() {
            object
                .entry("expireAt")
                .or_insert_with(|| expires_at.clone());
        }
    }
    Ok(json!({
        "totalCreditsRemaining": total,
        "free": raw.get("free").cloned().unwrap_or(Value::Null),
        "campaign": raw.get("campaign").cloned().unwrap_or(Value::Null),
        "subscription": subscription,
        "creditItems": raw.get("creditItems").cloned().unwrap_or_else(|| json!([])),
        "expiresAt": raw.get("expiresAt").cloned().unwrap_or(Value::Null),
        "available": total,
        "unit": "credits",
        "wallets": wallets,
        "raw": raw,
    }))
}

#[cfg(test)]
mod tests {
    use super::number;
    use serde_json::json;

    #[test]
    fn numeric_strings_are_accepted_for_credit_fields() {
        assert_eq!(number(Some(&json!("12.5"))), Some(12.5));
        assert_eq!(number(Some(&json!(null))), None);
    }
}
