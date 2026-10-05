//! LobsterAI 管理接口请求封装。

use serde_json::Value;

use crate::server::core::auth_http::{send_raw, ApiResponse};
use crate::server::core::proxies::{resolve_account_proxy, ProxyResolution, ResolvedProxy};
use crate::server::errors::GatewayError;

use super::credentials::{Credentials, CLIENT_VERSION, SERVER_BASE, USER_AGENT};

pub const REQUEST_TIMEOUT_MS: u64 = 30_000;

pub async fn request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    token: Option<&str>,
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let mut headers = vec![
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        (
            "X-LobsterAI-Client-Version".to_string(),
            CLIENT_VERSION.to_string(),
        ),
    ];
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        headers.push((
            "Authorization".to_string(),
            format!("Bearer {}", token.trim()),
        ));
    }
    send_raw(method, url, body, &headers, proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            let detail = crate::server::core::egress::describe_error_detail(&error);
            if error.is_timeout() {
                GatewayError::with_status(
                    504,
                    format!("LobsterAI 请求超时，请检查网络后重试（{detail}）"),
                )
                .with_code("lobsterai_transport")
            } else {
                GatewayError::with_status(
                    502,
                    format!("无法连接 LobsterAI，请检查网络或账号代理设置（{detail}）"),
                )
                .with_code("lobsterai_transport")
            }
        })
}

pub fn payload(response: ApiResponse, action: &str) -> Result<Value, GatewayError> {
    let status = response.status;
    let Some(payload) = response.payload else {
        return Err(GatewayError::with_status(
            i32::from(if response.ok { 502 } else { status }),
            format!("LobsterAI {action}未返回有效 JSON"),
        ));
    };
    if !response.ok {
        let message = upstream_message(&payload).unwrap_or("上游错误");
        return Err(GatewayError::with_status(
            i32::from(status),
            format!("LobsterAI {action}失败（HTTP {status}）：{message}"),
        ));
    }
    let code = payload
        .get("code")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
        })
        .unwrap_or(0);
    if code != 0 {
        let message = upstream_message(&payload).unwrap_or("上游业务错误");
        return Err(GatewayError::with_status(
            if code == 401 { 401 } else { 502 },
            format!("LobsterAI {action}失败（code={code}）：{message}"),
        )
        .with_code("lobsterai_business"));
    }
    Ok(payload.get("data").cloned().unwrap_or(payload))
}

pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

pub fn base_url() -> String {
    std::env::var("LOBSTERAI_SERVER_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| SERVER_BASE.to_string())
}

fn upstream_message(value: &Value) -> Option<&str> {
    value
        .get("message")
        .or_else(|| value.get("msg"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|message| !message.is_empty())
}

pub fn credentials_from_session(session: &Value) -> Result<Credentials, GatewayError> {
    Credentials::from_payload(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::auth_http::ApiResponse;
    use serde_json::json;

    #[test]
    fn envelope_data_is_unwrapped_and_business_error_is_not_silent() {
        let data = payload(
            ApiResponse {
                status: 200,
                ok: true,
                payload: Some(json!({"code":0,"data":{"x":1}})),
            },
            "测试",
        )
        .expect("data");
        assert_eq!(data["x"], 1);
        let error = payload(
            ApiResponse {
                status: 200,
                ok: true,
                payload: Some(json!({"code":7,"msg":"bad"})),
            },
            "测试",
        )
        .expect_err("business error");
        assert_eq!(error.code.as_deref(), Some("lobsterai_business"));
    }

    #[test]
    fn string_business_code_is_rejected() {
        let error = payload(
            ApiResponse {
                status: 200,
                ok: true,
                payload: Some(json!({"code":"401","message":"expired"})),
            },
            "测试",
        )
        .expect_err("string code");
        assert_eq!(error.status_code, 401);
    }
}
