//! LobsterAI 凭证：兼容官方客户端的嵌套形态与手动填写的平铺形态。
//!
//! 本模块只负责解析、脱敏和过期判断。刷新请求与账号存储回写分别位于
//! `refresh`，这样网页登录、手动添加和转发链路共用同一套字段口径。

use base64::Engine;
use serde_json::{json, Value};

use crate::server::core::account_store::MAX_TOKEN_LENGTH;
use crate::server::errors::GatewayError;
use crate::server::logging;

pub const SERVER_BASE: &str = "https://lobsterai-server.youdao.com";
pub const CLIENT_VERSION: &str = "2026.9.4";
pub const USER_AGENT: &str = "LobsterAI/2026.9.4";
pub const CLIENT_CAPABILITIES: &str = "kimi-k3-agentic-v1";
pub const REFRESH_MARGIN_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// 原始记录，刷新时保留未知字段。
    pub raw: Value,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: Option<i64>,
    pub refresh_expires_at_ms: Option<i64>,
    pub uid: String,
    pub user_id: String,
    pub nickname: String,
    pub uuid: String,
    pub first_keyfrom: String,
    pub latest_keyfrom: String,
}

impl Credentials {
    /// 从 `{auth, account}` 或平铺账号 JSON 解析凭证。
    pub fn from_payload(payload: &Value) -> Result<Self, GatewayError> {
        let Some(object) = payload.as_object() else {
            return Err(GatewayError::with_status(
                400,
                "LobsterAI 账号内容必须是 JSON 对象",
            ));
        };
        let auth = payload
            .get("auth")
            .filter(|value| value.is_object())
            .unwrap_or(payload);
        let account = payload
            .get("account")
            .filter(|value| value.is_object())
            .or_else(|| payload.get("user").filter(|value| value.is_object()))
            .unwrap_or(payload);
        let access_token = first_text(&[auth, payload], &["accessToken", "access_token", "token"]);
        if access_token.is_empty() {
            return Err(GatewayError::with_status(400, "缺少 LobsterAI accessToken"));
        }
        let refresh_token = first_text(&[auth, payload], &["refreshToken", "refresh_token"]);
        if access_token.chars().count() > MAX_TOKEN_LENGTH
            || refresh_token.chars().count() > MAX_TOKEN_LENGTH
            || access_token.chars().any(char::is_control)
            || refresh_token.chars().any(char::is_control)
        {
            return Err(GatewayError::with_status(
                400,
                "LobsterAI 凭证过长或包含非法控制字符",
            ));
        }
        let jwt = jwt_payload(&access_token);
        let uid = first_text(&[account, payload], &["uid", "id", "user_id"])
            .if_empty_or(&jwt_text(&jwt, &["uid", "user_id", "sub"]));
        let user_id = first_text(&[account, payload], &["userId", "user_id", "yid"])
            .if_empty_or(&jwt_text(&jwt, &["userId", "user_id", "yid", "sub"]));
        let nickname = first_text(&[account, payload], &["nickname", "name", "displayName"])
            .if_empty_or(&jwt_text(&jwt, &["nickname", "name"]));
        Ok(Self {
            raw: Value::Object(object.clone()),
            access_token,
            refresh_token,
            expires_at_ms: timestamp(auth, &["expiresAt", "expires_at", "expiresIn"])
                .or_else(|| timestamp(payload, &["expiresAt", "expires_at", "expiresIn"])),
            refresh_expires_at_ms: timestamp(
                auth,
                &["refreshExpiresAt", "refresh_expires_at", "refreshExpiresIn"],
            )
            .or_else(|| {
                timestamp(
                    payload,
                    &["refreshExpiresAt", "refresh_expires_at", "refreshExpiresIn"],
                )
            }),
            uid,
            user_id,
            nickname,
            uuid: first_text(&[auth, payload], &["uuid"]),
            first_keyfrom: first_text(&[auth, payload], &["firstKeyfrom", "first_keyfrom"]),
            latest_keyfrom: first_text(&[auth, payload], &["latestKeyfrom", "latest_keyfrom"]),
        })
    }

    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.is_empty()
    }

    pub fn effective_expiry_ms(&self) -> Option<i64> {
        jwt_claim(&self.access_token, "exp")
            .map(|seconds| seconds.saturating_mul(1000))
            .or(self.expires_at_ms)
    }

    pub fn needs_refresh(&self, force: bool) -> bool {
        force
            || self.effective_expiry_ms().is_none_or(|expires| {
                expires <= logging::now_ms().saturating_add(REFRESH_MARGIN_MS)
            })
    }

    pub fn refresh_payload(&self) -> Value {
        let mut body = json!({
            "refreshToken": self.refresh_token,
            "firstKeyfrom": self.first_keyfrom,
            "latestKeyfrom": current_millis_string(),
            "version": CLIENT_VERSION,
        });
        if !self.uuid.is_empty() {
            body["uuid"] = Value::String(self.uuid.clone());
        }
        if !self.user_id.is_empty() {
            body["userId"] = Value::String(self.user_id.clone());
        }
        body
    }

    /// 把换码/刷新结果归一为账号记录能直接落盘的嵌套形态。
    pub fn to_value(&self) -> Value {
        let mut raw = self.raw.clone();
        if !raw.is_object() {
            raw = json!({});
        }
        let auth = raw
            .get("auth")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let account = raw
            .get("account")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut auth = auth;
        auth.insert(
            "accessToken".to_string(),
            Value::String(self.access_token.clone()),
        );
        auth.insert(
            "refreshToken".to_string(),
            Value::String(self.refresh_token.clone()),
        );
        put_optional_timestamp(&mut auth, "expiresAt", self.expires_at_ms);
        put_optional_timestamp(&mut auth, "refreshExpiresAt", self.refresh_expires_at_ms);
        auth.insert("uuid".to_string(), Value::String(self.uuid.clone()));
        auth.insert(
            "firstKeyfrom".to_string(),
            Value::String(self.first_keyfrom.clone()),
        );
        auth.insert(
            "latestKeyfrom".to_string(),
            Value::String(self.latest_keyfrom.clone()),
        );
        raw["auth"] = Value::Object(auth);
        let mut account = account;
        account.insert("uid".to_string(), Value::String(self.uid.clone()));
        account.insert("userId".to_string(), Value::String(self.user_id.clone()));
        account.insert("nickname".to_string(), Value::String(self.nickname.clone()));
        raw["account"] = Value::Object(account);
        raw
    }

    pub fn token_tail(&self) -> String {
        let chars: Vec<char> = self.access_token.chars().collect();
        chars
            .into_iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
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

fn first_text(objects: &[&Value], keys: &[&str]) -> String {
    objects
        .iter()
        .find_map(|object| {
            let value = text(object, keys);
            (!value.is_empty()).then_some(value)
        })
        .unwrap_or_default()
}

fn put_optional_timestamp(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<i64>,
) {
    if let Some(value) = value {
        object.insert(key.to_string(), Value::from(value));
    } else {
        object.remove(key);
    }
}

fn timestamp(object: &Value, keys: &[&str]) -> Option<i64> {
    let key = keys.iter().find(|key| object.get(**key).is_some())?;
    let value = object.get(*key)?;
    let parsed = value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())?;
    if *key == "expiresIn" {
        return (parsed > 0).then(|| logging::now_ms().saturating_add(parsed.saturating_mul(1000)));
    }
    if parsed <= 0 {
        None
    } else if parsed < 100_000_000_000 {
        parsed.checked_mul(1000)
    } else {
        Some(parsed)
    }
}

fn jwt_claim(token: &str, claim: &str) -> Option<i64> {
    jwt_payload(token)?.get(claim).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_f64().map(|number| number as i64))
            .or_else(|| value.as_str()?.parse::<i64>().ok())
    })
}

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&decoded).ok()
}

fn jwt_text(payload: &Option<Value>, claims: &[&str]) -> String {
    let Some(payload) = payload.as_ref() else {
        return String::new();
    };
    claims
        .iter()
        .find_map(|claim| match payload.get(*claim) {
            Some(Value::String(value)) if !value.trim().is_empty() => {
                Some(value.trim().to_string())
            }
            Some(Value::Number(value)) => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

trait StringFallback {
    fn if_empty_or(self, fallback: &str) -> String;
}

impl StringFallback for String {
    fn if_empty_or(self, fallback: &str) -> String {
        if self.trim().is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

pub fn current_millis_string() -> String {
    logging::now_ms().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_and_flat_credentials_are_supported() {
        let nested = json!({
            "auth": {"accessToken": "a", "refreshToken": "r", "expiresAt": 1_800_000_000_000i64},
            "account": {"uid": "u1", "nickname": "N"}
        });
        let parsed = Credentials::from_payload(&nested).expect("nested");
        assert_eq!(parsed.access_token, "a");
        assert_eq!(parsed.uid, "u1");
        let flat = json!({"accessToken":"a", "refreshToken":"r", "uid":"u2"});
        assert_eq!(Credentials::from_payload(&flat).expect("flat").uid, "u2");
    }

    #[test]
    fn missing_access_token_is_rejected() {
        let error =
            Credentials::from_payload(&json!({"refreshToken":"r"})).expect_err("missing access");
        assert_eq!(error.status_code, 400);
    }

    #[test]
    fn expires_in_is_relative_and_refresh_payload_keeps_identity() {
        let credentials = Credentials::from_payload(&json!({
            "auth": {"accessToken":"a", "refreshToken":"r", "expiresIn":3600, "uuid":"u", "firstKeyfrom":"desktop"},
            "account": {"userId":"id"}
        })).expect("credentials");
        assert!(credentials.expires_at_ms.is_some());
        let body = credentials.refresh_payload();
        assert_eq!(body["refreshToken"], "r");
        assert_eq!(body["uuid"], "u");
        assert_eq!(body["userId"], "id");
    }

    #[test]
    fn refresh_expiry_and_unknown_nested_fields_are_preserved() {
        let credentials = Credentials::from_payload(&json!({
            "auth": {
                "accessToken": "a",
                "refreshToken": "r",
                "refreshExpiresAt": 1_900_000_000,
                "vendorField": "keep"
            },
            "account": {"uid":"u", "vendorAccountField": true}
        }))
        .expect("credentials");
        assert!(credentials.refresh_expires_at_ms.is_some());
        let value = credentials.to_value();
        assert_eq!(value["auth"]["vendorField"], "keep");
        assert_eq!(value["account"]["vendorAccountField"], true);
    }

    #[test]
    fn access_only_payload_is_accepted_and_jwt_identity_is_used() {
        let payload = json!({
            "accessToken": "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1LTEiLCJ1c2VySWQiOiJ1LTEifQ.sig"
        });
        let credentials = Credentials::from_payload(&payload).expect("access only");
        assert!(credentials.refresh_token.is_empty());
        assert_eq!(credentials.uid, "u-1");
        assert_eq!(credentials.user_id, "u-1");
    }
}
