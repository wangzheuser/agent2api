//! MiniMax Code 静态模型清单。
//!
//! 当前官方 Code 通道模型数量少且变更频率低；保留 `refresh_models` 扩展点，
//! 以后可在不改适配器契约的情况下接远程目录。

use serde_json::{json, Value};

pub fn list() -> Vec<Value> {
    json!([
        {
            "id": "MiniMax-M3",
            "name": "MiniMax-M3",
            "description": "MiniMax Code 官方 Anthropic 通道",
            "contextWindow": 512000,
            "maxTokens": 128000,
            "supportImage": true,
            "supportThinking": true,
            "supportsToolCall": true,
        },
        {
            "id": "MiniMax-M2.7",
            "name": "MiniMax-M2.7",
            "description": "MiniMax Code 官方模型",
            "contextWindow": 200000,
            "maxTokens": 128000,
            "supportImage": false,
            "supportThinking": true,
            "supportsToolCall": true,
        },
        {
            "id": "MiniMax-M2.7-highspeed",
            "name": "MiniMax-M2.7-HighSpeed",
            "description": "MiniMax Code 高速模型",
            "contextWindow": 200000,
            "maxTokens": 128000,
            "supportImage": false,
            "supportThinking": true,
            "supportsToolCall": true,
        }
    ])
    .as_array()
    .cloned()
    .unwrap_or_default()
}

pub fn default_model() -> &'static str {
    "MiniMax-M3"
}
