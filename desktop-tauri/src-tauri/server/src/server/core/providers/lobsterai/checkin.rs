//! LobsterAI 活动奖励：动态 slot → context → check_in。
//!
//! 活动配置不能写死：`activityCode` 与 `configRevision` 每次都从 slot/context
//! 返回值读取。无活动、无资格或今日已领取返回中性结果；网络、鉴权和上游业务
//! 错误保持失败结果。领取响应只有明确的成功/完成状态或正数奖励时才算成功。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

use super::auth;
use super::credentials::{Credentials, CLIENT_VERSION};

pub const REWARD_KIND: &str = "activity_reward";

#[derive(Clone, Debug, PartialEq)]
pub struct CheckinResult {
    pub success: bool,
    pub already_completed: bool,
    pub claimable: bool,
    pub neutral: bool,
    pub reward_credits: Option<f64>,
    pub wallet: Option<Value>,
    pub expires_at: Option<Value>,
    pub status: &'static str,
    pub message: String,
    unauthorized: bool,
}

impl CheckinResult {
    pub fn to_value(&self) -> Value {
        json!({
            "success": self.success,
            "alreadyCompleted": self.already_completed,
            "claimable": self.claimable,
            "neutral": self.neutral,
            "rewardKind": REWARD_KIND,
            "reward": self.reward_credits.map(Value::from).unwrap_or(Value::Null),
            "wallet": self.wallet.clone().unwrap_or(Value::Null),
            "expiresAt": self.expires_at.clone().unwrap_or(Value::Null),
            "status": self.status,
            "msg": self.message,
        })
    }
}

#[derive(Clone, Debug)]
struct CheckinError {
    message: String,
    unauthorized: bool,
}

impl CheckinError {
    fn from_gateway(error: GatewayError) -> Self {
        Self {
            unauthorized: error.status_code == 401,
            message: error.message,
        }
    }
}

fn neutral(message: impl Into<String>, already_completed: bool) -> CheckinResult {
    CheckinResult {
        success: false,
        already_completed,
        claimable: false,
        neutral: true,
        reward_credits: None,
        wallet: None,
        expires_at: None,
        status: if already_completed {
            "already_claimed"
        } else {
            "unavailable"
        },
        message: message.into(),
        unauthorized: false,
    }
}

fn failed(error: CheckinError) -> CheckinResult {
    CheckinResult {
        success: false,
        already_completed: false,
        claimable: false,
        neutral: false,
        reward_credits: None,
        wallet: None,
        expires_at: None,
        status: "error",
        message: error.message,
        unauthorized: error.unauthorized,
    }
}

async fn get_json(
    url: &str,
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, CheckinError> {
    let response = auth::request("GET", url, None, Some(&credentials.access_token), proxy)
        .await
        .map_err(CheckinError::from_gateway)?;
    auth::payload(response, "活动查询").map_err(CheckinError::from_gateway)
}

async fn post_json(
    url: &str,
    body: &Value,
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, CheckinError> {
    let response = auth::request(
        "POST",
        url,
        Some(body),
        Some(&credentials.access_token),
        proxy,
    )
    .await
    .map_err(CheckinError::from_gateway)?;
    auth::payload(response, "活动领取").map_err(CheckinError::from_gateway)
}

pub async fn daily_checkin(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> CheckinResult {
    let base = auth::base_url();
    let slot_url = format!(
        "{base}/api/client-activities/slot?placement=desktop_sidebar&clientVersion={CLIENT_VERSION}&containerApiVersion=2&platform=win32"
    );
    let slot = match get_json(&slot_url, credentials, proxy).await {
        Ok(value) => value,
        Err(error) => return failed(error),
    };
    if !slot_is_available(&slot) {
        return neutral("当前无可领取活动", false);
    }
    if explicitly_inactive(&slot) {
        return neutral("当前活动未启用", false);
    }
    let Some(activity) = activity_object(&slot) else {
        return neutral("当前无可领取活动", false);
    };
    let code = text(activity, &["activityCode", "activity_code", "code"]);
    let revision = number(activity, &["configRevision", "config_revision"])
        .or_else(|| value_number(&slot, &["configRevision", "config_revision"]))
        .unwrap_or(0);
    if code.is_empty() || revision <= 0 {
        return neutral("活动配置暂不可用", false);
    }

    let encoded_code = encode_path_segment(&code);
    let context_url =
        format!("{base}/api/client-activities/{encoded_code}/context?configRevision={revision}");
    let context = match get_json(&context_url, credentials, proxy).await {
        Ok(value) => value,
        Err(error) => return failed(error),
    };
    if explicitly_inactive(&context) {
        return neutral("当前活动未启用", false);
    }
    let claimed_today = bool_at(&context, &["/state/claimedToday", "/claimedToday"]);
    let completed = bool_at(&context, &["/state/completed", "/completed"]);
    if claimed_today || completed {
        return neutral("今日已领取", true);
    }
    if explicitly_ineligible(&context) {
        return neutral("当前账号无资格领取活动奖励", false);
    }
    if !has_checkin_action(&context) {
        return neutral("当前账号暂不可领取活动奖励", false);
    }

    let action_url = format!("{base}/api/client-activities/{encoded_code}/actions/check_in");
    let body = json!({
        "configRevision": revision,
        "idempotencyKey": crate::server::core::upstream::request::new_request_id(),
        "payload": {},
    });
    let result = match post_json(&action_url, &body, credentials, proxy).await {
        Ok(value) => value,
        Err(error) => return failed(error),
    };
    let result = result
        .get("result")
        .or_else(|| result.get("data"))
        .unwrap_or(&result);
    let reward_credits = [
        "rewardCredits",
        "creditsGranted",
        "grantedCredits",
        "credits",
    ]
    .iter()
    .find_map(|key| positive_number(result.get(*key)))
    .or_else(|| {
        result.get("reward").and_then(|reward| {
            positive_number(reward.get("credits")).or_else(|| positive_number(reward.get("amount")))
        })
    });
    let already_completed = bool_at(
        result,
        &["/alreadyCompleted", "/already_claimed", "/claimedToday"],
    ) || status_is(result, &["already_claimed", "already_completed"]);
    let explicit_success = bool_at(result, &["/success", "/ok", "/claimed", "/completed"])
        || status_is(result, &["success", "claimed", "completed"])
        || reward_credits.is_some();
    let wallet = result.get("wallet").cloned();
    let expires_at = result
        .get("expiresAt")
        .or_else(|| result.get("expires_at"))
        .cloned();
    if already_completed {
        return CheckinResult {
            success: false,
            already_completed: true,
            claimable: false,
            neutral: false,
            reward_credits,
            wallet,
            expires_at,
            status: "already_claimed",
            message: "今日已领取".to_string(),
            unauthorized: false,
        };
    }
    if !explicit_success {
        return failed(CheckinError {
            message: "活动领取响应未确认成功".to_string(),
            unauthorized: false,
        });
    }
    CheckinResult {
        success: true,
        already_completed: false,
        claimable: false,
        neutral: false,
        reward_credits,
        wallet,
        expires_at,
        status: "claimed",
        message: "活动奖励领取成功".to_string(),
        unauthorized: false,
    }
}

/// 供统一签到调度器调用的账号级入口。管理接口收到 401 时仅刷新一次并重试。
pub async fn claim_activity_reward(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let (id, _, _) = super::refresh::snapshot(store, account_id)?;
    let credentials = super::refresh::ensure_fresh(store, &id, false).await?;
    let (_, session, _) = super::refresh::snapshot(store, &id)?;
    let proxy = super::auth::account_proxy(&session)?;
    let mut result = daily_checkin(&credentials, proxy.as_ref()).await;
    if result.unauthorized && credentials.can_refresh() {
        let fresh = super::refresh::ensure_fresh(store, &id, true).await?;
        let (_, session, _) = super::refresh::snapshot(store, &id)?;
        let proxy = super::auth::account_proxy(&session)?;
        result = daily_checkin(&fresh, proxy.as_ref()).await;
    }
    Ok(result.to_value())
}

fn slot_is_available(slot: &Value) -> bool {
    slot.get("slotState")
        .or_else(|| slot.get("slot_state"))
        .and_then(Value::as_str)
        .is_some_and(|state| matches!(state, "available" | "active"))
        || bool_at(slot, &["/active", "/isActive"])
}

fn activity_object(slot: &Value) -> Option<&serde_json::Map<String, Value>> {
    slot.get("activity")
        .or_else(|| slot.pointer("/slot/activity"))
        .or_else(|| slot.pointer("/data/activity"))
        .and_then(Value::as_object)
}

fn explicitly_ineligible(context: &Value) -> bool {
    [
        "/eligible",
        "/qualified",
        "/isEligible",
        "/state/eligible",
        "/state/qualified",
        "/state/isEligible",
    ]
    .iter()
    .filter_map(|path| context.pointer(path))
    .any(|value| matches!(value, Value::Bool(false)) || value.as_str() == Some("false"))
}

fn explicitly_inactive(value: &Value) -> bool {
    [
        "/active",
        "/isActive",
        "/activity/active",
        "/activity/isActive",
        "/state/active",
        "/state/isActive",
    ]
    .iter()
    .filter_map(|path| value.pointer(path))
    .any(|flag| {
        flag.as_bool() == Some(false)
            || flag.as_str().is_some_and(|text| {
                matches!(
                    text.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "inactive" | "ended" | "closed"
                )
            })
    })
}

fn has_checkin_action(context: &Value) -> bool {
    let Some(actions) = context
        .get("actions")
        .or_else(|| context.pointer("/state/actions"))
    else {
        return false;
    };
    match actions {
        Value::Array(actions) => actions.iter().any(action_is_checkin),
        Value::Object(actions) => {
            actions.get("check_in").is_some_and(|value| {
                !matches!(value, Value::Bool(false)) && value.as_str() != Some("false")
            }) || action_object_is_checkin(actions)
        }
        _ => action_is_checkin(actions),
    }
}

fn action_is_checkin(action: &Value) -> bool {
    if action.as_str() == Some("check_in") {
        return true;
    }
    action.as_object().is_some_and(action_object_is_checkin)
}

fn action_object_is_checkin(action: &serde_json::Map<String, Value>) -> bool {
    ["type", "action", "name", "code", "id"]
        .iter()
        .any(|key| action.get(*key).and_then(Value::as_str) == Some("check_in"))
}

fn bool_at(value: &Value, paths: &[&str]) -> bool {
    paths
        .iter()
        .filter_map(|path| value.pointer(path))
        .any(|value| {
            value.as_bool().unwrap_or_else(|| {
                value.as_str().is_some_and(|text| {
                    matches!(
                        text.trim().to_ascii_lowercase().as_str(),
                        "true" | "1" | "yes"
                    )
                })
            })
        })
}

fn status_is(value: &Value, statuses: &[&str]) -> bool {
    value
        .get("status")
        .or_else(|| value.get("state"))
        .and_then(Value::as_str)
        .is_some_and(|status| {
            statuses
                .iter()
                .any(|expected| status.eq_ignore_ascii_case(expected))
        })
}

fn text(object: &serde_json::Map<String, Value>, keys: &[&str]) -> String {
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

fn number(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<i64> {
    value_number(&Value::Object(object.clone()), keys)
}

fn value_number(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|number| number as i64))
                .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
        })
    })
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let character = byte as char;
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '~') {
            encoded.push(character);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn positive_number(value: Option<&Value>) -> Option<f64> {
    let number = value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
    })?;
    (number.is_finite() && number > 0.0).then_some(number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slot_and_context_helpers_accept_dynamic_shapes() {
        let slot = json!({
            "slotState":"available",
            "activity":{"activityCode":"daily-v2","configRevision":"7"}
        });
        let activity = activity_object(&slot).expect("activity");
        assert_eq!(text(activity, &["activityCode"]), "daily-v2");
        assert_eq!(number(activity, &["configRevision"]), Some(7));
        let context = json!({"eligible":true,"actions":[{"type":"check_in"}]});
        assert!(has_checkin_action(&context));
    }

    #[test]
    fn empty_or_ineligible_context_is_neutral() {
        assert!(!has_checkin_action(&json!({"actions":[]})));
        assert!(explicitly_ineligible(
            &json!({"state":{"isEligible":false}})
        ));
        assert!(slot_is_available(&json!({"active":true})));
    }

    #[test]
    fn activity_code_is_encoded_as_one_path_segment() {
        assert_eq!(
            encode_path_segment("daily/v2 试验"),
            "daily%2Fv2%20%E8%AF%95%E9%AA%8C"
        );
    }

    #[test]
    fn successful_result_requires_explicit_success_or_reward() {
        let result = json!({"message":"accepted"});
        assert!(!bool_at(&result, &["/success", "/completed"]));
        assert_eq!(positive_number(result.get("credits")), None);
        let claimed = json!({"status":"claimed","creditsGranted":"100"});
        assert!(status_is(&claimed, &["claimed"]));
        assert_eq!(positive_number(claimed.get("creditsGranted")), Some(100.0));
    }

    #[test]
    fn result_serializes_reward_kind_and_status() {
        let result = CheckinResult {
            success: true,
            already_completed: false,
            claimable: false,
            neutral: false,
            reward_credits: Some(100.0),
            wallet: None,
            expires_at: None,
            status: "claimed",
            message: "ok".to_string(),
            unauthorized: false,
        };
        assert_eq!(result.to_value()["rewardKind"], REWARD_KIND);
        assert_eq!(result.to_value()["status"], "claimed");
    }
}
