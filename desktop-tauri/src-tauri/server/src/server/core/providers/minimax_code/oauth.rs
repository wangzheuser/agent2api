//! MiniMax Code OAuth 登录：state 绑定的授权地址、回调解析与换码。
//!
//! 官方 Code 客户端同时支持设备码和授权码形态。网关的网页登录任务使用
//! 标准授权码回调，refresh 仍严格使用官方 `mcode-public` 参数；手动设备码
//! 返回的 access/refresh token 也可直接粘贴到账号表单。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Map, Value};

use crate::server::core::upstream::request::new_request_id;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::auth;
use super::credentials::{self, Credentials};

pub const CALLBACK_PATH: &str = "/auth/minimax-callback";
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;
const MAX_STATE_LENGTH: usize = 512;
const MAX_CODE_LENGTH: usize = 8192;

static LOOPBACK_PORT: OnceLock<u16> = OnceLock::new();

#[derive(Clone, Debug)]
struct PendingLogin {
    redirect_uri: String,
    created_at: i64,
}

fn pending_table() -> &'static Mutex<HashMap<String, PendingLogin>> {
    static TABLE: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_loopback_port(port: u16) {
    let _ = LOOPBACK_PORT.set(port);
}

fn loopback_base() -> Option<String> {
    LOOPBACK_PORT
        .get()
        .copied()
        .map(|port| format!("http://127.0.0.1:{port}"))
}

fn oauth_base_url() -> String {
    std::env::var("MINIMAX_CODE_ACCOUNT_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| credentials::ACCOUNT_BASE.to_string())
}

/// 生成授权地址并登记一次性 state。
pub fn begin_login() -> Option<(String, String)> {
    let redirect_uri = format!("{}{CALLBACK_PATH}", loopback_base()?);
    let state = new_request_id();
    let pending = PendingLogin {
        redirect_uri: redirect_uri.clone(),
        created_at: logging::now_ms(),
    };
    let table = pending_table();
    let mut guard = table.lock().unwrap_or_else(|error| error.into_inner());
    sweep(&mut guard);
    guard.insert(state.clone(), pending);
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("response_type", "code");
    query.append_pair("client_id", credentials::CLIENT_ID);
    query.append_pair("scope", credentials::OAUTH_SCOPE);
    query.append_pair("audience", credentials::OAUTH_AUDIENCE);
    query.append_pair("redirect_uri", &redirect_uri);
    query.append_pair("state", &state);
    Some((
        format!("{}/oauth2/authorize?{}", oauth_base_url(), query.finish()),
        state,
    ))
}

fn take_pending(state: &str) -> Option<PendingLogin> {
    let mut guard = pending_table()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    sweep(&mut guard);
    guard.remove(state.trim())
}

fn sweep(table: &mut HashMap<String, PendingLogin>) {
    let now = logging::now_ms();
    table.retain(|_, pending| now.saturating_sub(pending.created_at) <= PENDING_TTL_MS);
}

pub fn parse_callback_code(
    callback_url: &str,
    expected_state: &str,
) -> Result<String, GatewayError> {
    let url = url::Url::parse(callback_url.trim())
        .map_err(|_| GatewayError::with_status(400, "MiniMax Code 登录回调地址无效"))?;
    if url.path() != CALLBACK_PATH {
        return Err(GatewayError::with_status(
            400,
            "MiniMax Code 登录回调地址无效",
        ));
    }
    let params: HashMap<String, String> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if let Some(error) = params.get("error").filter(|value| !value.trim().is_empty()) {
        return Err(GatewayError::with_status(
            400,
            format!("授权被拒绝（{error}）"),
        ));
    }
    let state = params.get("state").map(String::as_str).unwrap_or("").trim();
    if state.is_empty() || state != expected_state.trim() {
        return Err(GatewayError::with_status(
            400,
            "MiniMax Code 登录回调 state 校验失败，请重新发起登录",
        ));
    }
    let code = params
        .get("code")
        .or_else(|| params.get("authorization_code"))
        .map(String::as_str)
        .unwrap_or("")
        .trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "MiniMax Code 登录回调没有授权码",
        ));
    }
    if state.chars().count() > MAX_STATE_LENGTH || code.chars().count() > MAX_CODE_LENGTH {
        return Err(GatewayError::with_status(
            400,
            "MiniMax Code 登录回调参数过长",
        ));
    }
    Ok(code.to_string())
}

pub async fn exchange_code(code: &str, state: &str) -> Result<Credentials, GatewayError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "缺少 MiniMax Code 授权码"));
    }
    let Some(pending) = take_pending(state) else {
        return Err(GatewayError::with_status(
            404,
            "MiniMax Code 登录已取消或过期，请重新发起",
        ));
    };
    let form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        ("client_id".to_string(), credentials::CLIENT_ID.to_string()),
        ("scope".to_string(), credentials::OAUTH_SCOPE.to_string()),
        (
            "audience".to_string(),
            credentials::OAUTH_AUDIENCE.to_string(),
        ),
        ("redirect_uri".to_string(), pending.redirect_uri),
    ];
    let response =
        auth::request_form(&format!("{}/oauth2/token", oauth_base_url()), &form, None).await?;
    let data = auth::payload(response, "网页登录换取凭证")?;
    let normalized = normalize_token_payload(&data);
    let credentials = Credentials::from_payload(&normalized)?;
    if credentials.refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "MiniMax Code 换码响应缺少 refreshToken",
        ));
    }
    Ok(credentials)
}

fn normalize_token_payload(data: &Value) -> Value {
    if data.get("auth").is_some() {
        return data.clone();
    }
    let mut auth = Map::new();
    for (target, source) in [
        ("accessToken", "access_token"),
        ("refreshToken", "refresh_token"),
        ("expiresIn", "expires_in"),
        ("refreshExpiresIn", "refresh_expires_in"),
    ] {
        if let Some(value) = data.get(source).filter(|value| !value.is_null()) {
            auth.insert(target.to_string(), value.clone());
        }
    }
    json!({
        "auth": Value::Object(auth),
        "account": data.get("account").or_else(|| data.get("user")).cloned().unwrap_or_else(|| data.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_state_is_required_and_bound() {
        let error = parse_callback_code(
            "http://127.0.0.1:3065/auth/minimax-callback?code=c&state=wrong",
            "expected",
        )
        .expect_err("state mismatch");
        assert_eq!(error.status_code, 400);
    }

    #[test]
    fn nested_and_flat_token_payloads_are_normalized() {
        let value = normalize_token_payload(&json!({"access_token":"a", "refresh_token":"r"}));
        let parsed = Credentials::from_payload(&value).expect("normalized");
        assert_eq!(parsed.refresh_token, "r");
    }
}
