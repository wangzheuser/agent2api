//! 通过签到/活动任务领取额度的自定义提供商适配器。
//!
//! 该模块只处理奖励上游的 HTTP 协议。模型转发使用 custom provider 的
//! `apiKey`，奖励链路使用独立的字符串凭证；任何公开结构都只包含规范化的
//! 奖励与钱包字段，不回显 Cookie、Bearer token 或上游原始响应。

use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, ORIGIN, REFERER,
    USER_AGENT,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const ASTUDIO_BASE: &str = "https://agent.xfyun.cn/xingchen-studio";
const DUMATE_BASE: &str = "https://www.dumate.cn";
const MINIMAX_BASE: &str = "https://agent.minimax.cn";
const LOBSTER_BASE: &str = "https://lobsterai-server.youdao.com";
const LOBSTER_CLIENT_VERSION: &str = "2026.9.4";
const MINIMAX_TIMEZONE: &str = "Asia/Shanghai";

/// 四家奖励 profile 的稳定 ID。这个 ID 会写入 custom provider 配置，不能随意改名。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum RewardProfileId {
    #[serde(rename = "astudio")]
    AStudio,
    #[serde(rename = "dumate")]
    DuMate,
    #[serde(rename = "minimax-code")]
    MiniMaxCode,
    #[serde(rename = "lobsterai")]
    LobsterAI,
}

impl RewardProfileId {
    pub const ALL: [Self; 4] = [
        Self::AStudio,
        Self::DuMate,
        Self::MiniMaxCode,
        Self::LobsterAI,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AStudio => "astudio",
            Self::DuMate => "dumate",
            Self::MiniMaxCode => "minimax-code",
            Self::LobsterAI => "lobsterai",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::AStudio => "AStudio",
            Self::DuMate => "DuMate",
            Self::MiniMaxCode => "MiniMax Code",
            Self::LobsterAI => "LobsterAI",
        }
    }

    pub const fn credential_type(self) -> &'static str {
        match self {
            Self::AStudio | Self::DuMate => "cookie",
            Self::MiniMaxCode | Self::LobsterAI => "access-token",
        }
    }
}

impl fmt::Display for RewardProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RewardProfileId {
    type Err = RewardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_profile(value)
    }
}

/// Profile 目录公开形态。端点是文档信息，不包含实际凭证。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardProfileInfo {
    pub id: String,
    pub label: String,
    pub credential_type: String,
    pub status_endpoint: String,
    pub claim_endpoint: String,
}

/// 统一的奖励状态。`reward`/`wallet` 只包含适配器白名单字段。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardStatus {
    pub profile: String,
    pub claimable: bool,
    pub already_completed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reward: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 统一的领取结果。成功与「已领取」都属于幂等完成态，但含义由两个字段区分。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardClaim {
    pub profile: String,
    pub success: bool,
    pub already_completed: bool,
    pub claimable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reward: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 上游错误的安全表示。错误中不携带 URL 查询串、请求头或原始 body。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardError {
    pub message: String,
    pub status_code: Option<u16>,
}

impl RewardError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status_code: None,
        }
    }

    fn http(status: u16) -> Self {
        Self {
            message: format!("奖励上游 HTTP {status}"),
            status_code: Some(status),
        }
    }
}

impl fmt::Display for RewardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RewardError {}

/// 把配置中的字符串解析为稳定 profile ID。
pub fn parse_profile(value: &str) -> Result<RewardProfileId, RewardError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "astudio" | "a-studio" => Ok(RewardProfileId::AStudio),
        "dumate" | "du-mate" => Ok(RewardProfileId::DuMate),
        "minimax-code" | "minimax" | "minimaxcode" => Ok(RewardProfileId::MiniMaxCode),
        "lobsterai" | "lobster-ai" => Ok(RewardProfileId::LobsterAI),
        _ => Err(RewardError::invalid("未知奖励提供商")),
    }
}

/// 配置落盘使用的规范化字符串（兼容少量旧别名）。
pub fn normalize_profile_id(value: &str) -> Result<String, RewardError> {
    Ok(parse_profile(value)?.as_str().to_string())
}

/// 判断字符串是否为当前已注册的 profile。
pub fn is_known_profile(value: &str) -> bool {
    parse_profile(value).is_ok()
}

/// 返回四家预置 profile，不读取任何账号凭证。
pub fn list_profiles() -> Vec<RewardProfileInfo> {
    RewardProfileId::ALL
        .into_iter()
        .map(|profile| RewardProfileInfo {
            id: profile.as_str().to_string(),
            label: profile.label().to_string(),
            credential_type: profile.credential_type().to_string(),
            status_endpoint: match profile {
                RewardProfileId::AStudio => format!("{ASTUDIO_BASE}/points/balance"),
                RewardProfileId::DuMate => {
                    format!("{DUMATE_BASE}/api/dumate/points/loginBonusInfo")
                }
                RewardProfileId::MiniMaxCode => {
                    format!("{MINIMAX_BASE}/minimax-cloud/api/v1/signin/status")
                }
                RewardProfileId::LobsterAI => format!("{LOBSTER_BASE}/api/client-activities/slot"),
            },
            claim_endpoint: match profile {
                RewardProfileId::AStudio => format!("{ASTUDIO_BASE}/tenant-app/init-web"),
                RewardProfileId::DuMate => format!("{DUMATE_BASE}/api/dumate/points/loginBonus"),
                RewardProfileId::MiniMaxCode => {
                    format!("{MINIMAX_BASE}/minimax-cloud/api/v1/signin/claim")
                }
                RewardProfileId::LobsterAI => format!(
                    "{LOBSTER_BASE}/api/client-activities/{{activityCode}}/actions/check_in"
                ),
            },
        })
        .collect()
}

fn validate_credential(credential: &str, kind: &str) -> Result<(), RewardError> {
    let value = credential.trim();
    if value.is_empty() {
        return Err(RewardError::invalid(format!("{kind} 凭证为空")));
    }
    if value.contains('\r') || value.contains('\n') {
        return Err(RewardError::invalid("凭证包含非法换行"));
    }
    Ok(())
}

fn header_value(value: &str) -> Result<HeaderValue, RewardError> {
    HeaderValue::from_str(value).map_err(|_| RewardError::invalid("凭证包含非法请求头字符"))
}

fn client_ref(client: Option<&reqwest::Client>) -> reqwest::Client {
    client.cloned().unwrap_or_else(reqwest::Client::new)
}

async fn response_json(
    response: reqwest::Response,
    credential: &str,
) -> Result<Value, RewardError> {
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|_| RewardError::invalid("奖励上游响应读取失败"))?;
    if !(200..300).contains(&status) {
        return Err(RewardError::http(status));
    }
    let value = serde_json::from_slice::<Value>(&body)
        .map_err(|_| RewardError::invalid("奖励上游返回无效 JSON"))?;
    if let Some(code) = response_code(&value) {
        if code != 0 {
            let message = response_message(&value, credential);
            return Err(RewardError {
                message,
                status_code: Some(status),
            });
        }
    }
    // DuMate 的网页接口在部分业务失败场景只返回 `success: false`，不一定
    // 同时带标准 code。把这类响应按业务错误处理，避免把“未领取”当成成功。
    if value.get("success").and_then(Value::as_bool) == Some(false)
        || value.get("ok").and_then(Value::as_bool) == Some(false)
    {
        return Err(RewardError {
            message: response_message(&value, credential),
            status_code: Some(status),
        });
    }
    Ok(value)
}

fn response_code(value: &Value) -> Option<i64> {
    // 不同站点会把业务状态包在 `data`/`result` 中，MiniMax 还使用
    // `base_resp.status_code`。只检查顶层会把 `{data:{code:...}}` 的失败
    // 当作成功，进而误记一次签到。
    [
        "/code",
        "/statusCode",
        "/status_code",
        "/base_resp/status_code",
        "/data/code",
        "/data/statusCode",
        "/data/status_code",
        "/data/base_resp/status_code",
        "/result/code",
        "/result/statusCode",
        "/result/status_code",
        "/result/base_resp/status_code",
    ]
    .iter()
    .find_map(|path| value.pointer(path).and_then(number_i64))
}

fn response_message(value: &Value, credential: &str) -> String {
    let msg = [
        "/message",
        "/msg",
        "/desc",
        "/error",
        "/status_msg",
        "/data/message",
        "/data/msg",
        "/data/desc",
        "/data/error",
        "/data/status_msg",
        "/result/message",
        "/result/msg",
        "/result/desc",
        "/result/error",
        "/result/status_msg",
    ]
    .iter()
    .find_map(|path| value.pointer(path).and_then(Value::as_str))
    .unwrap_or("奖励上游返回业务错误");
    let mut safe = msg.chars().take(160).collect::<String>();
    if !credential.is_empty() {
        safe = safe.replace(credential, "<redacted>");
    }
    if safe.trim().is_empty() {
        "奖励上游返回业务错误".into()
    } else {
        safe
    }
}

fn payload(value: &Value) -> Value {
    value
        .get("data")
        .or_else(|| value.get("result"))
        .cloned()
        .unwrap_or_else(|| value.clone())
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn number_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
        .or_else(|| number(value).map(|n| n as i64))
}

fn boolish(value: Option<&Value>) -> bool {
    value
        .and_then(|v| v.as_bool())
        .or_else(|| value.and_then(|v| v.as_i64()).map(|n| n != 0))
        .or_else(|| {
            value.and_then(|v| v.as_str()).map(|s| {
                matches!(
                    s.to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "claimed" | "completed"
                )
            })
        })
        .unwrap_or(false)
}

fn first_number(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| value.get(*key).and_then(number))
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).map(str::to_owned))
}

fn reward_object(entries: impl IntoIterator<Item = (&'static str, Option<f64>)>) -> Option<Value> {
    let mut map = Map::new();
    for (key, value) in entries {
        if let Some(value) = value {
            map.insert(key.to_string(), json!(value));
        }
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

fn wallet_whitelist(value: &Value, keys: &[&str]) -> Option<Value> {
    let mut map = Map::new();
    for key in keys {
        if let Some(item) = value.get(*key) {
            if item.is_number() || item.is_boolean() || item.is_string() || item.is_array() {
                map.insert((*key).to_string(), item.clone());
            }
        }
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

fn parse_astudio_wallet(value: &Value) -> Option<Value> {
    wallet_whitelist(
        &payload(value),
        &[
            "totalAmount",
            "totalBalance",
            "sparkTotalAmount",
            "sparkTotalBalance",
            "memberTotal",
            "memberBalance",
            "buyTotal",
            "buyBalance",
            "activityTotal",
            "activityBalance",
            "sparkNextExpireTime",
            "activityNextExpireTime",
        ],
    )
}

fn parse_astudio_status(value: &Value) -> RewardStatus {
    let data = payload(value);
    RewardStatus {
        profile: RewardProfileId::AStudio.to_string(),
        claimable: true,
        already_completed: false,
        reward: None,
        wallet: parse_astudio_wallet(&data),
        expires_at: first_string(
            &data,
            &[
                "nextExpireTime",
                "activityNextExpireTime",
                "sparkNextExpireTime",
            ],
        ),
        message: None,
    }
}

fn astudio_granted(value: &Value) -> (Option<f64>, Option<f64>) {
    let data = payload(value);
    (
        first_number(&data, &["totalAmount"]),
        first_number(&data, &["sparkTotalAmount"]),
    )
}

fn parse_astudio_claim(before: &Value, after: &Value, response: &Value) -> RewardClaim {
    let (before_total, before_spark) = astudio_granted(before);
    let (after_total, after_spark) = astudio_granted(after);
    let delta = match (before_total, after_total) {
        (Some(a), Some(b)) if b >= a => Some(b - a),
        _ => None,
    };
    let spark_delta = match (before_spark, after_spark) {
        (Some(a), Some(b)) if b >= a => Some(b - a),
        _ => None,
    };
    let code_ok = response_code(response).is_none_or(|code| code == 0);
    let reward = reward_object([
        ("points", delta.filter(|v| *v > 0.0)),
        ("sparkPoints", spark_delta.filter(|v| *v > 0.0)),
    ]);
    let already = code_ok && reward.is_none();
    RewardClaim {
        profile: RewardProfileId::AStudio.to_string(),
        success: code_ok,
        already_completed: already,
        claimable: !already,
        reward,
        wallet: parse_astudio_wallet(after),
        expires_at: first_string(
            &payload(after),
            &[
                "nextExpireTime",
                "activityNextExpireTime",
                "sparkNextExpireTime",
            ],
        ),
        message: if already {
            Some("今日已领取或上游未发放新积分".into())
        } else {
            Some("签到完成".into())
        },
    }
}

fn parse_dumate_status(value: &Value) -> RewardStatus {
    let data = payload(value);
    let already = boolish(data.get("hasIssued").or_else(|| data.get("has_issued")))
        || boolish(
            data.get("alreadyCompleted")
                .or_else(|| data.get("already_completed")),
        );
    let points = first_number(
        &data,
        &[
            "points",
            "rewardPoints",
            "bonusPoints",
            "loginBonus",
            "amount",
        ],
    );
    RewardStatus {
        profile: RewardProfileId::DuMate.to_string(),
        claimable: !already,
        already_completed: already,
        reward: reward_object([("points", points)]),
        wallet: wallet_whitelist(
            &data,
            &[
                "totalPoints",
                "usedPoints",
                "left",
                "monthPoints",
                "month_points",
            ],
        ),
        expires_at: first_string(&data, &["expireAt", "expire_at", "expireTime"]),
        message: None,
    }
}

fn parse_dumate_claim(status: &RewardStatus, value: &Value) -> RewardClaim {
    let data = payload(value);
    let code_ok = response_code(value).is_none_or(|code| code == 0);
    let explicit_already = boolish(
        data.get("hasIssued")
            .or_else(|| data.get("alreadyCompleted"))
            .or_else(|| data.get("already_completed")),
    ) || ["already", "claimed", "issued"]
        .iter()
        .any(|key| data.get(*key).is_some_and(|v| boolish(Some(v))));
    let already = status.already_completed || explicit_already;
    let reward = reward_object([(
        "points",
        first_number(&data, &["points", "rewardPoints", "bonusPoints", "amount"]),
    )])
    .or_else(|| status.reward.clone());
    RewardClaim {
        profile: RewardProfileId::DuMate.to_string(),
        success: code_ok && (already || reward.is_some()),
        already_completed: already,
        claimable: !already,
        reward,
        wallet: wallet_whitelist(
            &data,
            &[
                "totalPoints",
                "usedPoints",
                "left",
                "monthPoints",
                "month_points",
            ],
        ),
        expires_at: first_string(&data, &["expireAt", "expire_at", "expireTime"])
            .or_else(|| status.expires_at.clone()),
        message: if already {
            Some("今日已领取".into())
        } else {
            Some("签到完成".into())
        },
    }
}

fn minimax_panel(value: &Value) -> Value {
    let data = payload(value);
    data.get("panel")
        .or_else(|| data.get("signin_panel"))
        .or_else(|| value.pointer("/data/panel"))
        .cloned()
        .unwrap_or(data)
}

fn minimax_today(value: &Value) -> (bool, bool, Option<f64>, Option<String>) {
    let data = payload(value);
    let panel = minimax_panel(value);
    let days = panel.get("days").and_then(Value::as_array);
    if let Some(days) = days {
        for day in days {
            if boolish(day.get("is_today").or_else(|| day.get("isToday"))) {
                let status = day.get("status").and_then(number_i64).unwrap_or(0);
                let points = first_number(day, &["points", "rewardPoints"]);
                let expiry = first_string(day, &["expire_at", "expireAt", "expireAtMs"]);
                return (status == 2, status == 3, points, expiry);
            }
        }
    }
    let already = boolish(
        data.get("checkedInToday")
            .or_else(|| data.get("checked_in_today"))
            .or_else(|| data.get("alreadyCompleted")),
    );
    let claimable = boolish(
        data.get("claimableToday")
            .or_else(|| data.get("claimable_today")),
    );
    (
        claimable,
        already,
        first_number(&data, &["todayPoints", "points", "rewardPoints"]),
        first_string(&data, &["expireAt", "expire_at"]),
    )
}

fn parse_minimax_status(value: &Value) -> RewardStatus {
    let (claimable, already, points, expires_at) = minimax_today(value);
    RewardStatus {
        profile: RewardProfileId::MiniMaxCode.to_string(),
        claimable,
        already_completed: already,
        reward: reward_object([("points", points)]),
        wallet: wallet_whitelist(
            &payload(value),
            &["credits", "balance", "quota", "totalCreditsRemaining"],
        ),
        expires_at,
        message: None,
    }
}

fn parse_minimax_claim(status: &RewardStatus, value: &Value) -> RewardClaim {
    let data = payload(value);
    let result = data
        .get("claim_result")
        .or_else(|| data.get("claimResult"))
        .and_then(number_i64);
    let already = result == Some(2) || status.already_completed;
    let success =
        response_code(value).is_none_or(|code| code == 0) && (already || result == Some(1));
    RewardClaim {
        profile: RewardProfileId::MiniMaxCode.to_string(),
        success,
        already_completed: already,
        claimable: !already,
        reward: reward_object([("points", first_number(&data, &["points", "rewardPoints"]))])
            .or_else(|| status.reward.clone()),
        wallet: wallet_whitelist(
            &data,
            &["credits", "balance", "quota", "totalCreditsRemaining"],
        ),
        expires_at: first_string(&data, &["expireAt", "expire_at", "expire_at_ms"])
            .or_else(|| status.expires_at.clone()),
        message: if already {
            Some("今天已签到".into())
        } else if success {
            Some("签到完成".into())
        } else {
            Some("签到未完成".into())
        },
    }
}

fn parse_lobster_wallet(value: &Value) -> Option<Value> {
    let data = payload(value);
    let mut map = Map::new();
    for key in ["totalCreditsRemaining", "creditsRemaining", "totalCredits"] {
        if let Some(item) = data.get(key).filter(|v| v.is_number()) {
            map.insert(key.into(), item.clone());
        }
    }
    if let Some(items) = data.get("creditItems").and_then(Value::as_array) {
        let clean = items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                let mut row = Map::new();
                for key in ["type", "label", "creditsRemaining", "expiresAt"] {
                    if let Some(value) = object.get(key).filter(|v| v.is_number() || v.is_string())
                    {
                        row.insert(key.into(), value.clone());
                    }
                }
                (!row.is_empty()).then_some(Value::Object(row))
            })
            .collect::<Vec<_>>();
        if !clean.is_empty() {
            map.insert("creditItems".into(), Value::Array(clean));
        }
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

#[derive(Clone, Debug)]
struct LobsterState {
    status: RewardStatus,
    code: String,
    revision: i64,
}

fn path_segment(value: &str) -> String {
    // `form_urlencoded` 使用 `+` 表示空格，这是 query/form 的规则，不是
    // path segment 的规则。活动 code 来自上游，必须按 RFC 3986 的 unreserved
    // 集合编码，避免 `+`、`/` 等字符改变实际路由。
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(
                char::from_digit((byte >> 4) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            encoded.push(
                char::from_digit((byte & 0x0f) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
    encoded
}

fn parse_lobster_slot(value: &Value) -> Option<(String, i64)> {
    let data = payload(value);
    if data.get("slotState").and_then(Value::as_str) != Some("available") {
        return None;
    }
    let activity = data.get("activity")?.as_object()?;
    let code = activity
        .get("activityCode")
        .or_else(|| activity.get("activity_code"))?
        .as_str()?
        .trim();
    if code.is_empty() {
        return None;
    }
    let revision = activity
        .get("configRevision")
        .or_else(|| activity.get("config_revision"))
        .and_then(number_i64)
        .unwrap_or(0);
    Some((code.to_string(), revision))
}

fn parse_lobster_context(slot: &Value, context: &Value, wallet: Option<Value>) -> LobsterState {
    let (code, revision) = parse_lobster_slot(slot).unwrap_or_default();
    let data = payload(context);
    let claimed = boolish(
        data.pointer("/state/claimedToday")
            .or_else(|| data.get("claimedToday")),
    );
    let has_action = data
        .get("actions")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|v| v.as_str() == Some("check_in")));
    let points = first_number(
        &data,
        &["creditsGranted", "rewardCredits", "credits", "points"],
    );
    LobsterState {
        status: RewardStatus {
            profile: RewardProfileId::LobsterAI.to_string(),
            claimable: !claimed && has_action && !code.is_empty(),
            already_completed: claimed,
            reward: reward_object([("credits", points)]),
            wallet,
            expires_at: first_string(&data, &["expireAt", "expiresAt", "expire_at"]),
            message: None,
        },
        code,
        revision,
    }
}

fn parse_lobster_claim(status: &RewardStatus, value: &Value, wallet: Option<Value>) -> RewardClaim {
    let data = payload(value);
    let already = status.already_completed
        || boolish(
            data.get("already")
                .or_else(|| data.get("claimedToday"))
                .or_else(|| data.get("alreadyCompleted")),
        );
    let reward = reward_object([(
        "credits",
        first_number(
            &data,
            &["creditsGranted", "rewardCredits", "credits", "points"],
        ),
    )])
    .or_else(|| status.reward.clone());
    let success =
        response_code(value).is_none_or(|code| code == 0) && (already || reward.is_some());
    RewardClaim {
        profile: RewardProfileId::LobsterAI.to_string(),
        success,
        already_completed: already,
        claimable: !already,
        reward,
        wallet,
        expires_at: first_string(&data, &["expireAt", "expiresAt", "expire_at"])
            .or_else(|| status.expires_at.clone()),
        message: if already {
            Some("今日已领取".into())
        } else if success {
            Some("签到完成".into())
        } else {
            Some("签到未完成".into())
        },
    }
}

fn astudio_headers(credential: &str, json_body: bool) -> Result<HeaderMap, RewardError> {
    validate_credential(credential, "AStudio Cookie")?;
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(USER_AGENT, HeaderValue::from_static("AStudio/3.4.4"));
    headers.insert("clientType", HeaderValue::from_static("21"));
    headers.insert("studioVersion", HeaderValue::from_static("3.4.4"));
    let mut cookie = header_value(credential.trim())?;
    cookie.set_sensitive(true);
    headers.insert(COOKIE, cookie);
    if json_body {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    Ok(headers)
}

fn dumate_headers(credential: &str, json_body: bool) -> Result<HeaderMap, RewardError> {
    validate_credential(credential, "DuMate Cookie")?;
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36"));
    headers.insert("X-Dumate-Client-Type", HeaderValue::from_static("web"));
    headers.insert(ORIGIN, HeaderValue::from_static(DUMATE_BASE));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://www.dumate.cn/app"),
    );
    let mut cookie = header_value(credential.trim())?;
    cookie.set_sensitive(true);
    headers.insert(COOKIE, cookie);
    if json_body {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    Ok(headers)
}

fn bearer_headers(credential: &str) -> Result<HeaderMap, RewardError> {
    validate_credential(credential, "access token")?;
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let mut auth = header_value(&format!("Bearer {}", credential.trim()))?;
    auth.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth);
    Ok(headers)
}

fn lobster_headers(credential: &str) -> Result<HeaderMap, RewardError> {
    let mut headers = bearer_headers(credential)?;
    headers.insert(USER_AGENT, HeaderValue::from_static("LobsterAI/2026.9.4"));
    headers.insert(
        "X-LobsterAI-Client-Version",
        HeaderValue::from_static(LOBSTER_CLIENT_VERSION),
    );
    Ok(headers)
}

async fn astudio_status(
    client: &reqwest::Client,
    credential: &str,
) -> Result<RewardStatus, RewardError> {
    let response = client
        .get(format!("{ASTUDIO_BASE}/points/balance"))
        .headers(astudio_headers(credential, false)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("AStudio 状态请求失败"))?;
    Ok(parse_astudio_status(
        &response_json(response, credential).await?,
    ))
}

async fn astudio_claim(
    client: &reqwest::Client,
    credential: &str,
) -> Result<RewardClaim, RewardError> {
    let before = client
        .get(format!("{ASTUDIO_BASE}/points/balance"))
        .headers(astudio_headers(credential, false)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("AStudio 余额请求失败"))?;
    let before = response_json(before, credential).await?;
    let response = client
        .post(format!("{ASTUDIO_BASE}/tenant-app/init-web"))
        .headers(astudio_headers(credential, true)?)
        .json(&json!({}))
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("AStudio 签到请求失败"))?;
    let result = response_json(response, credential).await?;
    let after = client
        .get(format!("{ASTUDIO_BASE}/points/balance"))
        .headers(astudio_headers(credential, false)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("AStudio 余额请求失败"))?;
    let after = response_json(after, credential).await?;
    Ok(parse_astudio_claim(&before, &after, &result))
}

async fn dumate_status(
    client: &reqwest::Client,
    credential: &str,
) -> Result<RewardStatus, RewardError> {
    let bonus = client
        .get(format!("{DUMATE_BASE}/api/dumate/points/loginBonusInfo"))
        .headers(dumate_headers(credential, false)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("DuMate 状态请求失败"))?;
    let bonus = response_json(bonus, credential).await?;
    let mut status = parse_dumate_status(&bonus);
    let wallet = client.get(format!("{DUMATE_BASE}/api/dumate/points/quota_overview?clientType=desktop&timezone=Asia%2FShanghai")).headers(dumate_headers(credential, false)?).timeout(HTTP_TIMEOUT).send().await.map_err(|_| RewardError::invalid("DuMate 余额请求失败"))?;
    let wallet = response_json(wallet, credential).await?;
    status.wallet = wallet_whitelist(
        &payload(&wallet),
        &[
            "totalPoints",
            "usedPoints",
            "left",
            "monthPoints",
            "month_points",
        ],
    );
    Ok(status)
}

async fn dumate_claim(
    client: &reqwest::Client,
    credential: &str,
) -> Result<RewardClaim, RewardError> {
    let status = dumate_status(client, credential).await?;
    if status.already_completed || !status.claimable {
        return Ok(RewardClaim {
            profile: RewardProfileId::DuMate.to_string(),
            success: status.already_completed,
            already_completed: status.already_completed,
            claimable: status.claimable,
            reward: status.reward,
            wallet: status.wallet,
            expires_at: status.expires_at,
            message: Some(
                if status.already_completed {
                    "今日已领取"
                } else {
                    "当前不可领取"
                }
                .into(),
            ),
        });
    }
    let response = client
        .post(format!("{DUMATE_BASE}/api/dumate/points/loginBonus"))
        .headers(dumate_headers(credential, true)?)
        .json(&json!({}))
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("DuMate 签到请求失败"))?;
    let result = response_json(response, credential).await?;
    let mut claim = parse_dumate_claim(&status, &result);
    if claim.wallet.is_none() {
        claim.wallet = dumate_status(client, credential)
            .await
            .ok()
            .and_then(|s| s.wallet);
    }
    Ok(claim)
}

async fn minimax_status(
    client: &reqwest::Client,
    credential: &str,
) -> Result<RewardStatus, RewardError> {
    let url =
        format!("{MINIMAX_BASE}/minimax-cloud/api/v1/signin/status?timezone_id={MINIMAX_TIMEZONE}");
    let response = client
        .get(url)
        .headers(bearer_headers(credential)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("MiniMax Code 状态请求失败"))?;
    Ok(parse_minimax_status(
        &response_json(response, credential).await?,
    ))
}

async fn minimax_claim(
    client: &reqwest::Client,
    credential: &str,
) -> Result<RewardClaim, RewardError> {
    let status = minimax_status(client, credential).await?;
    if status.already_completed || !status.claimable {
        return Ok(RewardClaim {
            profile: RewardProfileId::MiniMaxCode.to_string(),
            success: status.already_completed,
            already_completed: status.already_completed,
            claimable: status.claimable,
            reward: status.reward,
            wallet: status.wallet,
            expires_at: status.expires_at,
            message: Some(
                if status.already_completed {
                    "今天已签到"
                } else {
                    "今天暂不可领"
                }
                .into(),
            ),
        });
    }
    let url =
        format!("{MINIMAX_BASE}/minimax-cloud/api/v1/signin/claim?timezone_id={MINIMAX_TIMEZONE}");
    let response = client
        .post(url)
        .headers(bearer_headers(credential)?)
        .json(&json!({}))
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("MiniMax Code 签到请求失败"))?;
    Ok(parse_minimax_claim(
        &status,
        &response_json(response, credential).await?,
    ))
}

async fn lobster_state(
    client: &reqwest::Client,
    credential: &str,
) -> Result<LobsterState, RewardError> {
    let query =
        "placement=desktop_sidebar&clientVersion=2026.9.4&containerApiVersion=2&platform=win32";
    let slot_response = client
        .get(format!("{LOBSTER_BASE}/api/client-activities/slot?{query}"))
        .headers(lobster_headers(credential)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("LobsterAI 活动查询失败"))?;
    let slot = response_json(slot_response, credential).await?;
    let wallet = client
        .get(format!("{LOBSTER_BASE}/api/user/profile-summary"))
        .headers(lobster_headers(credential)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("LobsterAI 额度查询失败"))?;
    let wallet = parse_lobster_wallet(&response_json(wallet, credential).await?);
    let Some((code, revision)) = parse_lobster_slot(&slot) else {
        return Ok(LobsterState {
            status: RewardStatus {
                profile: RewardProfileId::LobsterAI.to_string(),
                claimable: false,
                already_completed: false,
                reward: None,
                wallet,
                expires_at: None,
                message: Some("当前没有可用活动".into()),
            },
            code: String::new(),
            revision: 0,
        });
    };
    let context_url = format!(
        "{LOBSTER_BASE}/api/client-activities/{}/context?configRevision={revision}",
        path_segment(&code)
    );
    let context_response = client
        .get(context_url)
        .headers(lobster_headers(credential)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|_| RewardError::invalid("LobsterAI 活动状态请求失败"))?;
    let context = response_json(context_response, credential).await?;
    Ok(parse_lobster_context(&slot, &context, wallet))
}

async fn lobster_status(
    client: &reqwest::Client,
    credential: &str,
) -> Result<RewardStatus, RewardError> {
    Ok(lobster_state(client, credential).await?.status)
}

fn random_idempotency_key() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u128);
        bytes.copy_from_slice(&now.to_be_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}", bytes[0],bytes[1],bytes[2],bytes[3],bytes[4],bytes[5],bytes[6],bytes[7],bytes[8],bytes[9],bytes[10],bytes[11],bytes[12],bytes[13],bytes[14],bytes[15])
}

async fn lobster_claim(
    client: &reqwest::Client,
    credential: &str,
) -> Result<RewardClaim, RewardError> {
    let state = lobster_state(client, credential).await?;
    if state.status.already_completed || !state.status.claimable {
        return Ok(RewardClaim {
            profile: RewardProfileId::LobsterAI.to_string(),
            success: state.status.already_completed,
            already_completed: state.status.already_completed,
            claimable: state.status.claimable,
            reward: state.status.reward,
            wallet: state.status.wallet,
            expires_at: state.status.expires_at,
            message: Some(
                if state.status.already_completed {
                    "今日已领取"
                } else {
                    "当前不可领取"
                }
                .into(),
            ),
        });
    }
    let url = format!(
        "{LOBSTER_BASE}/api/client-activities/{}/actions/check_in",
        path_segment(&state.code)
    );
    let response = client.post(url).headers(lobster_headers(credential)?).json(&json!({ "configRevision": state.revision, "idempotencyKey": random_idempotency_key(), "payload": {} })).timeout(HTTP_TIMEOUT).send().await.map_err(|_| RewardError::invalid("LobsterAI 签到请求失败"))?;
    let result = response_json(response, credential).await?;
    let wallet = match client
        .get(format!("{LOBSTER_BASE}/api/user/profile-summary"))
        .headers(lobster_headers(credential)?)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response_json(response, credential)
            .await
            .ok()
            .and_then(|v| parse_lobster_wallet(&v)),
        Err(_) => None,
    };
    let fallback_wallet = state.status.wallet.clone();
    Ok(parse_lobster_claim(
        &state.status,
        &result,
        wallet.or(fallback_wallet),
    ))
}

/// 查询奖励状态。传入 `None` 时内部创建默认 reqwest client。
pub async fn status(
    profile: RewardProfileId,
    credential: &str,
    client: Option<&reqwest::Client>,
) -> Result<RewardStatus, RewardError> {
    let owned = client_ref(client);
    match profile {
        RewardProfileId::AStudio => astudio_status(&owned, credential).await,
        RewardProfileId::DuMate => dumate_status(&owned, credential).await,
        RewardProfileId::MiniMaxCode => minimax_status(&owned, credential).await,
        RewardProfileId::LobsterAI => lobster_status(&owned, credential).await,
    }
}

/// 执行幂等领取。每个适配器在写请求前都先检查今日状态。
pub async fn claim(
    profile: RewardProfileId,
    credential: &str,
    client: Option<&reqwest::Client>,
) -> Result<RewardClaim, RewardError> {
    let owned = client_ref(client);
    match profile {
        RewardProfileId::AStudio => astudio_claim(&owned, credential).await,
        RewardProfileId::DuMate => dumate_claim(&owned, credential).await,
        RewardProfileId::MiniMaxCode => minimax_claim(&owned, credential).await,
        RewardProfileId::LobsterAI => lobster_claim(&owned, credential).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_catalog_has_four_redacted_entries() {
        let list = list_profiles();
        assert_eq!(list.len(), 4);
        let serialized = serde_json::to_string(&list).unwrap();
        assert!(!serialized.contains("Cookie"));
        assert!(!serialized.contains("Bearer"));
        assert!(serialized.contains("minimax-code"));
    }

    #[test]
    fn astudio_delta_separates_spark_points() {
        let before = json!({"data":{"totalAmount":100,"sparkTotalAmount":5000}});
        let after = json!({"data":{"totalAmount":200,"totalBalance":180,"sparkTotalAmount":7000,"sparkTotalBalance":6900}});
        let claim = parse_astudio_claim(&before, &after, &json!({"code":0}));
        assert!(claim.success);
        assert_eq!(claim.reward.as_ref().unwrap()["points"], json!(100.0));
        assert_eq!(claim.reward.as_ref().unwrap()["sparkPoints"], json!(2000.0));
    }

    #[test]
    fn dumate_already_issued_is_not_claimable() {
        let status = parse_dumate_status(&json!({"result":{"hasIssued":true,"monthPoints":500}}));
        assert!(status.already_completed);
        assert!(!status.claimable);
        let claim = parse_dumate_claim(&status, &json!({"code":0}));
        assert!(claim.already_completed);
        assert!(claim.success);
    }

    #[test]
    fn minimax_claim_panel_statuses_are_mapped() {
        let available = parse_minimax_status(
            &json!({"data":{"days":[{"is_today":true,"status":2,"points":300}]}}),
        );
        assert!(available.claimable);
        assert_eq!(available.reward.unwrap()["points"], json!(300.0));
        let claimed = parse_minimax_status(
            &json!({"data":{"days":[{"is_today":true,"status":3,"points":300}]}}),
        );
        assert!(claimed.already_completed);
        assert!(!claimed.claimable);
    }

    #[test]
    fn lobster_dynamic_activity_code_and_claimed_state() {
        let slot = json!({"data":{"slotState":"available","activity":{"activityCode":"campaign-2026","configRevision":7}}});
        let context = json!({"data":{"state":{"claimedToday":false},"actions":["check_in"],"rewardCredits":100}});
        let state =
            parse_lobster_context(&slot, &context, Some(json!({"totalCreditsRemaining":1200})));
        assert_eq!(state.code, "campaign-2026");
        assert!(state.status.claimable);
        assert_eq!(state.status.reward.unwrap()["credits"], json!(100.0));
        let claimed = parse_lobster_context(
            &slot,
            &json!({"data":{"state":{"claimedToday":true},"actions":["check_in"]}}),
            None,
        );
        assert!(claimed.status.already_completed);
        assert!(!claimed.status.claimable);
    }

    #[test]
    fn credential_validation_rejects_newline_and_error_never_echoes_secret() {
        assert!(validate_credential("token\nforged", "token").is_err());
        let secret = "cookie-secret-123";
        let error = response_message(&json!({"message":secret}), secret);
        assert!(!error.contains(secret));
        let encoded = serde_json::to_string(&json!({"message":error})).unwrap();
        assert!(!encoded.contains(secret));
    }

    #[test]
    fn nested_response_codes_and_messages_are_detected() {
        assert_eq!(response_code(&json!({"data":{"code":80000}})), Some(80000));
        assert_eq!(
            response_code(&json!({"result":{"base_resp":{"status_code":17}}})),
            Some(17)
        );
        assert_eq!(
            response_message(&json!({"data":{"msg":"登录失效"}}), ""),
            "登录失效"
        );
    }

    #[test]
    fn path_segment_uses_percent_encoding_not_form_plus() {
        assert_eq!(
            path_segment("campaign 2026+/汉"),
            "campaign%202026%2B%2F%E6%B1%89"
        );
    }

    #[test]
    fn random_idempotency_key_is_uuid_v4_shaped() {
        let key = random_idempotency_key();
        assert_eq!(key.len(), 36);
        assert_eq!(&key[14..15], "4");
        assert!(matches!(&key[19..20], "8" | "9" | "a" | "b"));
    }
}
