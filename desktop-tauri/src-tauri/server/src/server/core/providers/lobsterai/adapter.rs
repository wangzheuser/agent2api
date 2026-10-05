//! LobsterAI ProviderAdapter：OpenAI Chat Completions、续期、网页登录和额度。

use std::future::Future;
use std::pin::Pin;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, SuccessHead, UpstreamErrorClass,
};
use crate::server::core::providers::content_block;
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;

use super::{auth, balance, credentials, models, oauth, refresh};

const SUCCESS_HEAD_LIMIT: usize = 8192;

pub struct LobsterAIAdapter;

pub static LOBSTERAI_ADAPTER: LobsterAIAdapter = LobsterAIAdapter;

impl ProviderAdapter for LobsterAIAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::LobsterAI
    }

    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let credential = credentials::Credentials::from_payload(account)?;
        if credential.access_token.trim().is_empty() {
            return Err(GatewayError::with_status(
                401,
                "LobsterAI 账号缺少 accessToken，请重新登录",
            ));
        }
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
            (
                "Authorization".to_string(),
                format!("Bearer {}", credential.access_token.trim()),
            ),
            (
                "User-Agent".to_string(),
                credentials::USER_AGENT.to_string(),
            ),
            (
                "X-LobsterAI-Client-Version".to_string(),
                credentials::CLIENT_VERSION.to_string(),
            ),
            (
                "X-LobsterAI-Client-Capabilities".to_string(),
                credentials::CLIENT_CAPABILITIES.to_string(),
            ),
        ];
        Ok(ChatRequestPlan::chat(
            format!(
                "{}/api/proxy/v1/chat/completions?clientVersion={}",
                auth::base_url(),
                credentials::CLIENT_VERSION
            ),
            headers,
            body.clone(),
        ))
    }

    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_message(error_body);
        let code = error_body.get("code").and_then(Value::as_i64);
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429
            || is_quota_error(code, &raw)
            || ((status == 402 || status == 403) && is_quota_text(&raw))
        {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: if status == 402 || status == 403 {
                    429
                } else {
                    status
                },
            };
        }
        content_block::classify_or_fatal(status, error_body, message, code)
    }

    fn inspect_success_head(&self, body: &[u8]) -> SuccessHead {
        let text = String::from_utf8_lossy(body);
        let has_error_event =
            text.lines().any(|line| line.trim() == "event: error") || text.contains("event:error");
        let Some(value) = first_json_value(body) else {
            if has_error_event || body.len() >= SUCCESS_HEAD_LIMIT {
                return SuccessHead::Ready;
            }
            return SuccessHead::Pending;
        };
        let Some(error) = value.get("error") else {
            if has_error_event {
                let message = format!("上游返回 200: {}", error_message(&value));
                return SuccessHead::Failure(UpstreamErrorClass::Fatal {
                    status: 502,
                    message,
                    upstream_code: value.get("code").and_then(Value::as_i64),
                });
            }
            return SuccessHead::Ready;
        };
        if !error.is_object() {
            return SuccessHead::Ready;
        }
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .or_else(|| value.get("code").and_then(Value::as_i64));
        let raw = error_message_optional(error).unwrap_or_default();
        if raw.is_empty() && !has_error_event {
            return SuccessHead::Ready;
        }
        let message = format!(
            "上游返回 200: {}",
            if raw.is_empty() {
                "上游业务错误"
            } else {
                &raw
            }
        );
        if is_quota_error(code, &raw) || is_quota_text(&raw) {
            return SuccessHead::Failure(UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            });
        }
        if code == Some(401) {
            return SuccessHead::Failure(UpstreamErrorClass::TokenExpired { message });
        }
        SuccessHead::Failure(UpstreamErrorClass::Fatal {
            status: 502,
            message,
            upstream_code: code,
        })
    }

    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(refresh::ensure_fresh(store, account_id, false)
                .await?
                .access_token)
        })
    }

    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(refresh::ensure_fresh(store, account_id, true)
                .await?
                .access_token)
        })
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        if account_id.trim().is_empty() {
            return false;
        }
        refresh::snapshot(store, account_id)
            .map(|(_, _, credentials)| {
                credentials.can_refresh() && credentials.needs_refresh(false)
            })
            .unwrap_or(false)
    }

    fn refresh_models<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
        _force: bool,
    ) -> Pin<Box<dyn Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move { ModelRefreshOutcome::unchanged() })
    }

    fn supports_usage(&self) -> bool {
        true
    }

    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value, GatewayError>> + Send + 'a>> {
        Box::pin(async move { balance::query_usage(store, account_id).await })
    }

    fn supports_web_login(&self) -> bool {
        true
    }

    fn build_login_url(&self) -> Option<(String, String)> {
        oauth::begin_login()
    }

    fn exchange_login_code<'a>(
        &'a self,
        store: &'a AccountStore,
        code: &'a str,
        state: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let credentials = oauth::exchange_code(code, state, None).await?;
            let account = store
                .add_lobsterai_account(&credentials, None, "web")
                .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
            account
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| GatewayError::with_status(500, "LobsterAI 登录成功但账号未能写入"))
        })
    }
}

fn error_message(value: &Value) -> String {
    error_message_optional(value).unwrap_or_else(|| "上游错误".to_string())
}

fn error_message_optional(value: &Value) -> Option<String> {
    value
        .get("message")
        .or_else(|| value.get("msg"))
        .or_else(|| value.get("error").and_then(|error| error.get("message")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_string)
}

fn is_quota_error(code: Option<i64>, message: &str) -> bool {
    matches!(code, Some(40201 | 40202 | 42901 | 42902)) || is_quota_text(message)
}

fn is_quota_text(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "quota",
        "credit",
        "credits",
        "insufficient",
        "余额不足",
        "额度不足",
        "积分不足",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn first_json_value(body: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(body).ok()?;
    for line in text.lines() {
        let line = line.trim();
        let candidate = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if candidate.is_empty() || candidate == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quota_and_token_errors_are_classified() {
        let adapter = LobsterAIAdapter;
        assert!(matches!(
            adapter.classify_error(401, &json!({"message":"expired"})),
            UpstreamErrorClass::TokenExpired { .. }
        ));
        assert!(matches!(
            adapter.classify_error(402, &json!({"message":"insufficient credits"})),
            UpstreamErrorClass::QuotaLimited { .. }
        ));
    }

    #[test]
    fn http_200_sse_business_error_is_not_marked_ready() {
        let adapter = LobsterAIAdapter;
        let head =
            b"event: error\ndata: {\"error\":{\"code\":40201,\"message\":\"quota exceeded\"}}\n\n";
        assert!(matches!(
            adapter.inspect_success_head(head),
            SuccessHead::Failure(UpstreamErrorClass::QuotaLimited { .. })
        ));
    }

    #[test]
    fn first_json_value_skips_done_marker() {
        assert_eq!(
            first_json_value(b"data: [DONE]\n\ndata: {\"ok\":true}\n").expect("json")["ok"],
            true
        );
    }
}
