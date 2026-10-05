//! MiniMax Code token 续期：refresh-flight 单飞、401 单次重试和条件回写。

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
                GatewayError::with_status(401, "没有可用的 MiniMax Code 账号，请先添加账号")
            })?;
        (entry.id, entry.session)
    } else {
        let entry = store.get_session_by_id(account_id).ok_or_else(|| {
            GatewayError::with_status(404, "MiniMax Code 账号不存在或没有可用凭证")
        })?;
        if entry
            .session
            .get("provider")
            .and_then(Value::as_str)
            .is_some_and(|provider| provider != super::PROVIDER_ID)
        {
            return Err(GatewayError::with_status(
                404,
                "MiniMax Code 账号不存在或没有可用凭证",
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
            "MiniMax Code 账号没有 refreshToken，请重新登录或补充完整凭证",
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
            // The account may have been refreshed or re-imported between the
            // first snapshot and acquiring the leader slot.  Re-read while
            // still holding the single-flight ownership so an older request
            // cannot refresh (or overwrite) the newer credentials.
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
    let form = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        (
            "refresh_token".to_string(),
            credentials.refresh_token.clone(),
        ),
        ("client_id".to_string(), credentials::CLIENT_ID.to_string()),
        ("scope".to_string(), credentials::OAUTH_SCOPE.to_string()),
        (
            "audience".to_string(),
            credentials::OAUTH_AUDIENCE.to_string(),
        ),
    ];
    let response = auth::request_form(
        &format!("{}/oauth2/token", oauth_base_url()),
        &form,
        proxy.as_ref(),
    )
    .await?;
    let data = auth::payload(response, "凭证续期")?;
    let access_token = credentials::text(&data, &["access_token", "accessToken", "token"]);
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "MiniMax Code 续期响应缺少 accessToken，旧凭证未被覆盖",
        ));
    }
    let refresh_token = credentials::text(&data, &["refresh_token", "refreshToken"]);
    let expires_at_ms = expiry_from_response(&data).or_else(|| {
        Credentials::from_payload(&json!({"accessToken": access_token}))
            .ok()
            .and_then(|value| value.effective_expiry_ms())
    });
    let refresh_expires_at_ms = expiry_field(&data, &["refresh_expires_in", "refreshExpiresIn"]);
    let mut fresh = credentials.clone();
    fresh.access_token = access_token;
    if !refresh_token.is_empty() {
        fresh.refresh_token = refresh_token;
    }
    fresh.expires_at_ms = expires_at_ms;
    fresh.refresh_expires_at_ms = refresh_expires_at_ms.or(credentials.refresh_expires_at_ms);
    match store.update_account_tokens_if_current(
        account_id,
        &credentials.access_token,
        &credentials.refresh_token,
        Some(&fresh.access_token),
        Some(&fresh.refresh_token),
        fresh.expires_at_ms.map(|value| value as f64),
        fresh.refresh_expires_at_ms.map(|value| value as f64),
    ) {
        Ok(CredentialWrite::Written) => Ok(fresh),
        Ok(CredentialWrite::Stale) => {
            let (_, _, current) = snapshot(store, account_id)?;
            Ok(current)
        }
        Err(error) => Err(GatewayError::with_status(error.status_code, error.message)),
    }
}

fn oauth_base_url() -> String {
    std::env::var("MINIMAX_CODE_ACCOUNT_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| credentials::ACCOUNT_BASE.to_string())
}

fn expiry_field(payload: &Value, keys: &[&str]) -> Option<i64> {
    let value = keys.iter().find_map(|key| payload.get(*key))?;
    let raw = value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.parse::<i64>().ok())?;
    if raw <= 0 {
        None
    } else if raw < 100_000_000_000 {
        raw.checked_mul(1000)
    } else {
        Some(raw)
    }
}

fn expiry_from_response(payload: &Value) -> Option<i64> {
    expiry_field(payload, &["expires_at", "expiresAt"]).or_else(|| {
        expiry_field(payload, &["expires_in", "expiresIn"])
            .map(|seconds| crate::server::logging::now_ms().saturating_add(seconds))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn oauth_constants_are_sent_as_expected() {
        assert_eq!(credentials::CLIENT_ID, "mcode-public");
        assert_eq!(credentials::OAUTH_SCOPE, "agent.default");
        assert_eq!(credentials::OAUTH_AUDIENCE, "agent-backend");
        assert_eq!(
            expiry_from_response(&json!({"expires_in":3600})).is_some(),
            true
        );
    }
}
