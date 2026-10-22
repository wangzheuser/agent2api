//! Trae 国内 SOLO 的每日签到。
//!
//! 这条链与模型额度查询都走 `api.trae.cn`，但签到钱包和模型积分池是两份账，
//! 所以只把领取结果归一成签到结果，不把它混进 `usage` 的 credits pool。
//!
//! ── 9074 不是"限流" ────────────────────────────────────
//! 这一码曾按"设备限流、稍后重试"处理（含换一代设备号再打一发）。2026-10-05
//! 的双侧对照推翻了那个读法：同一账号、同一时刻、同一套头，绑了设备的令牌
//! 回 `code:0`，没绑的回 9074；改 UA / body / 设备号都动不了它
//! （yetone/magpie#808，magpie 插件 0.2.3 起改走带 `DeviceProof` 的续期）。
//!
//! 所以现在的口径是：
//!   · 签到落在**客户端在设备服务里登记过的那台设备**上（见 [`device_id`] 与
//!     `client_device`），一轮内不换号；
//!   · 9074 一发即收口，文案不再说"限流"，也不再自动重试；
//!   · 9095（设备今日已签）算当天的"已完成"，不是失败；
//!   · 9004（缺设备号）与 4014 / 1001（凭证被拒/作废）只能重新登录。

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
/// 设备今天已经签过 —— 当天的最终答案，不是失败。
const DEVICE_SIGNED_CODE: i64 = 9095;
/// 缺 `x-device-id`（或格式不合法）时上游回这一码。
const DEVICE_MISSING_CODE: i64 = 9004;
/// 服务端拒绝认证（HTTP 200 + 业务码）：凭证被踢下线，只能重新登录。
const AUTH_REJECTED_CODE: i64 = 4014;

/// 查询并领取一次 Trae 每日积分。返回形状与其它签到 provider 对齐：
/// `{success, msg, status, rewardPoints?, alreadyClaimed?}`。
pub async fn claim_daily(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    claim_daily_at(store, account_id, UG_HOST).await
}

/// 与 [`claim_daily`] 同一条链，只是端点可注入。
///
/// 生产恒用 `UG_HOST`；测试用本地 mock，这样「账号落库 → 读凭据 → 续期 →
/// status/claim/回查」能在真 SQLite + 真 HTTP 上跑通，而不是只测纯函数。
pub(crate) async fn claim_daily_at(
    store: &AccountStore,
    account_id: &str,
    base: &str,
) -> Result<Value, GatewayError> {
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

    claim_with_refresh_at(credential, proxy, base, || async {
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
    // 一轮一个号，status → claim → 回查三发之间保持不变；撞 9074 不换号再试
    // （见 `device_id` 的说明）。
    let device_id = checkin_device_id(&credential);
    let status = match request(&credential, &device_id, STATUS_PATH, proxy, base).await? {
        ReplyKind::Auth => return Err(ClaimError::Auth),
        ReplyKind::DeviceBusy => {
            return Ok(backoff(&device_id, "Trae 签到状态接口", DEVICE_BUSY_CODE))
        }
        ReplyKind::Body(body) => body,
    };

    if status_code(&status) != 0 {
        return Err(classify_reply(status, "Trae 签到状态查询失败"));
    }
    let before = status_of(&status);
    if before.done_today() || !before.enable {
        return Ok(complete(
            decide(&before, &Status::default(), None),
            &device_id,
        ));
    }
    match request(&credential, &device_id, CLAIM_PATH, proxy, base).await? {
        ReplyKind::Auth => Err(ClaimError::Auth),
        ReplyKind::DeviceBusy => Ok(backoff(&device_id, "Trae 签到领取接口", DEVICE_BUSY_CODE)),
        ReplyKind::Body(body) => {
            let code = status_code(&body);
            if code == AUTH_EXPIRED_CODE {
                return Err(ClaimError::Auth);
            }
            if code == DEVICE_SIGNED_CODE {
                // 设备今天已经签过了：这是当天的最终答案，不是失败，也不能换号。
                return Ok(device_signed(&device_id));
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
    // 4014 是 HTTP 200 里的业务码（"not able to authenticate you"），语义与
    // 401 相同：凭证被服务端踢下线，重试无用，只能重新 OAuth。把它当普通
    // 失败退避，只会让账号每天白撞一次。
    if reply.status == 401
        || code == 401
        || code == AUTH_EXPIRED_CODE
        || code == AUTH_REJECTED_CODE
    {
        return Ok(ReplyKind::Auth);
    }
    if code == DEVICE_BUSY_CODE {
        return Ok(ReplyKind::DeviceBusy);
    }
    if code == DEVICE_MISSING_CODE {
        return Ok(ReplyKind::Body(json!({
            "code": DEVICE_MISSING_CODE,
            "msg": "上游判定缺少或无法识别设备号（9004）：该账号没有可用的设备绑定，请重新登录后再签到",
        })));
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

/// 一轮签到用的设备号：**账号自己的那台设备**。
///
/// ── 「自己的设备」有两种，签到只认后者 ────────────────────
/// ① 登录时（`oauth.rs`）随机生成的 `DeviceInfo.DeviceID`（落盘在
///    `credential.device_id`）：位数对，但**不是客户端在设备服务里登记过的那台**。
/// ② 本机 Trae CN 客户端 `storage.json` 里 `iCubeAuthInfo://icube-dc:<号>` 的号：
///    这才是 `claim` 认的设备。
///
/// 2026-10-22 同一账号实测：用 ① 时 `status` 回 `code:0` 而 `claim` 回 9074；
/// 换成 ② 后 `claim` 当场 `code:0`、当日 200 积分到账（详见 `client_device`）。
///
/// 这里的 [`device_id`] 只做 ② 之后的形状处理（① 已经是 15/16 位就直接用，
/// 老数据的 19 位/空值按 `uid + device_id` 稳定派生）—— 真正挑哪台设备是
/// [`checkin_device_id`] 的事。
///
/// ── 9074 之后为什么不再换号 ───────────────────────────────
/// 上一版在状态接口撞 9074 时换一代设备号再打一发。那既不是稳定身份
/// （风控画像里的"同一账号换设备"），也让"当天到底签没签过"变得不可判定。
/// 两家同族实现都不这么做：9074 一发即收口，9095 就是当天的答案。
pub fn device_id(credential: &Credential) -> String {
    derived_device_id(credential)
}

/// 一轮签到实际用的设备号：**客户端登记过的那台优先**。
///
/// 顺序（每一层都有明确理由，不要合并成一条兜底链）：
///   1. 本机 Trae CN 客户端登录的**同一个 uid** 的设备号 —— 唯一被上游认的
///      "这台设备"（见 `client_device` 模块头的实测）；
///   2. 凭据里已经是客户端形状的 `deviceId` —— 手工粘贴/登录时带了真号；
///   3. 用「凭据设备号 + uid」派生一个稳定的 16 位号 —— 老数据与无客户端环境
///      的兜底，行为与改动前一致。
///
/// 第 1 层不放宽的代价是"没装客户端就只能继续 9074"，但放成"随便读一个设备号"
/// 会退化得更严重：整个网关的多条账号会被塞进同一台设备，上游按设备每日去重，
/// 除第一个之外全部失败。
fn checkin_device_id(credential: &Credential) -> String {
    if let Some(device) = super::client_device::device_for_uid(credential.uid.trim()) {
        return device.device_id;
    }
    derived_device_id(credential)
}

/// 凭据自己带的设备号（客户端形状直接用，否则稳定派生）。
fn derived_device_id(credential: &Credential) -> String {
    let recorded = credential.device_id.trim();
    if is_client_shaped(recorded) {
        return recorded.to_string();
    }
    // 老数据：19 位或空。按「自己的设备号 + uid」派生，跨轮稳定。
    let seed = format!("trae-cn-device\0{}\0{}", recorded, credential.uid.trim());
    let digest = Sha256::digest(seed.as_bytes());
    let mut number = 0u128;
    for byte in digest.iter() {
        number = (number * 256 + u128::from(*byte)) % DEVICE_MODULUS;
    }
    format!("{number:016}")
}

/// 客户端下发的设备号是 15 或 16 位纯数字（`iCubeAuthInfo://icube-dc:<id>`）。
fn is_client_shaped(device_id: &str) -> bool {
    matches!(device_id.len(), 15 | 16) && device_id.bytes().all(|byte| byte.is_ascii_digit())
}

/// 9074：上游拒绝这次领取，但**不是限流**。
///
/// 2026-10-22 的真实账号实测把这一码定死：**同一 token、同一套头**，
/// 用登录时随机生成的 `deviceId` → `claim` 回 9074（`status` 却是 `code:0`）；
/// 换成本机客户端 `storage.json` 里登记的那台设备号 → `claim` 立刻 `code:0`。
/// 期间改 UA / 头形状 / `{"req_source":1}` 请求体 / 出口 IP 都不改变结论。
///
/// 所以文案不能再写"限流，请稍后重试"（那会把人引向"过一会儿再点"），也
/// 不该再提"先确认续期过" —— 续期解决的是令牌，不是设备登记。
fn backoff(device_id: &str, where_: &str, code: i64) -> Value {
    json!({
        "success": false,
        "status": "backoff",
        "code": code,
        "checkinDeviceId": device_id,
        "msg": format!(
            "{where_}被上游拒绝（{code}）。这一码不是限流，是设备身份不被认：该账号的签到\
             必须落在 Trae CN 客户端登记过的那台设备上（当前用的是 {device_id}）。\
             请在本机 Trae CN 客户端登录**这个账号**后重试，本次不再自动重试"
        ),
    })
}

/// 9095：这台设备今天已经签过 —— 落在「已完成」而不是「失败」。
fn device_signed(device_id: &str) -> Value {
    json!({
        "success": false,
        "status": "already_claimed",
        "code": DEVICE_SIGNED_CODE,
        "alreadyClaimed": true,
        // 台账按"今日已完成"记，明天照常再试；当成失败会让它在当天反复重打。
        "alreadyCompleted": true,
        "checkinDeviceId": device_id,
        "msg": "该设备今日已签到（上游按设备每日只计一次；同设备上的其他账号今天领不到）",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checkin_device_is_the_accounts_own_when_the_client_shape_is_known() {
        let credential = Credential {
            uid: "u1".into(),
            device_id: "1234567890123456".into(),
            ..Default::default()
        };
        // 登录时上游认下的那台设备，原样用，不另造一个。
        assert_eq!("1234567890123456", device_id(&credential));
        // 15 位（真机实测存在）同样直接采用。
        let fifteen = Credential {
            uid: "u1".into(),
            device_id: "123456789012345".into(),
            ..Default::default()
        };
        assert_eq!("123456789012345", device_id(&fifteen));
    }

    #[test]
    fn a_legacy_device_id_derives_one_stable_16_digit_number() {
        for legacy in ["1234567890123456789", ""] {
            let credential = Credential {
                uid: "u1".into(),
                device_id: legacy.into(),
                ..Default::default()
            };
            let id = device_id(&credential);
            assert_eq!(16, id.len(), "{legacy}");
            assert!(id.bytes().all(|byte| byte.is_ascii_digit()), "{legacy}");
            // 跨轮相同（不随日期/代数漂），且不与其他账号共用。
            assert_eq!(id, device_id(&credential));
            let other = Credential {
                uid: "u2".into(),
                device_id: legacy.into(),
                ..Default::default()
            };
            assert_ne!(id, device_id(&other));
        }
        // 19 位不能被"截短成 16 位"糊弄过去：那是另一台设备的号。
        let nineteen = Credential {
            uid: "u1".into(),
            device_id: "1234567890123456789".into(),
            ..Default::default()
        };
        assert_ne!("1234567890123456", device_id(&nineteen));
    }

    #[test]
    fn the_client_registered_device_wins_only_for_the_same_account() {
        // 本机客户端登录的是哪个号，就只能给哪个号用它的设备号：
        // 借用别的号的设备号会让上游按"同一台设备"去重，本号反而签不到。
        let ours = Credential {
            uid: "762424146795179".into(),
            device_id: "7264783250393601".into(),
            ..Default::default()
        };
        let client = crate::server::core::providers::trae::client_device::device_for_uid(&ours.uid);
        match client {
            // 没装客户端/没登录时，退回凭据自己的号 —— 行为与改动前一致。
            None => assert_eq!("7264783250393601", checkin_device_id(&ours)),
            Some(device) => {
                assert_eq!(device.device_id, checkin_device_id(&ours));
                assert_eq!(ours.uid, device.uid);
                // 同一条凭据挂了别的 uid 时不能复用这台设备。
                let other = Credential {
                    uid: "7478516977494033416".into(),
                    ..ours.clone()
                };
                assert_ne!(device.device_id, checkin_device_id(&other));
            }
        }
    }

    #[test]
    fn the_device_signed_code_is_the_days_answer_not_a_failure() {
        let row = device_signed("1234567890123456");
        assert_eq!(row["code"], 9095);
        assert_eq!(row["status"], "already_claimed");
        assert_eq!(row["alreadyCompleted"], true);
        assert!(row.get("awarded").is_none());
        assert_eq!(Probe::GiveUp, probe_for(9095));
    }

    #[test]
    fn the_backoff_message_no_longer_calls_9074_a_rate_limit() {
        let row = backoff("1234567890123456", "Trae 签到领取接口", 9074);
        let message = row["msg"].as_str().unwrap_or_default();
        assert!(!message.contains("设备限流"), "{message}");
        assert!(message.contains("不是限流"), "{message}");
        assert_eq!(row["code"], 9074);
        // 平台限流与凭证被拒都有各自的终点，不能都落到"换方案再试"。
        assert_eq!(Probe::GiveUp, probe_for(9004));
        assert_eq!(Probe::GiveUp, probe_for(4014));
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
///
/// 这一层现在**只用于分类**：9074 的"下一个探测体"是空集（`probe_bodies`
/// 恒一发），所以真正的收口在 `claim_once` 里，不在这里重试。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// 采纳这一发
    Accept,
    /// 9074：换下一个探测体（体用尽则整体失败）
    NextBody,
    /// 其它非零码：换下一个鉴权方案（方案用尽则整体失败）
    NextScheme,
    /// 终点：1001（JWT 已作废）、9095（设备今日已签）、9004（缺设备号）、
    /// 4014（凭证被拒）。前两码的处置在驱动层或 `claim_once`，后两码只能重登。
    GiveUp,
}

pub fn probe_for(code: i64) -> Probe {
    match code {
        CODE_OK => Probe::Accept,
        CODE_REJECTED => Probe::NextBody,
        CODE_TOKEN_DEAD | DEVICE_SIGNED_CODE | DEVICE_MISSING_CODE | AUTH_REJECTED_CODE => {
            Probe::GiveUp
        }
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
