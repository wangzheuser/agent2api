//! Trae 国内 SOLO 的每日签到。
//!
//! 这条链与模型额度查询都走 `api.trae.cn`，但签到钱包和模型积分池是两份账，
//! 所以只把领取结果归一成签到结果，不把它混进 `usage` 的 credits pool。
//! 上游会按设备指纹做软限流：状态探测允许换一代设备 id 再试一次，领取遇到
//! 9074 则返回可读的退避状态，避免同一账号在短时间内连点造成更多风控。

use std::time::Duration;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::adapter::{account_proxy, read_record, renew_forced, renew_if_due};
use super::credentials::Credential;
use super::http::post_json;
use super::usage::{ug_headers, UG_HOST};

const STATUS_PATH: &str = "/trae/api/v2/ug/checkin_credits/status";
const CLAIM_PATH: &str = "/trae/api/v2/ug/checkin_credits/claim";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DEVICE_MODULUS: u128 = 10_000_000_000_000_000;
const AUTH_EXPIRED_CODE: i64 = 1001;
const DEVICE_BUSY_CODE: i64 = 9074;

/// 查询并领取一次 Trae 每日积分。返回形状与其它签到 provider 对齐：
/// `{success, msg, status, rewardPoints?, alreadyClaimed?}`。
pub async fn claim_daily(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let record = read_record(store, account_id)?;
    let proxy = account_proxy(&record)?;
    let credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    let credential = renew_if_due(store, &record, &credential, proxy.as_ref()).await?;
    if credential.access_token.trim().is_empty() {
        return Err(GatewayError::with_status(401, "Trae 账号缺少 accessToken，无法签到"));
    }

    match claim_once(credential, proxy.as_ref()).await {
        Ok(value) => Ok(value),
        Err(ClaimError::Auth) => {
            // 401 / code=1001 只做一次完整刷新重试，避免坏 token 无限循环。
            let fresh_record = read_record(store, account_id)?;
            let fresh_proxy = account_proxy(&fresh_record)?;
            let fresh = Credential::from_payload(&fresh_record).map_err(GatewayError::new)?;
            let renewed = renew_forced(store, &fresh_record, &fresh, fresh_proxy.as_ref()).await?;
            claim_once(renewed, fresh_proxy.as_ref())
                .await
                .map_err(ClaimError::into_gateway)
        }
        Err(error) => Err(error.into_gateway()),
    }
}

async fn claim_once(
    credential: Credential,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
) -> Result<Value, ClaimError> {
    let mut generation = 0u8;
    let status = loop {
        let device_id = device_id(&credential.uid, generation);
        match request(&credential, &device_id, STATUS_PATH, proxy).await? {
            ReplyKind::Auth => return Err(ClaimError::Auth),
            ReplyKind::DeviceBusy if generation == 0 => {
                generation = 1;
                continue;
            }
            ReplyKind::DeviceBusy => {
                return Ok(json!({
                    "success": false,
                    "status": "backoff",
                    "msg": "Trae 签到状态接口触发设备限流，请稍后重试",
                }));
            }
            ReplyKind::Body(body) => break body,
        }
    };

    if status_code(&status) != 0 {
        return Err(classify_reply(status, "Trae 签到状态查询失败"));
    }
    if already_claimed(&status) {
        return Ok(json!({
            "success": false,
            "status": "already_claimed",
            "alreadyClaimed": true,
            "msg": "今天已签到",
        }));
    }
    if !claimable(&status) {
        return Ok(json!({
            "success": false,
            "status": "unsupported",
            "msg": "当前没有可领取的 Trae 签到积分",
        }));
    }

    let device_id = device_id(&credential.uid, generation);
    match request(&credential, &device_id, CLAIM_PATH, proxy).await? {
        ReplyKind::Auth => Err(ClaimError::Auth),
        ReplyKind::DeviceBusy => Ok(json!({
            "success": false,
            "status": "backoff",
            "msg": "Trae 签到领取触发设备限流，请稍后重试",
        })),
        ReplyKind::Body(body) => {
            let code = status_code(&body);
            if code == AUTH_EXPIRED_CODE {
                return Err(ClaimError::Auth);
            }
            if code != 0 {
                return Err(classify_reply(body, "Trae 签到领取失败"));
            }
            let reward = first_i64(&body, &["reward_points", "rewardPoints", "points", "credits"]);
            let mut result = Map::new();
            result.insert("success".to_string(), Value::Bool(true));
            result.insert("status".to_string(), Value::String("claimed".to_string()));
            result.insert("msg".to_string(), Value::String(match reward {
                Some(value) if value > 0 => format!("签到成功，获得 {value} 积分"),
                _ => "签到成功".to_string(),
            }));
            if let Some(value) = reward {
                result.insert("rewardPoints".to_string(), Value::from(value));
            }
            result.insert("raw".to_string(), body);
            Ok(Value::Object(result))
        }
    }
}

enum ReplyKind {
    Auth,
    DeviceBusy,
    Body(Value),
}

#[derive(Debug)]
enum ClaimError {
    Auth,
    Gateway(GatewayError),
}

impl ClaimError {
    fn into_gateway(self) -> GatewayError {
        match self {
            Self::Auth => GatewayError::with_status(401, "Trae 登录态已过期，签到前刷新失败，请重新登录"),
            Self::Gateway(error) => error,
        }
    }
}

impl From<GatewayError> for ClaimError {
    fn from(error: GatewayError) -> Self { Self::Gateway(error) }
}

async fn request(
    credential: &Credential,
    device_id: &str,
    path: &str,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
) -> Result<ReplyKind, ClaimError> {
    let header_map = ug_headers(credential.variant(), credential.access_token.trim(), device_id);
    let headers = header_map
        .iter()
        .map(|(key, value)| (key.as_str(), value.clone()))
        .collect::<Vec<_>>();
    let url = format!("{UG_HOST}{path}");
    let reply = post_json(&url, &json!({}), &headers, REQUEST_TIMEOUT, proxy).await?;
    let body = reply.json().unwrap_or(Value::Null);
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if reply.status == 401 || code == AUTH_EXPIRED_CODE {
        return Ok(ReplyKind::Auth);
    }
    if code == DEVICE_BUSY_CODE {
        return Ok(ReplyKind::DeviceBusy);
    }
    if reply.status >= 400 {
        return Ok(ReplyKind::Body(json!({
            "code": reply.status,
            "msg": first_text(&body, &["msg", "message"]).unwrap_or_else(|| format!("HTTP {}", reply.status)),
        })));
    }
    Ok(ReplyKind::Body(body))
}

fn status_code(body: &Value) -> i64 {
    body.get("code").and_then(Value::as_i64).unwrap_or(-1)
}

fn first_text(body: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| body.get(*key).and_then(Value::as_str).map(str::to_string))
}

fn first_i64(body: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| body.get(*key).and_then(Value::as_i64))
        .or_else(|| {
            body.get("data")
                .and_then(|data| keys.iter().find_map(|key| data.get(*key).and_then(Value::as_i64)))
        })
}

fn bool_field(body: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| body.get(*key).and_then(Value::as_bool))
        .or_else(|| body.get("data").and_then(|data| keys.iter().find_map(|key| data.get(*key).and_then(Value::as_bool))))
}

fn already_claimed(body: &Value) -> bool {
    bool_field(body, &["already_claimed", "alreadyClaimed", "claimed", "has_claimed", "hasClaimed"]) == Some(true)
}

fn claimable(body: &Value) -> bool {
    bool_field(body, &["can_claim", "canClaim", "claimable", "available"]).unwrap_or(true)
}

fn classify_reply(body: Value, fallback: &str) -> ClaimError {
    let code = status_code(&body);
    let message = first_text(&body, &["msg", "message"]).unwrap_or_else(|| fallback.to_string());
    ClaimError::Gateway(GatewayError::with_status(
        if code == DEVICE_BUSY_CODE { 429 } else { 502 },
        message,
    ))
}

/// 稳定的 16 位设备指纹。设备代数只在状态接口遇到 9074 时递增，
/// 不写回账号，避免把一次风控退避永久改变登录设备绑定。
pub fn device_id(uid: &str, generation: u8) -> String {
    let seed = if generation == 0 { uid.to_string() } else { format!("{uid}#gen{generation}") };
    let digest = Sha256::digest(seed.as_bytes());
    let mut number = 0u128;
    for byte in digest.iter() {
        number = (number * 256 + u128::from(*byte)) % DEVICE_MODULUS;
    }
    format!("{number:016}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_and_16_digits() {
        assert_eq!("4302850041909017", device_id("u1", 0));
        assert_eq!(16, device_id("u1", 1).len());
        assert!(device_id("u1", 0).bytes().all(|byte| byte.is_ascii_digit()));
        assert_ne!(device_id("u1", 0), device_id("u1", 1));
    }

    #[test]
    fn status_fields_accept_flat_and_nested_shapes() {
        assert!(already_claimed(&json!({"data":{"already_claimed":true}})));
        assert!(!claimable(&json!({"data":{"can_claim":false}})));
        assert_eq!(Some(12), first_i64(&json!({"data":{"reward_points":12}}), &["reward_points"]));
    }
}
