//! LobsterAI 模型清单。
//!
//! LobsterAI 代理目前对外稳定提供 DeepSeek Flash；上游客户端版本可能追加
//! 模型，因此保留一处静态兜底，后续可在这里接远程目录缓存。

use serde_json::{json, Value};

pub const DEFAULT_MODEL: &str = "deepseek-flash";

pub fn list() -> Vec<Value> {
    vec![json!({
        "id": DEFAULT_MODEL,
        "name": "DeepSeek v4.1 Flash (LobsterAI)",
        "displayName": "DeepSeek v4.1 Flash (LobsterAI)",
        "maxInputTokens": 131_072,
        "maxOutputTokens": 131_072,
        "supportsToolCall": true,
        "supportsReasoning": true,
        "kind": "chat",
    })]
}

pub fn wire_model(requested: &str) -> &str {
    if requested.trim().is_empty() {
        DEFAULT_MODEL
    } else {
        // LobsterAI 当前代理把模型名作为路由标识原样接收；未知模型由上游
        // 返回业务错误，不在网关静默替换用户显式选择。
        requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_contains_single_tool_capable_model() {
        let models = list();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], DEFAULT_MODEL);
        assert_eq!(models[0]["supportsToolCall"], true);
    }

    #[test]
    fn explicit_model_is_not_silently_replaced() {
        assert_eq!(wire_model("custom-model"), "custom-model");
        assert_eq!(wire_model(""), DEFAULT_MODEL);
    }
}
