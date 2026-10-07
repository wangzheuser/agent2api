//! 认证后的可靠会话身份；正文/前缀/每轮请求 ID 不参与逻辑会话键。

use crate::server::core::key_scope::RoutingPrincipal;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSession {
    /// 不透明摘要，不保存客户端身份、凭证或明文会话 ID。
    pub key: String,
}

impl RouteSession {
    pub fn from_body(body: &Value, principal: Option<&RoutingPrincipal>) -> Option<Self> {
        let principal = principal?;
        let metadata = body.get("metadata");
        let encoded = metadata
            .and_then(|m| m.get("user_id"))
            .and_then(Value::as_str);
        // 只解析一层 JSON；长度上限防止身份字段成为无界额外解析负担。
        let identity = encoded
            .filter(|s| s.len() <= 4096)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .filter(Value::is_object);
        let legacy = encoded.and_then(legacy_identity);
        let explicit = body
            .get("conversation_id")
            .or_else(|| body.get("conversationId"))
            .or_else(|| metadata.and_then(|m| m.get("conversation_id")))
            .or_else(|| metadata.and_then(|m| m.get("conversationId")));
        let conversation =
            match explicit.or_else(|| identity.as_ref().and_then(|i| i.get("session_id"))) {
                Some(value) => valid_id(Some(value))?,
                None => legacy?.1,
            };
        let device_value = identity.as_ref().and_then(|i| i.get("device_id"));
        let device = match device_value {
            Some(value) => valid_id(Some(value))?,
            None => legacy.map(|value| value.0).unwrap_or(""),
        };
        let route = match body.get("model") {
            Some(model) => valid_id(Some(model))?.to_lowercase(),
            None => String::new(),
        };
        Some(Self {
            key: opaque_key(&[principal.partition(), device, conversation, &route]),
        })
    }

    /// 实际上游分区：保持同 UID/地区稳定，切换账号或地区则隔离。
    pub(crate) fn for_target(&self, provider: &str, uid: &str) -> String {
        opaque_key(&[&self.key, provider, uid])
    }
}

/// 只接受已知旧客户端形态，普通 user_id 中碰巧出现 session 字样不算会话。
fn legacy_identity(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix("user_")?;
    let (device, rest) = value.split_once("_account_")?;
    if device.len() != 64 || !device.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let (account, session) = rest.split_once("_session_")?;
    if (!account.is_empty() && !uuid_shape(account)) || !uuid_shape(session) {
        return None;
    }
    Some((device, session))
}

fn uuid_shape(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn valid_id(value: Option<&Value>) -> Option<&str> {
    let value = value?.as_str()?;
    if value.chars().any(char::is_control) {
        return None;
    }
    let value = value.trim();
    (!value.is_empty() && value.len() <= 256).then_some(value)
}

fn opaque_key(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn route_session_validates_legacy_suffix_and_device_partition() {
        let principal = client("fixture");
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let raw = format!("user_{}_account__session_{session_id}", "a".repeat(64));
        let body = json!({"model":"glm", "metadata":{"user_id":raw}});
        let first = session(&body, &principal);
        let other = json!({"model":"glm", "metadata":{"user_id":format!("user_{}_account__session_{session_id}", "b".repeat(64))}});
        assert_ne!(first, session(&other, &principal));
        for raw in [
            format!("user_short_account__session_{session_id}"),
            format!(
                "user_{}_account__session_{session_id}_extra",
                "a".repeat(64)
            ),
            "anything_session_123".into(),
        ] {
            assert!(RouteSession::from_body(
                &json!({"metadata":{"user_id":raw}}),
                Some(&principal)
            )
            .is_none());
        }
    }

    fn client(key: &str) -> RoutingPrincipal {
        RoutingPrincipal::from_environment_key(key)
    }

    fn session(body: &Value, principal: &RoutingPrincipal) -> String {
        RouteSession::from_body(body, Some(principal))
            .expect("reliable session")
            .key
    }

    #[test]
    fn route_session_configured_key_identity_is_not_permission_or_secret() {
        use crate::server::core::api_keys::ApiKeyEntry;
        let mut entry = ApiKeyEntry {
            id: "entry-a".into(),
            name: "fixture".into(),
            key: "fixture-secret-a".into(),
            enabled: true,
            created_at: 0,
            allowed_providers: vec!["workbuddy".into()],
            allowed_models: vec!["glm".into()],
        };
        let body = json!({"conversation_id":"session", "model":"glm"});
        let initial = session(&body, &RoutingPrincipal::from_entry(&entry));
        entry.id = "entry-b".into();
        assert_ne!(
            initial,
            session(&body, &RoutingPrincipal::from_entry(&entry))
        );
        entry.id = "entry-a".into();
        entry.key = "rotated-secret".into();
        entry.allowed_providers.clear();
        entry.allowed_models.clear();
        assert_eq!(
            initial,
            session(&body, &RoutingPrincipal::from_entry(&entry))
        );
        assert_ne!(
            initial,
            session(
                &body,
                &RoutingPrincipal::from_environment_key("fixture-secret-a")
            )
        );
    }

    #[test]
    fn route_session_parses_real_encoded_identity_and_survives_content_changes() {
        let principal = client("fixture-client");
        let mut body = json!({"model":"workbuddy/glm-5.3-flash", "metadata": {
            "user_id": json!({"device_id":"device-a", "account_uuid":"", "session_id":"session-a"}).to_string()
        }, "messages":[{"role":"user","content":"first"}]});
        let first = session(&body, &principal);
        body["messages"] = json!([{"role":"system","content":"compacted"}]);
        body["tools"] = json!([{"name":"changed"}]);
        assert_eq!(first, session(&body, &principal));
        assert_eq!(first.len(), 64);
        assert!(!first.contains("session-a"));
    }

    #[test]
    fn route_session_partitions_client_device_model_and_explicit_provider() {
        let a = client("client-a");
        let b = client("client-b");
        let body = json!({"model":"workbuddy/glm", "conversation_id":"s",
            "metadata":{"user_id":json!({"session_id":"other","device_id":"a"}).to_string()}});
        let first = session(&body, &a);
        assert_ne!(first, session(&body, &b));
        for replacement in [
            json!({"model":"glm","conversation_id":"s"}),
            json!({"model":"workbuddy-intl/glm","conversation_id":"s"}),
            json!({"model":"workbuddy/glm","conversation_id":"s", "metadata":{"user_id":json!({"session_id":"other","device_id":"b"}).to_string()}}),
        ] {
            assert_ne!(first, session(&replacement, &a));
        }
        let mut normalized = body.clone();
        normalized["model"] = json!(" WorkBuddy/GLM ");
        assert_eq!(first, session(&normalized, &a));
    }

    #[test]
    fn route_session_explicit_precedence_and_untrusted_identity_rejection() {
        let principal = client("fixture-client");
        let simple = json!({"model":"glm","conversation_id":"top"});
        let mixed = json!({"model":"glm","conversation_id":"top","metadata": {
            "conversation_id":"meta","user_id":json!({"session_id":"encoded"}).to_string()}});
        assert_eq!(session(&simple, &principal), session(&mixed, &principal));
        let metadata = json!({"model":"glm","metadata":{"conversationId":"top"}});
        assert_eq!(session(&simple, &principal), session(&metadata, &principal));
        assert!(RouteSession::from_body(&simple, None).is_none());
        for body in [
            json!({"metadata":{"user_id":"arbitrary-user"}}),
            json!({"metadata":{"user_id":"{broken"}}),
            json!({"metadata":{"user_id":json!({"session_id":42}).to_string()}}),
            json!({"conversation_id":42}),
            json!({"conversation_id":""}),
            json!({"conversation_id":"a\nb"}),
            json!({"conversation_id":"x".repeat(257)}),
            json!({"conversation_id":"s","metadata":{"user_id":json!({"device_id":42}).to_string()}}),
        ] {
            assert!(
                RouteSession::from_body(&body, Some(&principal)).is_none(),
                "invalid identity accepted"
            );
        }
    }
}
