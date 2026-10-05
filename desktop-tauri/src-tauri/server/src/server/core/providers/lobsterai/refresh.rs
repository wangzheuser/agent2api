//! LobsterAI access token 刷新：单飞、二次检查和条件回写。

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;

use super::auth;
use super::credentials::{self, Credentials};

static FLIGHTS: OnceLock<Table<Credentials>> = OnceLock::new();

pub fn snapshot(
    store: &AccountStore,
    account_id: &str,
) -> Result<(String, Value, Credentials), GatewayError> {
    let (id, session) = if account_id.trim().is_empty() {
        let entry = store
            .current_entry_for_provider(super::PROVIDER_ID)
            .ok_or_else(|| {
                GatewayError::with_status(401, "没有可用的 LobsterAI 账号，请先添加账号")
            })?;
        (entry.id, entry.session)
    } else {
        let entry = store
            .get_session_by_id(account_id)
            .ok_or_else(|| GatewayError::with_status(404, "LobsterAI 账号不存在或没有可用凭证"))?;
        if entry
            .session
            .get("provider")
            .and_then(Value::as_str)
            .is_some_and(|provider| provider != super::PROVIDER_ID)
        {
            return Err(GatewayError::with_status(
                404,
                "LobsterAI 账号不存在或没有可用凭证",
            ));
        }
        (entry.id, entry.session)
    };
    let credentials = Credentials::from_payload(&session)?;
    Ok((id, session, credentials))
}

pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    force: bool,
) -> Result<Credentials, GatewayError> {
    let (id, _session, credentials) = snapshot(store, account_id)?;
    if !credentials.needs_refresh(force) {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "LobsterAI 账号没有 refreshToken，请重新登录或补充完整凭证",
        ));
    }
    let key = format!(
        "{}:{}:{}:{}",
        store.file_string(),
        id,
        refresh_flight::fingerprint(&credentials.access_token),
        refresh_flight::fingerprint(&credentials.refresh_token),
    );
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            // Re-check after joining the flight.  Another request may have
            // completed a refresh or re-imported the account while this call
            // was waiting to become leader; use that snapshot instead of
            // issuing a second refresh with stale credentials.
            let result = match snapshot(store, &id) {
                Ok((_, _current_session, current_credentials))
                    if current_credentials.access_token != credentials.access_token
                        || current_credentials.refresh_token != credentials.refresh_token =>
                {
                    Ok(current_credentials)
                }
                Ok((_, current_session, current_credentials)) => {
                    refresh_and_save(store, &id, &current_session, &current_credentials).await
                }
                Err(error) => Err(error),
            };
            leader.finish(result.clone());
            result
        }
    }
}

async fn refresh_and_save(
    store: &AccountStore,
    account_id: &str,
    session: &Value,
    credentials: &Credentials,
) -> Result<Credentials, GatewayError> {
    let proxy = auth::account_proxy(session)?;
    let response = auth::request(
        "POST",
        &format!("{}{}/api/auth/refresh", auth::base_url(), ""),
        Some(&credentials.refresh_payload()),
        None,
        proxy.as_ref(),
    )
    .await?;
    let data = auth::payload(response, "凭证续期")?;
    let access_token = credentials::text(&data, &["accessToken", "access_token", "token"]);
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "LobsterAI 续期响应缺少 accessToken，旧凭证未被覆盖",
        ));
    }
    let refresh_token = credentials::text(&data, &["refreshToken", "refresh_token"]);
    let expires_at_ms = data
        .get("expiresAt")
        .or_else(|| data.get("expires_at"))
        .or_else(|| data.get("expiresIn"))
        .and_then(|value| {
            let number = value
                .as_i64()
                .or_else(|| value.as_f64().map(|v| v as i64))
                .or_else(|| value.as_str()?.parse::<i64>().ok())?;
            if value == data.get("expiresIn").unwrap_or(&Value::Null) {
                Some(crate::server::logging::now_ms().saturating_add(number.saturating_mul(1000)))
            } else if number < 100_000_000_000 {
                number.checked_mul(1000)
            } else {
                Some(number)
            }
        })
        .or_else(|| {
            credentials::Credentials::from_payload(&json!({"accessToken": access_token}))
                .ok()
                .and_then(|value| value.effective_expiry_ms())
        });
    let mut fresh = credentials.clone();
    fresh.access_token = access_token;
    if !refresh_token.is_empty() {
        fresh.refresh_token = refresh_token;
    }
    fresh.expires_at_ms = expires_at_ms;
    match store.update_account_tokens_if_current(
        account_id,
        &credentials.access_token,
        &credentials.refresh_token,
        Some(&fresh.access_token),
        Some(&fresh.refresh_token),
        fresh.expires_at_ms.map(|value| value as f64),
        None,
    ) {
        Ok(CredentialWrite::Written) => Ok(fresh),
        Ok(CredentialWrite::Stale) => {
            let (_, _, current) = snapshot(store, account_id)?;
            Ok(current)
        }
        Err(error) => Err(GatewayError::with_status(error.status_code, error.message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refresh_payload_contains_required_identity_fields() {
        let credential = Credentials::from_payload(&json!({
            "auth": {"accessToken":"a", "refreshToken":"r", "uuid":"u", "firstKeyfrom":"f"},
            "account": {"userId":"uid"}
        }))
        .expect("credentials");
        let payload = credential.refresh_payload();
        assert_eq!(payload["refreshToken"], "r");
        assert_eq!(payload["uuid"], "u");
        assert_eq!(payload["userId"], "uid");
    }
}
