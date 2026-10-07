//! MiniMax Code 管理接口请求封装。

use md5::{Digest, Md5};
use serde_json::Value;
use url::Url;

use crate::server::core::auth_http::{send_form, send_raw, ApiResponse};
use crate::server::core::proxies::{resolve_account_proxy, ProxyResolution, ResolvedProxy};
use crate::server::errors::GatewayError;

use super::credentials::{Credentials, SERVER_BASE, USER_AGENT};

pub const REQUEST_TIMEOUT_MS: u64 = 30_000;

pub async fn request_json(
    method: &str,
    url: &str,
    body: Option<&Value>,
    token: &str,
    user_id: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let (url, mut headers) =
        management_request_parts(url, user_id, body, crate::server::logging::now_ms())?;
    headers.push(("Authorization".into(), format!("Bearer {}", token.trim())));
    send_raw(
        method,
        &url,
        body,
        &headers,
        proxy,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| transport_error(error, "MiniMax Code"))
}

// 对齐官方 MiniMax Code public-gateway 的客户端归因契约；OAuth 表单与推理不走这里。
fn management_request_parts(
    url: &str,
    user_id: &str,
    body: Option<&Value>,
    now_ms: i64,
) -> Result<(String, Vec<(String, String)>), GatewayError> {
    let mut url =
        Url::parse(url).map_err(|_| GatewayError::with_status(500, "MiniMax 管理接口地址无效"))?;
    let now = now_ms.to_string();
    let platform = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        os => os,
    };
    // 查询与领取均使用上海日界，与原签到调度口径一致。
    url.query_pairs_mut().extend_pairs([
        ("device_platform", "web"),
        ("biz_id", "3"),
        ("app_id", "3001"),
        ("version_code", "22201"),
        ("is_desktop", "1"),
        ("desktop_version", "0.4.12"),
        ("unix", now.as_str()),
        ("timezone_offset", "28800"),
        ("sys_language", "zh"),
        ("lang", "zh"),
        ("device_id", "0"),
        ("os_name", platform),
        ("browser_name", "mcode"),
        ("user_id", user_id),
        ("client", "mcode"),
    ]);
    let body_text = body.map(Value::to_string);
    let path_query = &url[url::Position::BeforePath..url::Position::AfterQuery];
    let encoded =
        crate::server::core::providers::codearts::signer::encode_component(path_query.as_bytes());
    let md5 = |text: &str| format!("{:x}", Md5::digest(text.as_bytes()));
    let second = (now_ms / 1000).to_string();
    // 公开的归因常量不是账号秘密；真正的鉴权仍由 Bearer token 完成。
    let yy = md5(&format!(
        "{encoded}_{}{}ooui",
        body_text.as_deref().unwrap_or("{}"),
        md5(&now)
    ));
    let signature = md5(&format!(
        "{second}I*7Cf%WZ#S&%1RlZJ&C2{}",
        body_text.as_deref().unwrap_or("")
    ));
    Ok((
        url.into(),
        vec![
            ("User-Agent".into(), "MiniMaxCode".into()),
            ("Content-Type".into(), "application/json".into()),
            ("yy".into(), yy),
            ("x-timestamp".into(), second),
            ("x-signature".into(), signature),
        ],
    ))
}

/// OAuth subject/本地 token 哈希不是业务 realUserID。只查询、不改账号主键或刷新链。
pub async fn real_user_id(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Result<String, GatewayError> {
    let response = request_json(
        "GET",
        &format!("{}/v1/api/user/info", base_url()),
        None,
        &credentials.access_token,
        "0",
        proxy,
    )
    .await?;
    let data = payload(response, "用户身份查询")?;
    let info = [
        "/data/userInfo",
        "/data/user_info",
        "/userInfo",
        "/user_info",
    ]
    .iter()
    .find_map(|path| data.pointer(path));
    let id = info
        .map(|info| super::credentials::text(info, &["realUserID", "real_user_id"]))
        .unwrap_or_default();
    if id.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "MiniMax 用户身份响应缺少 realUserID",
        ));
    }
    Ok(id)
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
        .or_else(|| {
            payload
                .pointer("/statusInfo/code")
                .and_then(number)
                .filter(|code| *code != 0)
        })
    {
        let message = upstream_message(&payload).unwrap_or("上游业务错误");
        return Err(GatewayError::with_status(
            if matches!(code, 401 | 1_000_048) {
                401
            } else {
                502
            },
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
    fn management_request_carries_identity_and_exact_body_signature() {
        let (url, headers) = management_request_parts("https://agent.minimax.cn/minimax-cloud/api/v1/signin/claim?timezone_id=Asia%2FShanghai", "user-1", Some(&json!({})), 1_800_000_000_000).unwrap();
        let url = Url::parse(&url).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["user_id"], "user-1");
        assert_eq!(query["client"], "mcode");
        assert_eq!(query["is_desktop"], "1");
        assert_eq!(query["timezone_offset"], "28800");
        assert_eq!(query["timezone_id"], "Asia/Shanghai");
        assert_eq!(
            headers
                .iter()
                .find(|(key, _)| key == "x-signature")
                .unwrap()
                .1,
            "8831c467e8e076516af22ae8d90f9eaf"
        );
        let (_, get_headers) =
            management_request_parts(url.as_str(), "user-1", None, 1_800_000_000_000).unwrap();
        assert_eq!(
            get_headers
                .iter()
                .find(|(key, _)| key == "x-signature")
                .unwrap()
                .1,
            "1d6aa390eab991e912a0043707d284d8"
        );
    }

    #[test]
    fn identity_business_auth_failure_is_not_a_successful_http_200() {
        let error = payload(
            ApiResponse {
                status: 200,
                ok: true,
                payload: Some(json!({"statusInfo":{"code":1000048}})),
            },
            "身份查询",
        )
        .unwrap_err();
        assert_eq!(error.status_code, 401);
    }

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
