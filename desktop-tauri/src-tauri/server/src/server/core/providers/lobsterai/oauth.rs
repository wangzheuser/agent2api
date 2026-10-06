//! LobsterAI 网页登录：官方 portal 授权码、一次性 state 与凭证换取。
//!
//! 官方客户端/参考实现使用：
//! ```text
//! {portal}/portal#/login?source=electron&redirect_uri=<loopback>&state=<state>
//! POST /api/auth/exchange {
//!   authCode, firstKeyfrom, latestKeyfrom, uuid, version
//! }
//! ```
//! pending 上下文只放内存，回调成功或失败都会一次性消费，避免重复换码。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Map, Value};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::upstream::request::new_request_id;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::auth;
use super::credentials::{self, Credentials, CLIENT_VERSION};

pub const CALLBACK_PATH: &str = "/auth/callback";
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;
const MAX_STATE_LENGTH: usize = 512;
const MAX_CODE_LENGTH: usize = 8192;

static LOOPBACK_PORT: OnceLock<u16> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct PendingLogin {
    pub uuid: String,
    pub first_keyfrom: String,
    pub redirect_uri: String,
    created_at: i64,
}

fn pending_table() -> &'static Mutex<HashMap<String, PendingLogin>> {
    static TABLE: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 在服务启动时登记监听端口；登录地址由无参 ProviderAdapter 同步生成。
pub fn set_loopback_port(port: u16) {
    let _ = LOOPBACK_PORT.set(port);
}

pub fn loopback_base() -> Option<String> {
    LOOPBACK_PORT
        .get()
        .copied()
        .map(|port| format!("http://127.0.0.1:{port}"))
}

fn login_portal() -> String {
    std::env::var("LOBSTERAI_LOGIN_PORTAL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://lobsterai.youdao.com".to_string())
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn random_id() -> String {
    new_request_id()
}

/// 生成官方 portal 授权地址，并登记一次性换码上下文。
pub fn begin_login() -> Option<(String, String)> {
    let base = loopback_base()?;
    let redirect_uri = format!("{base}{CALLBACK_PATH}");
    let state = random_id();
    let pending = PendingLogin {
        uuid: random_id(),
        first_keyfrom: logging::now_ms().to_string(),
        redirect_uri: redirect_uri.clone(),
        created_at: logging::now_ms(),
    };
    let table = pending_table();
    let mut guard = table.lock().unwrap_or_else(|error| error.into_inner());
    sweep(&mut guard);
    guard.insert(state.clone(), pending);
    let url = format!(
        "{}/portal#/login?source=electron&redirect_uri={}&state={state}",
        login_portal(),
        urlencode(&redirect_uri),
    );
    Some((url, state))
}

pub fn take_pending(state: &str) -> Option<PendingLogin> {
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

/// 校验 portal 回调 URL 并提取授权码。
pub fn parse_callback_code(
    callback_url: &str,
    expected_state: &str,
) -> Result<String, GatewayError> {
    let url = url::Url::parse(callback_url.trim())
        .map_err(|_| GatewayError::with_status(400, "LobsterAI 登录回调地址无效"))?;
    if url.path() != CALLBACK_PATH {
        return Err(GatewayError::with_status(400, "LobsterAI 登录回调地址无效"));
    }
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code" || key == "authCode" || key == "authorization_code")
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default();
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default();
    if state.is_empty() || state != expected_state {
        return Err(GatewayError::with_status(
            400,
            "LobsterAI 登录回调 state 校验失败，请重新发起登录",
        ));
    }
    if code.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "LobsterAI 登录回调没有授权码",
        ));
    }
    if state.chars().count() > MAX_STATE_LENGTH || code.chars().count() > MAX_CODE_LENGTH {
        return Err(GatewayError::with_status(400, "LobsterAI 登录回调参数过长"));
    }
    Ok(code)
}

/// 官方客户端会在回调里附带 `return_to`，用于把浏览器带回 portal 成功页。
/// 只允许 Youdao 域名和本机回环地址，避免把 OAuth 回调变成开放重定向。
pub fn safe_return_to(value: &str) -> Option<String> {
    let value = value.trim();
    let url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let allowed = host == "youdao.com"
        || host.ends_with(".youdao.com")
        || host == "127.0.0.1"
        || host == "localhost";
    allowed.then(|| value.to_string())
}

/// 用一次性授权码换取 LobsterAI 凭证。`state` 在这里再次消费，防止绕过路由层重复换码。
pub async fn exchange_code(
    code: &str,
    state: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credentials, GatewayError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "缺少 LobsterAI 授权码"));
    }
    if code.chars().count() > MAX_CODE_LENGTH {
        return Err(GatewayError::with_status(400, "LobsterAI 授权码过长"));
    }
    let Some(pending) = take_pending(state) else {
        return Err(GatewayError::with_status(
            404,
            "LobsterAI 登录已取消或过期，请重新发起",
        ));
    };
    let body = json!({
        "authCode": code,
        "firstKeyfrom": pending.first_keyfrom,
        "latestKeyfrom": logging::now_ms().to_string(),
        "uuid": pending.uuid,
        "version": CLIENT_VERSION,
    });
    let response = auth::request(
        "POST",
        &format!("{}/api/auth/exchange", auth::base_url()),
        Some(&body),
        None,
        proxy,
    )
    .await?;
    let data = auth::payload(response, "网页登录换取凭证")?;
    let normalized = normalize_exchange_payload(&data, &pending);
    let credentials = Credentials::from_payload(&normalized)?;
    if credentials.refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "LobsterAI 换码响应缺少 refreshToken",
        ));
    }
    Ok(credentials)
}

fn normalize_exchange_payload(data: &Value, pending: &PendingLogin) -> Value {
    if data.get("auth").is_some() {
        return data.clone();
    }
    let user = data
        .get("user")
        .or_else(|| data.get("account"))
        .cloned()
        .unwrap_or_else(|| data.clone());
    let mut auth = Map::new();
    auth.insert(
        "accessToken".to_string(),
        Value::String(credentials::text(
            data,
            &["accessToken", "access_token", "token"],
        )),
    );
    auth.insert(
        "refreshToken".to_string(),
        Value::String(credentials::text(data, &["refreshToken", "refresh_token"])),
    );
    if let Some(value) = data.get("expiresAt").filter(|value| !value.is_null()) {
        auth.insert("expiresAt".to_string(), value.clone());
    } else if let Some(value) = data.get("expiresIn").filter(|value| !value.is_null()) {
        auth.insert("expiresIn".to_string(), value.clone());
    }
    auth.insert(
        "uuid".to_string(),
        Value::String(credentials::text(data, &["uuid"]).if_empty_or(&pending.uuid)),
    );
    auth.insert(
        "firstKeyfrom".to_string(),
        Value::String(
            credentials::text(data, &["firstKeyfrom"]).if_empty_or(&pending.first_keyfrom),
        ),
    );
    auth.insert(
        "latestKeyfrom".to_string(),
        Value::String(logging::now_ms().to_string()),
    );
    json!({
        "auth": Value::Object(auth),
        "account": user,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_url_contains_official_exchange_parameters() {
        set_loopback_port(30_65);
        let (url, state) = begin_login().expect("loopback");
        assert!(url.contains("/portal#/login?source=electron"));
        assert!(url.contains("redirect_uri=http://127.0.0.1:3065/auth/callback"));
        assert!(url.contains(&format!("state={state}")));
        assert!(take_pending(&state).is_some());
    }

    #[test]
    fn callback_state_is_bound_to_the_pending_login() {
        let error = parse_callback_code(
            "http://127.0.0.1:3065/auth/callback?code=c&state=wrong",
            "expected",
        )
        .expect_err("state mismatch");
        assert_eq!(error.status_code, 400);
    }

    #[test]
    fn return_to_is_limited_to_youdao_or_loopback() {
        assert!(safe_return_to("https://lobsterai.youdao.com/portal#/login").is_some());
        assert!(safe_return_to("http://127.0.0.1:3065/auth/callback").is_some());
        assert!(safe_return_to("https://example.invalid/steal").is_none());
        assert!(safe_return_to("javascript:alert(1)").is_none());
    }

    #[test]
    fn callback_parser_accepts_official_return_to_parameter() {
        let code = parse_callback_code(
            "http://127.0.0.1:3065/auth/callback?return_to=https%3A%2F%2Flobsterai.youdao.com%2Fportal%23%2Flogin&code=fixture-code&state=fixture-state",
            "fixture-state",
        )
        .expect("official callback");
        assert_eq!(code, "fixture-code");
    }
}
