//! MiniMax Code 管理接口请求封装。

use serde_json::Value;

use crate::server::core::auth_http::{send_form, send_raw, ApiResponse};
use crate::server::core::proxies::{resolve_account_proxy, ProxyResolution, ResolvedProxy};
use crate::server::errors::GatewayError;

use super::credentials::{SERVER_BASE, USER_AGENT};

pub const REQUEST_TIMEOUT_MS: u64 = 30_000;

pub async fn request_json(
    method: &str,
    url: &str,
    body: Option<&Value>,
    token: Option<&str>,
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let mut headers = vec![("User-Agent".to_string(), USER_AGENT.to_string())];
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        headers.push((
            "Authorization".to_string(),
            format!("Bearer {}", token.trim()),
        ));
    }
    send_raw(method, url, body, &headers, proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| transport_error(error, "MiniMax Code"))
}

pub async fn request_form(
    url: &str,
    form: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    send_form(
        "POST",
        url,
        form,
        &[("User-Agent".to_string(), USER_AGENT.to_string())],
        proxy,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| transport_error(error, "MiniMax OAuth"))
}

pub fn payload(response: ApiResponse, action: &str) -> Result<Value, GatewayError> {
    let status = response.status;
    let Some(payload) = response.payload else {
        return Err(GatewayError::with_status(
            i32::from(if response.ok { 502 } else { status }),
            format!("MiniMax Code {action}未返回有效 JSON"),
        ));
    };
    if !response.ok {
        let message = upstream_message(&payload).unwrap_or("上游错误");
        return Err(GatewayError::with_status(
            i32::from(status),
            format!("MiniMax Code {action}失败（HTTP {status}）：{message}"),
        ));
    }
    if let Some(code) = payload
        .get("base_resp")
        .and_then(|value| value.get("status_code"))
        .and_then(number)
        .filter(|code| *code != 0)
        .or_else(|| {
            payload
                .get("code")
                .and_then(number)
                .filter(|code| *code != 0)
        })
    {
        let message = upstream_message(&payload).unwrap_or("上游业务错误");
        return Err(GatewayError::with_status(
            if code == 401 { 401 } else { 502 },
            format!("MiniMax Code {action}失败（code={code}）：{message}"),
        )
        .with_code("minimax_business"));
    }
    Ok(payload)
}

fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
}

pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

pub fn base_url() -> String {
    std::env::var("MINIMAX_CODE_SERVER_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| SERVER_BASE.to_string())
}

fn upstream_message(value: &Value) -> Option<&str> {
    value
        .get("message")
        .or_else(|| value.get("msg"))
        .or_else(|| {
            value
                .get("base_resp")
                .and_then(|item| item.get("status_msg"))
        })
        .or_else(|| value.get("error").and_then(|item| item.get("message")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn transport_error(error: reqwest::Error, name: &str) -> GatewayError {
    let detail = crate::server::core::egress::describe_error_detail(&error);
    if error.is_timeout() {
        GatewayError::with_status(504, format!("{name}请求超时，请检查网络后重试（{detail}）"))
            .with_code("minimax_transport")
    } else {
        GatewayError::with_status(
            502,
            format!("无法连接{name}，请检查网络或账号代理设置（{detail}）"),
        )
        .with_code("minimax_transport")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::auth_http::ApiResponse;
    use serde_json::json;

    #[test]
    fn base_resp_business_error_is_not_treated_as_http_success() {
        let error = payload(
            ApiResponse {
                status: 200,
                ok: true,
                payload: Some(json!({
                    "base_resp": {"status_code": 7, "status_msg": "bad"}
                })),
            },
            "测试",
        )
        .expect_err("business error");
        assert_eq!(error.code.as_deref(), Some("minimax_business"));
    }

    #[test]
    fn string_business_code_is_not_treated_as_http_success() {
        let error = payload(
            ApiResponse {
                status: 200,
                ok: true,
                payload: Some(json!({"code":"401", "message":"expired"})),
            },
            "测试",
        )
        .expect_err("string business code");
        assert_eq!(error.status_code, 401);
    }
}
