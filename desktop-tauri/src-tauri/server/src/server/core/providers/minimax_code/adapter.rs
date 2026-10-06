//! MiniMax Code ProviderAdapter：Anthropic Messages、续期、额度和网页登录。

use std::future::Future;
use std::pin::Pin;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::protocol::anthropic_outbound;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, SuccessHead, UpstreamErrorClass,
    UpstreamResponse,
};
use crate::server::core::providers::content_block;
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;

use super::{auth, balance, credentials, models, refresh};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct MiniMaxCodeAdapter;

pub static MINIMAX_CODE_ADAPTER: MiniMaxCodeAdapter = MiniMaxCodeAdapter;

impl ProviderAdapter for MiniMaxCodeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::MiniMaxCode
    }

    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    fn supports_default_model(&self) -> bool {
        true
    }

    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let credential = credentials::Credentials::from_payload(account)?;
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(models::default_model());
        let mut payload =
            anthropic_outbound::anthropic_request_from_chat(body, model).map_err(|message| {
                GatewayError::with_status(400, format!("请求体转换失败：{message}"))
            })?;
        if let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                if let Some(object) = tool.as_object_mut() {
                    object.insert("type".to_string(), Value::String("custom".to_string()));
                }
            }
        }
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
            (
                "Authorization".to_string(),
                format!("Bearer {}", credential.access_token.trim()),
            ),
            (
                "anthropic-version".to_string(),
                ANTHROPIC_VERSION.to_string(),
            ),
            (
                "User-Agent".to_string(),
                credentials::USER_AGENT.to_string(),
            ),
        ];
        if let Some(beta) = client_headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            headers.push(("Anthropic-Beta".to_string(), beta.to_string()));
        }
        Ok(ChatRequestPlan {
            url: format!(
                "{}/mavis/api/v1/llm/v1/messages?beta=true",
                auth::base_url()
            ),
            headers,
            body: payload,
            response: UpstreamResponse::Anthropic,
        })
    }

    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_message(error_body);
        let code = error_code(error_body);
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 || code == Some(401) {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 || code == Some(429) || is_quota_text(&raw) {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            };
        }
        content_block::classify_or_fatal(status, error_body, message, code)
    }

    fn inspect_success_head(&self, body: &[u8]) -> SuccessHead {
        let text = String::from_utf8_lossy(body);
        if text.lines().any(|line| line.trim() == "event: error") || text.contains("event:error") {
            let value = first_json_value(body).unwrap_or_else(|| serde_json::json!({}));
            return SuccessHead::Failure(self.classify_error(200, &value));
        }
        if first_json_value(body).is_some() || body.len() >= 8192 {
            SuccessHead::Ready
        } else {
            SuccessHead::Pending
        }
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
        false
    }
}

fn error_message(value: &Value) -> String {
    value
        .get("message")
        .or_else(|| value.get("msg"))
        .or_else(|| value.get("error").and_then(|error| error.get("message")))
        .or_else(|| {
            value
                .get("base_resp")
                .and_then(|error| error.get("status_msg"))
        })
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("上游错误")
        .to_string()
}

fn error_code(value: &Value) -> Option<i64> {
    [
        value.get("code"),
        value.get("error").and_then(|error| error.get("code")),
        value
            .get("base_resp")
            .and_then(|response| response.get("status_code")),
        value
            .get("error")
            .and_then(|error| error.get("base_resp"))
            .and_then(|response| response.get("status_code")),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
    })
}

fn is_quota_text(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "quota",
        "rate limit",
        "too many",
        "insufficient",
        "余额",
        "额度",
        "限流",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn first_json_value(body: &[u8]) -> Option<Value> {
    body.split(|byte| *byte == b'\n' || *byte == b'\r')
        .filter_map(|line| {
            let line = line.strip_prefix(b"data:").unwrap_or(line);
            let line = std::str::from_utf8(line).ok()?.trim();
            if line.is_empty() || line == "[DONE]" {
                None
            } else {
                serde_json::from_str(line).ok()
            }
        })
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_requests_become_anthropic_and_tools_get_custom_type() {
        let plan = MiniMaxCodeAdapter
            .build_chat_request(
                &json!({"accessToken":"token"}),
                &json!({"model":"MiniMax-M3","messages":[{"role":"user","content":"hi"}],"tools":[{"name":"x","input_schema":{"type":"object"}}]}),
                &HeaderMap::new(),
            )
            .expect("plan");
        assert_eq!(plan.response, UpstreamResponse::Anthropic);
        assert_eq!(plan.body["tools"][0]["type"], "custom");
        assert!(plan.url.contains("beta=true"));
    }

    #[test]
    fn http_200_sse_error_event_is_not_marked_ready() {
        let adapter = MiniMaxCodeAdapter;
        let head = b"event: error\ndata: malformed\n\n";
        assert!(matches!(
            adapter.inspect_success_head(head),
            SuccessHead::Failure(UpstreamErrorClass::Fatal { .. })
        ));
    }

    #[test]
    fn http_200_sse_token_error_triggers_refresh_classification() {
        let adapter = MiniMaxCodeAdapter;
        let head = b"event: error\ndata: {\"code\":401,\"message\":\"expired\"}\n\n";
        assert!(matches!(
            adapter.inspect_success_head(head),
            SuccessHead::Failure(UpstreamErrorClass::TokenExpired { .. })
        ));
    }

    #[test]
    fn nested_and_string_sse_error_codes_trigger_refresh_classification() {
        let adapter = MiniMaxCodeAdapter;
        let head = b"event: error\ndata: {\"error\":{\"code\":\"401\",\"message\":\"expired\"}}\n\n";
        assert!(matches!(
            adapter.inspect_success_head(head),
            SuccessHead::Failure(UpstreamErrorClass::TokenExpired { .. })
        ));
    }
}
