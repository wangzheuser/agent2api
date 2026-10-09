//! Trae 国内 SOLO 的每日签到。
//!
//! 这条链与模型额度查询都走 `api.trae.cn`，但签到钱包和模型积分池是两份账，
//! 所以只把领取结果归一成签到结果，不把它混进 `usage` 的 credits pool。
//! 上游会按设备指纹做软限流：状态探测允许换一代设备 id 再试一次，领取遇到
//! 9074 则返回可读的退避状态，避免同一账号在短时间内连点造成更多风控。

use std::future::Future;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::core::account_store::AccountStore;
use crate::server::core::proxies::ResolvedProxy;
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
        return Err(GatewayError::with_status(
            401,
            "Trae 账号缺少 accessToken，无法签到",
        ));
    }

    claim_with_refresh_at(credential, proxy, UG_HOST, || async {
        let fresh_record = read_record(store, account_id)?;
        let fresh_proxy = account_proxy(&fresh_record)?;
        let fresh = Credential::from_payload(&fresh_record).map_err(GatewayError::new)?;
        let renewed = renew_forced(store, &fresh_record, &fresh, fresh_proxy.as_ref()).await?;
        Ok((renewed, fresh_proxy))
    })
    .await
}

// 刷新预算属于整轮，已接受 claim 后只恢复同设备回查，绝不重新领取。
// 显式端点与刷新入口也用于本地 HTTP 驱动测试，不改全局环境或真实账号。
async fn claim_with_refresh_at<F, Fut>(
    credential: Credential,
    proxy: Option<ResolvedProxy>,
    base: &str,
    mut refresh: F,
) -> Result<Value, GatewayError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(Credential, Option<ResolvedProxy>), GatewayError>>,
{
    match claim_once(credential, proxy.as_ref(), base).await {
        Ok(value) => Ok(value),
        Err(error @ (ClaimError::Auth | ClaimError::ConfirmationAuth { .. })) => {
            let (renewed, fresh_proxy) = refresh().await?;
            let result = match error {
                ClaimError::ConfirmationAuth {
                    before,
                    claim,
                    device_id,
                } => {
                    confirm(
                        &renewed,
                        &device_id,
                        fresh_proxy.as_ref(),
                        base,
                        before,
                        claim,
                    )
                    .await
                }
                _ => claim_once(renewed, fresh_proxy.as_ref(), base).await,
            };
            result
                .map(|mut row| {
                    row["reauthAttempted"] = json!(true);
                    row
                })
                .map_err(ClaimError::into_gateway)
        }
        Err(error) => Err(error.into_gateway()),
    }
}

async fn claim_once(
    credential: Credential,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
    base: &str,
) -> Result<Value, ClaimError> {
    let mut generation = 0u8;
    let status = loop {
        let device_id = device_id(&credential.uid, generation);
        match request(&credential, &device_id, STATUS_PATH, proxy, base).await? {
            ReplyKind::Auth => return Err(ClaimError::Auth),
            ReplyKind::DeviceBusy if generation == 0 => {
                generation = 1;
                continue;
            }
            ReplyKind::DeviceBusy => {
                return Ok(json!({
                    "success": false,
                    "status": "backoff",
                    "code": DEVICE_BUSY_CODE,
                    "checkinDeviceId": device_id,
                    "msg": "Trae 签到状态接口触发设备限流，请稍后重试",
                }));
            }
            ReplyKind::Body(body) => break body,
        }
    };

    if status_code(&status) != 0 {
        return Err(classify_reply(status, "Trae 签到状态查询失败"));
    }
    let before = status_of(&status);
    let device_id = device_id(&credential.uid, generation);
    if before.done_today() || !before.enable {
        return Ok(complete(
            decide(&before, &Status::default(), None),
            &device_id,
        ));
    }
    match request(&credential, &device_id, CLAIM_PATH, proxy, base).await? {
        ReplyKind::Auth => Err(ClaimError::Auth),
        ReplyKind::DeviceBusy => Ok(json!({
            "success": false,
            "status": "backoff",
            "code": DEVICE_BUSY_CODE,
            "checkinDeviceId": device_id,
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
            confirm(&credential, &device_id, proxy, base, before, body).await
        }
    }
}

async fn confirm(
    credential: &Credential,
    device_id: &str,
    proxy: Option<&ResolvedProxy>,
    base: &str,
    before: Status,
    claim: Value,
) -> Result<Value, ClaimError> {
    let after = match request(credential, device_id, STATUS_PATH, proxy, base).await? {
        ReplyKind::Auth => {
            return Err(ClaimError::ConfirmationAuth {
                before,
                claim,
                device_id: device_id.to_string(),
            })
        }
        ReplyKind::Body(value) => Some(status_of(&value)),
        ReplyKind::DeviceBusy => None,
    };
    let mut row = complete(
        decide(&before, &status_of(&claim), after.as_ref()),
        device_id,
    );
    if row.get("success").and_then(Value::as_bool) == Some(true) {
        // H 的领取响应字段继续保留；U 的当日奖励数额单独来自同设备回查。
        if let Some(reward) = first_i64(
            &claim,
            &["reward_points", "rewardPoints", "points", "credits"],
        ) {
            row["rewardPoints"] = json!(reward);
        }
        row["raw"] = claim;
    }
    Ok(row)
}

enum ReplyKind {
    Auth,
    DeviceBusy,
    Body(Value),
}

#[derive(Debug)]
enum ClaimError {
    Auth,
    ConfirmationAuth {
        before: Status,
        claim: Value,
        device_id: String,
    },
    Gateway(GatewayError),
}

impl ClaimError {
    fn into_gateway(self) -> GatewayError {
        match self {
            Self::Auth | Self::ConfirmationAuth { .. } => {
                GatewayError::with_status(401, "Trae 登录态已过期，签到状态刷新失败，请重新登录")
            }
            Self::Gateway(error) => error,
        }
    }
}

impl From<GatewayError> for ClaimError {
    fn from(error: GatewayError) -> Self {
        Self::Gateway(error)
    }
}

async fn request(
    credential: &Credential,
    device_id: &str,
    path: &str,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
    base: &str,
) -> Result<ReplyKind, ClaimError> {
    let header_map = ug_headers(
        credential.variant(),
        credential.access_token.trim(),
        device_id,
    );
    let headers = header_map
        .iter()
        .map(|(key, value)| (key.as_str(), value.clone()))
        .collect::<Vec<_>>();
    let url = format!("{base}{path}");
    let reply = post_json(&url, &json!({}), &headers, REQUEST_TIMEOUT, proxy).await?;
    let body = reply.json().unwrap_or(Value::Null);
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if reply.status == 401 || code == 401 || code == AUTH_EXPIRED_CODE {
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
    keys.iter()
        .find_map(|key| body.get(*key).and_then(Value::as_str).map(str::to_string))
}

fn first_i64(body: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| body.get(*key).and_then(Value::as_i64))
        .or_else(|| {
            body.get("data").and_then(|data| {
                keys.iter()
                    .find_map(|key| data.get(*key).and_then(Value::as_i64))
            })
        })
}

fn bool_field(body: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| body.get(*key).and_then(Value::as_bool))
        .or_else(|| {
            body.get("data").and_then(|data| {
                keys.iter()
                    .find_map(|key| data.get(*key).and_then(Value::as_bool))
            })
        })
}

fn already_claimed(body: &Value) -> bool {
    // 账号与设备任一完成即停止；前一个 false 不能挡住后一个 true。
    [
        "already_claimed",
        "alreadyClaimed",
        "claimed",
        "has_claimed",
        "hasClaimed",
        "checked_in",
        "did_checked_in",
    ]
    .iter()
    .any(|key| {
        body.get(*key).and_then(Value::as_bool) == Some(true)
            || body
                .get("data")
                .and_then(|data| data.get(*key))
                .and_then(Value::as_bool)
                == Some(true)
    })
}

fn claimable(body: &Value) -> bool {
    bool_field(
        body,
        &["can_claim", "canClaim", "claimable", "available", "enable"],
    )
    .unwrap_or(true)
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
    let seed = if generation == 0 {
        uid.to_string()
    } else {
        format!("{uid}#gen{generation}")
    };
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

const CODE_OK: i64 = 0;
#[cfg(test)]
#[path = "checkin_merge_tests.rs"]
mod merge_tests;
const CODE_REJECTED: i64 = DEVICE_BUSY_CODE;
const CODE_TOKEN_DEAD: i64 = AUTH_EXPIRED_CODE;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub checked_in: bool,
    pub did_checked_in: bool,
    pub enable: bool,
    pub credits: i64,
    pub extra_credits: i64,
    pub code: i64,
    pub message: String,
}

impl Status {
    /// 「今天这个账号已经算签过了」——两个维度任一为真即真。
    ///
    /// `did_checked_in` 是**设备维度**（这个号今天签过），`checked_in` 是账号维度。
    pub fn done_today(&self) -> bool {
        self.code == CODE_OK && (self.checked_in || self.did_checked_in)
    }

    /// 今日奖励的两个分量（合计见 [`Award::total`]）。
    pub fn award(&self) -> Award {
        Award {
            credits: self.credits,
            extra_credits: self.extra_credits,
        }
    }
}

/// 响应体 → `Status`。缺字段一律按 `false` / `0` 收（**不猜**：`enable` 缺省
/// 按"没开"处理最保守，宁可少签一次也不多打一发请求）。
pub fn status_of(payload: &Value) -> Status {
    let number = |key: &str| first_i64(payload, &[key]).unwrap_or(0);
    Status {
        checked_in: already_claimed(payload),
        did_checked_in: flag(payload, "did_checked_in"),
        enable: claimable(payload),
        credits: number("credits"),
        extra_credits: number("extra_credits"),
        code: payload.get("code").and_then(Value::as_i64).unwrap_or(-1),
        message: payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
    }
}

fn flag(payload: &Value, key: &str) -> bool {
    bool_field(payload, &[key]).unwrap_or(false)
}

/// 一次往返后的业务码判定。
///
/// 参考实现分三路：码 0 → 收；码 9074 → 换下一个探测体（同方案）；其它非零
/// → 换下一个鉴权方案。比它多一档 `GiveUp`（1001）：既然这一码判为「服务端
/// 已作废这张 JWT」，换 `Bearer` 重试还是拿同一张死令牌去打 —— 那一发不会
/// 改变结果，只会把一次点击变成两发配额。换发在驱动那一层做。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// 采纳这一发
    Accept,
    /// 9074：换下一个探测体（体用尽则整体失败）
    NextBody,
    /// 其它非零码：换下一个鉴权方案（方案用尽则整体失败）
    NextScheme,
    /// 1001：这一发就是终点 —— 真正的处置在驱动那一层（换发新令牌后重放整轮）
    GiveUp,
}

pub fn probe_for(code: i64) -> Probe {
    match code {
        CODE_OK => Probe::Accept,
        CODE_REJECTED => Probe::NextBody,
        CODE_TOKEN_DEAD => Probe::GiveUp,
        _ => Probe::NextScheme,
    }
}

/// 状态 → 领取 → 回查 的**结论**（与网络层解耦）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 上游没给这个账号开签到活动（中性，不算失败也不算成功）
    NotEnabled,
    /// 今天已经签过了（`already` 带的是当日奖励数额，可能为 0）
    AlreadyCheckedIn(Award),
    /// 本次真领到了（回查已确认），参数为奖励数额
    Claimed(Award),
    /// 领取被拒（`code` 是上游业务码，`message` 是上游原话）
    Rejected(i64, String),
    /// claim 回了 code:0 但同设备号回查没翻转 —— 服务端静默拒签，**不算成功**
    Unconfirmed,
}

/// 今日奖励的**两部分**（基础 + 加成）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Award {
    pub credits: i64,
    pub extra_credits: i64,
}

impl Award {
    /// 参考实现给界面看的合计数（两者相加）。
    pub fn total(&self) -> i64 {
        self.credits + self.extra_credits
    }
}

/// 拿到状态与（必要时）领取+回查后的判定。
///
/// `before` 是本轮第一发状态；`after` 是领取后用**同一个设备号**的回查；
/// `claim` 是领取那一发的响应（只读它的 code 与 message，数额归回查读）。
pub fn decide(before: &Status, claim: &Status, after: Option<&Status>) -> Outcome {
    if before.code != CODE_OK {
        return Outcome::Rejected(before.code, before.message.clone());
    }
    if before.done_today() {
        return Outcome::AlreadyCheckedIn(before.award());
    }
    if !before.enable {
        return Outcome::NotEnabled;
    }
    if claim.code != CODE_OK {
        return Outcome::Rejected(claim.code, claim.message.clone());
    }
    match after {
        // 回查确认：奖励数按回查那份读（上游在 claim 里不保证给数额）
        Some(confirmed) if confirmed.done_today() => Outcome::Claimed(confirmed.award()),
        Some(_) | None => Outcome::Unconfirmed,
    }
}

/// 这一行结果是不是「服务端把这张 JWT 作废了」。
///
/// 判据两条并列（码、文案），因为 1001 有时**不带文案**：只认码会在文案到位时
/// 仍走一遍，只认文案会在文案缺失时整条失效。文案两种写法都要认 —— 上游与
/// 参考实现各自转述过 `not able to authenticate` 和 `unable to authenticate`，
/// 差一个词就漏判。
pub fn needs_reauth(row: &Value) -> bool {
    if matches!(
        row.get("code").and_then(Value::as_i64),
        Some(401 | CODE_TOKEN_DEAD)
    ) {
        return true;
    }
    let message = row.get("msg").and_then(Value::as_str).unwrap_or_default();
    message.contains("not able to authenticate") || message.contains("unable to authenticate")
}

fn complete(outcome: Outcome, device_id: &str) -> Value {
    let mut row = serde_json::Map::new();
    let awarded = match outcome {
        Outcome::NotEnabled => {
            row.insert("success".into(), json!(false));
            row.insert("status".into(), json!("unsupported"));
            row.insert(
                "msg".into(),
                json!("当前没有可领取的签到活动（上游未对该账号开放这一档）"),
            );
            None
        }
        Outcome::AlreadyCheckedIn(award) => {
            row.insert("success".into(), json!(false));
            row.insert("status".into(), json!("already_claimed"));
            row.insert("alreadyClaimed".into(), json!(true));
            // `alreadyCompleted` 是给 `billing::checkin::checkin_completed_today`
            // 看的：今天已经签过 = 当日台账要落库，定时链明天才照常再试，
            // 而不是把"已签"当成失败反复重打。
            row.insert("alreadyCompleted".into(), json!(true));
            row.insert(
                "msg".into(),
                json!(if award.total() > 0 {
                    format!("今日已签到（官方报出当日奖励 {} 积分）", award.total())
                } else {
                    "今日已签到".to_string()
                }),
            );
            Some(award)
        }
        Outcome::Claimed(award) => {
            row.insert("success".into(), json!(true));
            row.insert("status".into(), json!("claimed"));
            row.insert(
                "msg".into(),
                json!(if award.total() > 0 {
                    format!(
                        "签到成功（官方报出当日奖励 {} 积分；到账看余额列）",
                        award.total()
                    )
                } else {
                    // 回查确认了"今天签过"，但上游没给数额 —— 不编一个数字出来
                    "签到成功（上游未给出奖励数额）".to_string()
                }),
            );
            Some(award)
        }
        Outcome::Rejected(code, message) => {
            row.insert("success".into(), json!(false));
            row.insert("code".into(), json!(code));
            // 上游给了文案就原样带出，没给才说"未给"。
            let reason = if message.is_empty() {
                "上游未给文案"
            } else {
                message.as_str()
            };
            let text = if code == CODE_REJECTED {
                // 两种读法都写出来，因为一手记录只到"偏向"的程度。
                format!(
                    "官方拒绝本次签到（{code}）：{reason}；这一码有两说 —— 参考实现判产品谱系/设备画像错配，\
                     同族端点的外部实测判「按 device id 限流、换张派生号就领得到」。本轮只发了一发，\
                     当日不再自动重试，想再试就再点一次签到"
                )
            } else if code == CODE_TOKEN_DEAD {
                format!(
                    "官方拒绝本次签到（{code}）：{reason}；这一码判为「服务端已作废这张登录态」\
                     （本地到期时间看不出来），处置是换发新令牌后重放一轮"
                )
            } else {
                // 不猜含义 —— 跨家搬码表正是把别家结论当自家事实的那类错。
                format!(
                    "官方拒绝本次签到（{code}）：{reason}；这一码本家没有实测依据，只按失败上报，当日台账不落"
                )
            };
            row.insert("msg".into(), json!(text));
            None
        }
        Outcome::Unconfirmed => {
            row.insert("success".into(), json!(false));
            row.insert("claimUnconfirmed".into(), json!(true));
            row.insert(
                "msg".into(),
                json!("签到返回成功但同设备号回查未确认（服务端静默拒签），当日不再自动重试"),
            );
            None
        }
    };
    if let Some(award) = awarded {
        row.insert("awarded".into(), json!(award.total()));
        row.insert("checkinCredits".into(), json!(award.credits));
        row.insert("checkinExtraCredits".into(), json!(award.extra_credits));
    }
    row.insert("checkinDeviceId".into(), json!(device_id));
    Value::Object(row)
}

pub use claim_daily as claim_daily_checkin;
