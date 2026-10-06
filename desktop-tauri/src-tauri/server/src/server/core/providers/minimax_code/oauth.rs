//! MiniMax Code 的 RFC 8628 设备授权登录。

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::errors::GatewayError;

use super::auth;
use super::credentials::{self, Credentials};

const DEFAULT_EXPIRES_IN: u64 = 600;
const DEFAULT_INTERVAL: u64 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAuthStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in_seconds: u64,
    pub interval_seconds: u64,
    pub code_verifier: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DevicePoll {
    Pending { slow_down: bool },
    Authorized(Box<Credentials>),
}

pub async fn start_device_authorization() -> Result<DeviceAuthStart, GatewayError> {
    let code_verifier = pkce_verifier()?;
    let form = vec![
        ("client_id".to_string(), credentials::CLIENT_ID.to_string()),
        ("scope".to_string(), credentials::OAUTH_SCOPE.to_string()),
        ("audience".to_string(), credentials::OAUTH_AUDIENCE.to_string()),
        ("code_challenge".to_string(), pkce_challenge(&code_verifier)),
        ("code_challenge_method".to_string(), "S256".to_string()),
    ];
    let response = auth::request_form(
        &format!("{}/oauth2/device/code", oauth_base_url()),
        &form,
        None,
    )
    .await?;
    let payload = auth::payload(response, "申请设备授权")?;
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let Some(device_code) = text("device_code") else {
        return Err(GatewayError::with_status(502, "MiniMax Code 响应缺少 device_code"));
    };
    let Some(user_code) = text("user_code") else {
        return Err(GatewayError::with_status(502, "MiniMax Code 响应缺少 user_code"));
    };
    let Some(verification_uri) = text("verification_uri") else {
        return Err(GatewayError::with_status(502, "MiniMax Code 响应缺少 verification_uri"));
    };
    Ok(DeviceAuthStart {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: text("verification_uri_complete"),
        expires_in_seconds: payload
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_EXPIRES_IN)
            .clamp(1, 15 * 60),
        interval_seconds: payload
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_INTERVAL)
            .max(1),
        code_verifier,
    })
}

pub async fn poll_device_token(
    device_code: &str,
    code_verifier: &str,
) -> Result<DevicePoll, GatewayError> {
    let form = vec![
        (
            "grant_type".to_string(),
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        ),
        ("device_code".to_string(), device_code.to_string()),
        ("code_verifier".to_string(), code_verifier.to_string()),
        ("client_id".to_string(), credentials::CLIENT_ID.to_string()),
    ];
    let response = auth::request_form(
        &format!("{}/oauth2/token", oauth_base_url()),
        &form,
        None,
    )
    .await?;
    let status = response.status;
    let payload = response.payload.unwrap_or(Value::Null);
    if !response.ok || payload.get("error").is_some() {
        return classify_device_error(status, &payload);
    }
    let normalized = normalize_token_payload(&payload);
    let credentials = Credentials::from_payload(&normalized)?;
    if credentials.refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "MiniMax Code 登录响应缺少 refreshToken",
        ));
    }
    Ok(DevicePoll::Authorized(Box::new(credentials)))
}

fn classify_device_error(status: u16, payload: &Value) -> Result<DevicePoll, GatewayError> {
    let error = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match error {
        "authorization_pending" => Ok(DevicePoll::Pending { slow_down: false }),
        "slow_down" => Ok(DevicePoll::Pending { slow_down: true }),
        "expired_token" | "invalid_grant" => Err(GatewayError::with_status(
            408,
            "MiniMax Code 设备授权已过期，请重新发起登录",
        )),
        "access_denied" => Err(GatewayError::with_status(403, "MiniMax Code 设备授权被拒绝")),
        _ => Err(GatewayError::with_status(
            i32::from(if status >= 400 { status } else { 502 }),
            "MiniMax Code 登录轮询失败，请重新发起登录",
        )),
    }
}

fn oauth_base_url() -> String {
    std::env::var("MINIMAX_CODE_ACCOUNT_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| credentials::ACCOUNT_BASE.to_string())
}

fn pkce_verifier() -> Result<String, GatewayError> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| GatewayError::with_status(500, "无法生成 MiniMax Code 登录校验参数"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn normalize_token_payload(data: &Value) -> Value {
    let data = data
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(data);
    if data.get("auth").is_some() {
        return data.clone();
    }
    let auth = json!({
        "accessToken": data.get("accessToken").or_else(|| data.get("access_token")).cloned().unwrap_or(Value::Null),
        "refreshToken": data.get("refreshToken").or_else(|| data.get("refresh_token")).cloned().unwrap_or(Value::Null),
        "expiresIn": data.get("expiresIn").or_else(|| data.get("expires_in")).cloned().unwrap_or(Value::Null),
        "refreshExpiresIn": data.get("refreshExpiresIn").or_else(|| data.get("refresh_expires_in")).cloned().unwrap_or(Value::Null),
    });
    json!({ "auth": auth, "account": data.get("account").or_else(|| data.get("user")).cloned().unwrap_or_else(|| data.clone()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_base64url_sha256() {
        let verifier = "verifier";
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(pkce_challenge(verifier), expected);
    }

    #[test]
    fn nested_and_flat_tokens_are_normalized() {
        let value = normalize_token_payload(&json!({"access_token":"a", "refresh_token":"r"}));
        let parsed = Credentials::from_payload(&value).expect("token payload");
        assert_eq!(parsed.access_token, "a");
        assert_eq!(parsed.refresh_token, "r");
    }

    #[test]
    fn device_errors_follow_rfc8628_pending_semantics() {
        assert!(matches!(
            classify_device_error(400, &json!({"error":"authorization_pending"})),
            Ok(DevicePoll::Pending { slow_down: false })
        ));
        assert!(matches!(
            classify_device_error(400, &json!({"error":"slow_down"})),
            Ok(DevicePoll::Pending { slow_down: true })
        ));
        assert!(matches!(
            classify_device_error(200, &json!({"error":"authorization_pending"})),
            Ok(DevicePoll::Pending { slow_down: false })
        ));
        assert_eq!(
            classify_device_error(400, &json!({"error":"expired_token"}))
                .expect_err("expired")
                .status_code,
            408
        );
    }
}
