//! MiniMax Code 凭证解析、过期判断与脱敏字段。

use base64::Engine;
use serde_json::{json, Value};

use crate::server::core::account_store::MAX_TOKEN_LENGTH;
use crate::server::errors::GatewayError;
use crate::server::logging;

pub const MESSAGES_BASE: &str = "https://agent.minimax.cn/mavis/api/v1/llm/v1";
pub const SERVER_BASE: &str = "https://agent.minimax.cn";
pub const ACCOUNT_BASE: &str = "https://account.minimax.cn";
pub const USER_AGENT: &str = "agent2api-minimax-code";
pub const CLIENT_ID: &str = "mcode-public";
pub const OAUTH_SCOPE: &str = "agent.default";
pub const OAUTH_AUDIENCE: &str = "agent-backend";
pub const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub raw: Value,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: Option<i64>,
    pub refresh_expires_at_ms: Option<i64>,
    pub user_id: String,
    pub nickname: String,
    pub client_id: String,
    pub audience: String,
}

impl Credentials {
    /// 兼容官方 `auth`/`account` 嵌套结构和手动填写的平铺结构。
    pub fn from_payload(payload: &Value) -> Result<Self, GatewayError> {
        let Some(object) = payload.as_object() else {
            return Err(GatewayError::with_status(
                400,
                "MiniMax Code 账号内容必须是 JSON 对象",
            ));
        };
        let auth = payload
            .get("auth")
            .filter(|value| value.is_object())
            .unwrap_or(payload);
        let account = payload
            .get("account")
            .filter(|value| value.is_object())
            .unwrap_or(payload);
        let access_token = text(auth, &["accessToken", "access_token", "token"]);
        if access_token.is_empty() {
            return Err(GatewayError::with_status(
                400,
                "缺少 MiniMax Code accessToken",
            ));
        }
        let refresh_token = text(auth, &["refreshToken", "refresh_token"]);
        if access_token.chars().count() > MAX_TOKEN_LENGTH
            || refresh_token.chars().count() > MAX_TOKEN_LENGTH
            || access_token.chars().any(char::is_control)
            || refresh_token.chars().any(char::is_control)
        {
            return Err(GatewayError::with_status(
                400,
                "MiniMax Code 凭证过长或包含非法控制字符",
            ));
        }
        Ok(Self {
            raw: Value::Object(object.clone()),
            access_token,
            refresh_token,
            expires_at_ms: timestamp(auth, &["expiresAt", "expires_at", "expiresIn"]),
            refresh_expires_at_ms: timestamp(auth, &["refreshExpiresAt", "refresh_expires_at"]),
            user_id: text(account, &["userId", "user_id", "uid", "id"]),
            nickname: text(account, &["nickname", "name", "displayName"]),
            client_id: text(auth, &["clientId", "client_id"]),
            audience: text(auth, &["audience"]),
        })
    }

    pub fn effective_expiry_ms(&self) -> Option<i64> {
        jwt_claim(&self.access_token, "exp")
            .map(|seconds| seconds.saturating_mul(1000))
            .or(self.expires_at_ms)
    }

    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }

    pub fn needs_refresh(&self, force: bool) -> bool {
        force
            || self.effective_expiry_ms().is_none_or(|expires| {
                expires <= logging::now_ms().saturating_add(REFRESH_MARGIN_MS)
            })
    }

    pub fn token_tail(&self) -> String {
        self.access_token
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub fn to_value(&self) -> Value {
        let mut raw = self.raw.clone();
        if !raw.is_object() {
            raw = json!({});
        }
        raw["accessToken"] = Value::String(self.access_token.clone());
        raw["refreshToken"] = Value::String(self.refresh_token.clone());
        if let Some(value) = self.expires_at_ms {
            raw["expiresAt"] = Value::from(value);
        }
        if let Some(value) = self.refresh_expires_at_ms {
            raw["refreshExpiresAt"] = Value::from(value);
        }
        if !self.user_id.is_empty() {
            raw["userId"] = Value::String(self.user_id.clone());
        }
        if !self.nickname.is_empty() {
            raw["nickname"] = Value::String(self.nickname.clone());
        }
        raw
    }
}

pub fn text(object: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match object.get(*key) {
            Some(Value::String(value)) if !value.trim().is_empty() => {
                Some(value.trim().to_string())
            }
            Some(Value::Number(value)) => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn timestamp(object: &Value, keys: &[&str]) -> Option<i64> {
    let key = keys.iter().find(|key| object.get(**key).is_some())?;
    let value = object.get(*key)?;
    let parsed = value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())?;
    if parsed <= 0 {
        return None;
    }
    if *key == "expiresIn" {
        return Some(logging::now_ms().saturating_add(parsed.saturating_mul(1000)));
    }
    if parsed < 100_000_000_000 {
        parsed.checked_mul(1000)
    } else {
        Some(parsed)
    }
}

fn jwt_claim(token: &str, claim: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let object = serde_json::from_slice::<Value>(&decoded).ok()?;
    object.get(claim)?.as_f64().map(|value| value as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_flat_and_nested_payloads() {
        let nested = json!({
            "auth": {"accessToken": "a", "refreshToken": "r", "expiresAt": 1800000000000i64},
            "account": {"userId": "u", "nickname": "N"}
        });
        let value = Credentials::from_payload(&nested).expect("nested");
        assert_eq!(value.access_token, "a");
        assert_eq!(value.user_id, "u");
        let flat = json!({"access_token":"a", "refresh_token":"r", "user_id":"u2"});
        assert_eq!(
            Credentials::from_payload(&flat).expect("flat").user_id,
            "u2"
        );
    }

    #[test]
    fn requires_access_token_but_allows_access_only_manual_credentials() {
        assert!(Credentials::from_payload(&json!({"refreshToken":"r"})).is_err());
        assert!(!Credentials::from_payload(&json!({"accessToken":"a"}))
            .expect("access only")
            .can_refresh());
    }
}
