//! CatPaw 工具字段归一化：OpenAI `tools` / `tool_choice` → 上游 `toolConfigs`。
//!
//! 移植来源 `proxy-chat-utils.mjs` 的 `normalizeTools` 与
//! `catpaw-upstream-messages.mjs` 的 `normalizeTools` / `toolChoiceMode`
//! （两处的 `normalizeTools` 规则一致，以后者为准：它多了 `enable` /
//! `fromClient` 两个上游要求的字段）。
//!
//! ── 字段映射（UPSTREAM_PROTOCOL §3.2 是唯一权威）───────────────
//! | OpenAI 入站 | 上游 `toolConfigs[]` |
//! |---|---|
//! | `type:"function"` | （不出现；整条就是工具定义） |
//! | `function.name` | `name` |
//! | `function.description` | `description`（截 8000 字符） |
//! | `function.parameters` | `inputSchema`（缺省 `{"type":"object","properties":{}}`） |
//! | — | `enable: true` / `fromClient: true`（常量：客户端工具一律启用） |
//!
//! ── 为什么 `strict` 一律拒绝 ────────────────────────────────
//! 原实现对 `strict: true` 直接报 400（`strict=true 暂不支持`）。上游没有
//! 「强制按 schema 输出」的开关，声称支持等于骗客户端 —— 客户端以为拿到了
//! 结构性保证，实际没有。`strict: false` 是默认行为，放行。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 纯函数：不发网络请求、不持锁、不读盘，零 unwrap/expect/panic。

use serde_json::{json, Map, Value};

use super::models::{CatPawError, MAX_JSON_NESTING_DEPTH};

/// `tool_choice` 的归一形态（原实现 `toolChoiceMode`）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    /// 指定函数名（`{type:"function", function:{name}}`）
    Function(String),
}

/// 解析 `tool_choice`（原实现 `toolChoiceMode`）。
///
/// `undefined` / `null` / `"auto"` 都归 `Auto`；对象形态只认
/// `{type:"function", function:{name:"…"}}`，其余报 400。
pub fn tool_choice_mode(tool_choice: Option<&Value>) -> Result<ToolChoice, CatPawError> {
    let Some(value) = tool_choice else {
        return Ok(ToolChoice::Auto);
    };
    if value.is_null() {
        return Ok(ToolChoice::Auto);
    }
    match value {
        Value::String(text) if text == "auto" => Ok(ToolChoice::Auto),
        Value::String(text) if text == "none" => Ok(ToolChoice::None),
        Value::String(text) if text == "required" => Ok(ToolChoice::Required),
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str);
            let name = object
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str);
            match (kind, name) {
                (Some("function"), Some(name)) if !name.is_empty() => {
                    Ok(ToolChoice::Function(name.to_string()))
                }
                _ => Err(invalid_choice()),
            }
        }
        _ => Err(invalid_choice()),
    }
}

fn invalid_choice() -> CatPawError {
    CatPawError::bad_request("tool_choice 只支持 auto、none、required 或指定 function")
}

/// `tools` → 上游 `toolConfigs`（原实现 `normalizeTools`）。
///
/// `None`（客户端没给 tools 字段）返回空数组，不是错误 —— 不带工具的纯对话
/// 完全正常。给了但形态不对（非数组、非 function 类型）才报 400。
pub fn normalize_tools(tools: Option<&Value>) -> Result<Vec<Value>, CatPawError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    if tools.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = tools.as_array() else {
        return Err(CatPawError::bad_request("tools 必须是数组"));
    };
    let mut names: Vec<&str> = Vec::with_capacity(items.len());
    let mut out: Vec<Value> = Vec::with_capacity(items.len());
    for (index, tool) in items.iter().enumerate() {
        let object = tool.as_object();
        let function = object
            .filter(|object| object.get("type").and_then(Value::as_str) == Some("function"))
            .and_then(|object| object.get("function"))
            .and_then(Value::as_object)
            .ok_or_else(|| {
                CatPawError::bad_request(format!("tools[{index}] 只支持 type=function"))
            })?;
        if let Some(strict) = function.get("strict").filter(|value| !value.is_null()) {
            match strict.as_bool() {
                None => {
                    return Err(CatPawError::bad_request(format!(
                        "tools[{index}].function.strict 必须是布尔值"
                    )))
                }
                Some(true) => {
                    return Err(CatPawError::bad_request(format!(
                        "tools[{index}].function.strict=true 暂不支持"
                    )))
                }
                Some(false) => {}
            }
        }
        let name = function.get("name").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty()
            || name.len() > 128
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
        {
            return Err(CatPawError::bad_request(format!(
                "tools[{index}].function.name 无效"
            )));
        }
        if names.contains(&name) {
            return Err(CatPawError::bad_request(format!(
                "tools[{index}] 重复的工具名称: {name}"
            )));
        }
        names.push(name);
        // `fn.parameters ?? { type:'object', properties:{} }`：**空值合并**
        // （显式 null 也落到默认值；空对象 `{}` 是合法 schema，原样用）
        let schema = match function.get("parameters") {
            Some(value) if !value.is_null() => value.clone(),
            _ => json!({ "type": "object", "properties": {} }),
        };
        if !schema.is_object() {
            return Err(CatPawError::bad_request(format!(
                "tools[{index}].function.parameters 必须是 JSON Schema 对象"
            )));
        }
        validate_json_value(&schema, &format!("tools[{index}].function.parameters"), 0)?;
        let mut config = Map::new();
        config.insert("name".to_string(), Value::String(name.to_string()));
        config.insert("enable".to_string(), Value::Bool(true));
        if let Some(description) = function.get("description").and_then(Value::as_str) {
            config.insert(
                "description".to_string(),
                Value::String(description.chars().take(8000).collect()),
            );
        }
        config.insert("inputSchema".to_string(), schema);
        config.insert("fromClient".to_string(), Value::Bool(true));
        out.push(Value::Object(config));
    }
    Ok(out)
}

/// 按 `tool_choice` 选出本轮实际下发给上游的工具集
/// （原实现 `prepareRequest` 中段）。
///
/// | tool_choice | 下发的工具集 |
/// |---|---|
/// | `none` | 空（并已在上游侧断言「不许返回 tool_call」） |
/// | `auto` / `required` | 全部 |
/// | 指定函数 | 只含那一个（不存在则 400） |
pub fn select_tools(all: &[Value], choice: &ToolChoice) -> Result<Vec<Value>, CatPawError> {
    match choice {
        ToolChoice::None => Ok(Vec::new()),
        ToolChoice::Auto => Ok(all.to_vec()),
        ToolChoice::Required => {
            if all.is_empty() {
                return Err(CatPawError::bad_request(
                    "tool_choice 要求至少提供一个 function 工具",
                ));
            }
            Ok(all.to_vec())
        }
        ToolChoice::Function(name) => {
            let selected: Vec<Value> = all
                .iter()
                .filter(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str()))
                .cloned()
                .collect();
            if selected.is_empty() {
                return Err(CatPawError::bad_request(format!(
                    "tool_choice 指定的工具不存在: {name}"
                )));
            }
            Ok(selected)
        }
    }
}

/// JSON 值合法性校验（原实现 `validateJsonValue`）：嵌套深度受限、
/// 对象内不允许 `__proto__` / `constructor` / `prototype` 三个键。
///
/// 为什么网关也要挡这三个键：它们会一路进上游 JSON（上游是 JS 生态，
/// 原型污染是真实攻击面），且这类输入没有任何正常用途。
///
/// 深度上限见 [`MAX_JSON_NESTING_DEPTH`]：原实现写死 12，会把 Codex 这类
/// 合法且较深的工具 schema 误杀，故放宽。
fn validate_json_value(value: &Value, path: &str, depth: usize) -> Result<(), CatPawError> {
    if depth > MAX_JSON_NESTING_DEPTH {
        return Err(CatPawError::bad_request(format!("{path} 嵌套过深")));
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                validate_json_value(item, &format!("{path}[{index}]"), depth + 1)?;
            }
            Ok(())
        }
        Value::Object(object) => {
            for (key, item) in object {
                if matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
                    return Err(CatPawError::bad_request(format!(
                        "{path} 包含不允许的字段 {key}"
                    )));
                }
                validate_json_value(item, &format!("{path}.{key}"), depth + 1)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个指定深度的嵌套对象：第 n 层挂 `{"child": ...}`。
    fn nested_object(depth: usize) -> Value {
        let mut value = json!({});
        for _ in 0..depth {
            value = json!({ "child": value });
        }
        value
    }

    fn function_tool(name: &str, parameters: Value) -> Value {
        json!({
            "type": "function",
            "function": { "name": name, "parameters": parameters }
        })
    }

    /// 复现线上报错：`tools[12]` 的 schema 深到 13 层，原实现直接 400。
    #[test]
    fn normalize_tools_accepts_the_schema_that_used_to_be_rejected() {
        let deep = json!({
            "type": "object",
            "properties": {
                "target": {
                    "anyOf": [{
                        "type": "object",
                        "properties": {
                            "environment": {
                                "anyOf": [
                                    { "type": "string" },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "startingState": {
                                                "anyOf": [{
                                                    "type": "object",
                                                    "additionalProperties": { "type": "string" }
                                                }]
                                            }
                                        }
                                    }
                                ]
                            }
                        }
                    }]
                }
            }
        });
        let mut tools: Vec<Value> = (0..12)
            .map(|index| function_tool(&format!("tool_{index}"), json!({ "type": "object" })))
            .collect();
        tools.push(function_tool("exec", deep));

        let normalized = normalize_tools(Some(&Value::Array(tools)))
            .expect("合法的深层 schema 不应再被拒绝");
        assert_eq!(normalized.len(), 13);
        assert_eq!(
            normalized[12].get("name").and_then(Value::as_str),
            Some("exec")
        );
    }

    #[test]
    fn accepts_deep_but_legal_tool_schema() {
        // 复现报错现场：Codex 工具 schema 的
        // `target.anyOf[0].environment.anyOf[1].startingState.anyOf[0]
        //  .additionalProperties` 已超过原实现的 12 层上限。
        let schema = json!({
            "type": "object",
            "properties": {
                "target": {
                    "anyOf": [{
                        "type": "object",
                        "properties": {
                            "environment": {
                                "anyOf": [
                                    { "type": "string" },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "startingState": {
                                                "anyOf": [{
                                                    "type": "object",
                                                    "additionalProperties": { "type": "string" }
                                                }]
                                            }
                                        }
                                    }
                                ]
                            }
                        }
                    }]
                }
            }
        });
        assert!(validate_json_value(&schema, "tools[12].function.parameters", 0).is_ok());
    }

    #[test]
    fn accepts_nesting_up_to_the_limit() {
        assert!(validate_json_value(
            &nested_object(MAX_JSON_NESTING_DEPTH),
            "schema",
            0
        )
        .is_ok());
    }

    #[test]
    fn rejects_pathological_nesting() {
        let error = validate_json_value(
            &nested_object(MAX_JSON_NESTING_DEPTH + 2),
            "schema",
            0,
        )
        .expect_err("超过上限应当报错");
        assert!(error.message.contains("嵌套过深"), "{}", error.message);
    }

    #[test]
    fn rejects_prototype_pollution_keys() {
        for key in ["__proto__", "constructor", "prototype"] {
            // `json!` 的 `key:` 是字面量键名，动态键必须自己建 Map。
            let mut properties = Map::new();
            properties.insert(key.to_string(), json!({ "type": "string" }));
            let schema = json!({ "properties": properties });
            let error = validate_json_value(&schema, "schema", 0)
                .expect_err("危险键应当被拒绝");
            assert!(error.message.contains("包含不允许的字段"), "{}", error.message);
        }
    }
}
