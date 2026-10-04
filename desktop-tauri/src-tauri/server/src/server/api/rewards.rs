//! 自定义提供商奖励签到 API。
//!
//! 奖励凭证与模型转发 `apiKey` 是两条独立管道：本模块只读取
//! `custom_reward_credential_by_id` 的奖励凭证快照，任何响应都只返回 profile、
//! 状态和凭证尾号，绝不回传 cookie / access token。
//!
//!   GET  /api/reward-providers       profile 能力清单
//!   POST /api/rewards/configure      配置或清除账号奖励凭证
//!   GET  /api/rewards/status?id=...  查询实时奖励状态
//!   POST /api/rewards/claim          先查状态，再幂等领取

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStoreError;
use crate::server::core::reward_profiles::{self, RewardClaim, RewardError, RewardStatus};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::ServerState;

#[derive(Debug, Deserialize)]
pub struct RewardQuery {
    pub id: Option<String>,
}

/// GET /api/reward-providers
pub async fn list_reward_providers(State(_state): State<ServerState>) -> Response {
    // `RewardProfileInfo` contains endpoint and credential *kind* metadata only;
    // it has no account credential fields by construction.
    match serde_json::to_value(reward_profiles::list_profiles()) {
        Ok(profiles) => ok_json(json!({ "profiles": profiles })),
        Err(error) => errors::management_error(500, format!("奖励提供商清单序列化失败: {error}")),
    }
}

/// POST /api/rewards/configure
///
/// body: `{ "accountId": "...", "rewardCredential": "..." }`.
/// 空字符串表示清除既有奖励凭证。
pub async fn configure_reward(State(state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let account_id = match object.get("accountId") {
        Some(Value::String(value)) => value.trim(),
        Some(_) => return errors::management_error(400, "accountId 必须是字符串"),
        None => "",
    };
    if account_id.is_empty() {
        return errors::management_error(400, "缺少账号 id");
    }
    let credential = match object.get("rewardCredential") {
        Some(Value::String(value)) => value.trim(),
        Some(_) => return errors::management_error(400, "rewardCredential 必须是字符串"),
        None => return errors::management_error(400, "缺少 rewardCredential"),
    };
    match state
        .store()
        .set_custom_reward_credential(account_id, credential)
    {
        Ok(_) => {
            let configured = state.store().custom_reward_credential_by_id(account_id);
            let (profile, tail) = configured
                .as_ref()
                .map(|value| {
                    (
                        Some(value.profile_id.clone()),
                        Some(credential_tail(&value.credential)),
                    )
                })
                .unwrap_or((None, None));
            ok_json(json!({
                "accountId": account_id,
                "rewardProfile": profile,
                "rewardCredentialConfigured": configured.is_some(),
                "rewardCredentialTail": tail,
            }))
        }
        Err(error) => account_error(error),
    }
}

/// GET /api/rewards/status?id=...
pub async fn reward_status(
    State(state): State<ServerState>,
    query: Query<RewardQuery>,
) -> Response {
    let account_id = query
        .id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    if account_id.is_empty() {
        return errors::management_error(400, "缺少账号 id");
    }
    let Some(credential) = state.store().custom_reward_credential_by_id(account_id) else {
        return errors::management_error(404, "账号不存在或未配置奖励凭证");
    };
    let profile = match reward_profiles::parse_profile(&credential.profile_id) {
        Ok(profile) => profile,
        Err(error) => return reward_error(error, Some(&credential.credential)),
    };
    match reward_profiles::status(profile, &credential.credential, None).await {
        Ok(status) => ok_json(reward_status_response(
            account_id,
            &credential.profile_id,
            &status,
        )),
        Err(error) => reward_error(error, Some(&credential.credential)),
    }
}

/// POST /api/rewards/claim
pub async fn claim_reward(State(state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let account_id = match object.get("accountId").or_else(|| object.get("id")) {
        Some(Value::String(value)) => value.trim(),
        Some(_) => return errors::management_error(400, "accountId 必须是字符串"),
        None => "",
    };
    if account_id.is_empty() {
        return errors::management_error(400, "缺少账号 id");
    }
    let Some(credential) = state.store().custom_reward_credential_by_id(account_id) else {
        return errors::management_error(404, "账号不存在或未配置奖励凭证");
    };
    let profile = match reward_profiles::parse_profile(&credential.profile_id) {
        Ok(profile) => profile,
        Err(error) => return reward_error(error, Some(&credential.credential)),
    };
    // Always read status first.  Besides avoiding duplicate claims this lets an
    // inactive/dynamic activity return a useful no-op result without posting.
    let status = match reward_profiles::status(profile.clone(), &credential.credential, None).await
    {
        Ok(status) => status,
        Err(error) => return reward_error(error, Some(&credential.credential)),
    };
    if status.already_completed || !status.claimable {
        return ok_json(json!({
            "accountId": account_id,
            "profile": credential.profile_id,
            "success": false,
            "alreadyCompleted": status.already_completed,
            "claimable": status.claimable,
            "reward": status.reward,
            "wallet": status.wallet,
            "expiresAt": status.expires_at,
            "msg": status.message.unwrap_or_else(|| {
                if status.already_completed { "今日已领取".to_string() } else { "当前没有可领取的奖励".to_string() }
            }),
        }));
    }
    match reward_profiles::claim(profile, &credential.credential, None).await {
        Ok(claim) => ok_json(reward_claim_response(
            account_id,
            &credential.profile_id,
            &claim,
        )),
        Err(error) => reward_error(error, Some(&credential.credential)),
    }
}

fn body_object(body: &Bytes) -> Result<Map<String, Value>, Response> {
    let value = parse_body(body).map_err(|error| errors::management_error(400, error.message))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| errors::management_error(400, "请求体必须是 JSON 对象"))
}

fn account_error(error: AccountStoreError) -> Response {
    errors::management_error(error.status_code, sanitize_error(&error.message, None))
}

fn reward_error(error: RewardError, credential: Option<&str>) -> Response {
    let status = error
        .status_code
        .map(i32::from)
        .filter(|value| (400..600).contains(value))
        .unwrap_or(502);
    errors::management_error(status, sanitize_error(&error.message, credential))
}

fn sanitize_error(message: &str, credential: Option<&str>) -> String {
    let mut result = message.to_string();
    if let Some(credential) = credential.filter(|value| !value.is_empty()) {
        result = result.replace(credential, "[REDACTED]");
    }
    // Keep upstream diagnostics useful while preventing common cookie/token
    // labels from being echoed by a malformed upstream response.
    for key in [
        "apiKey",
        "api_key",
        "accessToken",
        "access_token",
        "cookie",
        "Cookie",
    ] {
        result = result.replace(key, "credential");
    }
    result
}

fn credential_tail(credential: &str) -> String {
    credential
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn reward_status_response(account_id: &str, profile: &str, status: &RewardStatus) -> Value {
    json!({
        "accountId": account_id,
        "profile": profile,
        "claimable": status.claimable,
        "alreadyCompleted": status.already_completed,
        "reward": status.reward.clone(),
        "wallet": status.wallet.clone(),
        "expiresAt": status.expires_at.clone(),
        "msg": status.message.clone(),
    })
}

fn reward_claim_response(account_id: &str, profile: &str, claim: &RewardClaim) -> Value {
    json!({
        "accountId": account_id,
        "profile": profile,
        "success": claim.success,
        "alreadyCompleted": claim.already_completed,
        "claimable": claim.claimable,
        "reward": claim.reward.clone(),
        "wallet": claim.wallet.clone(),
        "expiresAt": claim.expires_at.clone(),
        "msg": claim.message.clone(),
        "rawSummary": {
            "profile": claim.profile,
            "claimable": claim.claimable,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::{credential_tail, sanitize_error};

    #[test]
    fn credential_tail_only_keeps_last_four_chars() {
        assert_eq!(credential_tail("secret-token"), "oken");
        assert_eq!(credential_tail("abc"), "abc");
        assert_eq!(credential_tail(""), "");
    }

    #[test]
    fn error_sanitizer_removes_credential_and_labels() {
        assert_eq!(
            sanitize_error(
                "cookie=secret-token apiKey=secret-token",
                Some("secret-token")
            ),
            "credential=[REDACTED] credential=[REDACTED]"
        );
    }
}
