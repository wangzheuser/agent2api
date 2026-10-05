//! 通过签到/活动任务领取额度的自定义提供商适配器。
//!
//! 该模块只处理奖励上游的 HTTP 协议。模型转发使用 custom provider 的
//! `apiKey`，奖励链路使用独立的字符串凭证；任何公开结构都只包含规范化的
//! 奖励与钱包字段，不回显 Cookie、Bearer token 或上游原始响应。

use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, USER_AGENT,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const ASTUDIO_BASE: &str = "https://agent.xfyun.cn/xingchen-studio";
const DUMATE_BASE: &str = "https://www.dumate.cn";

/// 预置 API 奖励 profile 的稳定 ID。这个 ID 会写入 custom provider 配置，不能随意改名。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum RewardProfileId {
    #[serde(rename = "astudio")]
    AStudio,
    #[serde(rename = "dumate")]
    DuMate,
}

impl RewardProfileId {
    pub const ALL: [Self; 2] = [Self::AStudio, Self::DuMate];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AStudio => "astudio",
            Self::DuMate => "dumate",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::AStudio => "AStudio",
            Self::DuMate => "DuMate",
        }
    }

    pub const fn credential_type(self) -> &'static str {
        match self {
            Self::AStudio | Self::DuMate => "cookie",
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

/// 返回预置 API 奖励 profile，不读取任何账号凭证。
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
            },
            claim_endpoint: match profile {
                RewardProfileId::AStudio => format!("{ASTUDIO_BASE}/tenant-app/init-web"),
                RewardProfileId::DuMate => format!("{DUMATE_BASE}/api/dumate/points/loginBonus"),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_catalog_contains_only_preset_api_rewards() {
        let list = list_profiles();
        assert_eq!(list.len(), 2);
        let serialized = serde_json::to_string(&list).unwrap();
        assert!(!serialized.contains("Cookie"));
        assert!(!serialized.contains("Bearer"));
        assert!(serialized.contains("astudio"));
        assert!(serialized.contains("dumate"));
        assert!(!serialized.contains("minimax-code"));
        assert!(!serialized.contains("lobsterai"));
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
    fn native_provider_ids_are_not_custom_reward_profiles() {
        assert!(parse_profile("astudio").is_ok());
        assert!(parse_profile("dumate").is_ok());
        assert!(parse_profile("minimax-code").is_err());
        assert!(parse_profile("lobsterai").is_err());
        assert!(!is_known_profile("minimax-code"));
        assert!(!is_known_profile("lobsterai"));
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
}
