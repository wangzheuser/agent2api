//! 自定义提供商的对外绑定；原始 ID 和别名各自控制可见性及路由。

use serde_json::{json, Value};

use crate::server::core::capability;
use crate::server::core::models::model_id;

pub fn resolve(provider: &Value, requested: &str) -> Option<(String, Option<String>)> {
    resolve_binding(provider, requested, false)
}

pub(super) fn resolve_test(provider: &Value, requested: &str) -> Option<(String, Option<String>)> {
    resolve_binding(provider, requested, true)
}

fn resolve_binding(provider: &Value, requested: &str, testing: bool) -> Option<(String, Option<String>)> {
    if !provider.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let models = provider.get("models").and_then(Value::as_array)?;
    let mappings = provider.get("mappings").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let level = |entry: &Value| entry.get("reasoning").and_then(Value::as_str)
        .map(str::trim).filter(|text| !text.is_empty()).map(str::to_string);
    if let Some(model) = models.iter().find(|model| model_id(model).eq_ignore_ascii_case(requested)) {
        let default = mappings.iter().find(|mapping| {
            mapping.get("alias").and_then(Value::as_str).is_some_and(|alias| alias.eq_ignore_ascii_case(requested))
                && mapping.get("target").and_then(Value::as_str).is_some_and(|target| target.eq_ignore_ascii_case(requested))
        });
        if testing || (model.get("enabled").and_then(Value::as_bool).unwrap_or(true)
            && default.map_or(true, |mapping| mapping.get("enabled").and_then(Value::as_bool).unwrap_or(true)))
        {
            return Some((model_id(model), default.and_then(level).or_else(|| level(model))));
        }
    }
    if testing { return None; }
    mappings.iter().find_map(|mapping| {
        let alias = mapping.get("alias")?.as_str()?;
        let target = mapping.get("target")?.as_str()?;
        if !alias.eq_ignore_ascii_case(requested) || alias.eq_ignore_ascii_case(target)
            || !mapping.get("enabled").and_then(Value::as_bool).unwrap_or(true)
        {
            return None;
        }
        // target 的 enabled 只控制原始 ID，不能阻止别名调用它。
        let model = models.iter().find(|model| model_id(model).eq_ignore_ascii_case(target))?;
        Some((model_id(model), level(mapping)))
    })
}

/// 该家对外的模型条目（`{id, …能力位}`）—— 清单出口与 `/v1/models` 共用。
///
/// 条目的 id 是**对外名**（原始 ID 或某个存活的别名），能力位则取自
/// **target 的登记条目**：别名的清单条目是这里现造的、没有自己的记录，
/// 而下游按对外名看到的能力必须与按原始 ID 看到的完全一致 —— 否则同一个
/// 上游模型会有两套互相打架的声明。
///
/// 能力位**平铺**在条目顶层（与各内置家清单同形，`list_item` 直接读它们）；
/// 存储里那层 `capabilities` 对象由 `capability::apply_overrides` 展开 ——
/// 归一也走它，手改数据里的坏值到不了出口。
pub fn public_models(provider: &Value) -> Vec<Value> {
    let models = provider.get("models").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut names = Vec::new();
    names.extend(models.iter().map(model_id));
    if let Some(mappings) = provider.get("mappings").and_then(Value::as_array) {
        names.extend(mappings.iter().filter_map(|mapping| mapping.get("alias").and_then(Value::as_str).map(str::to_string)));
    }
    let mut result = Vec::new();
    for name in names {
        if name.is_empty()
            || result.iter().any(|model| model_id(model).eq_ignore_ascii_case(&name))
        {
            continue;
        }
        let Some((target, _)) = resolve(provider, &name) else { continue };
        let mut item = json!({ "id": name });
        let capabilities = capability::normalize_object(
            models
                .iter()
                .find(|model| model_id(model).eq_ignore_ascii_case(&target))
                .and_then(|model| model.get("capabilities")),
        );
        capability::apply_overrides(&mut item, &capabilities);
        result.push(item);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolution_ignores_model_switches_but_not_provider_switch_or_raw_catalog() {
        let mut provider = json!({"enabled":true,"models":[{"id":"Raw-A","enabled":false,"reasoning":"low"}],
            "mappings":[{"alias":"Raw-A","target":"Raw-A","enabled":false,"reasoning":"high"},
                        {"alias":"alias","target":"Raw-A","enabled":true}]});
        let original = provider.clone();
        assert!(resolve(&provider, "Raw-A").is_none());
        assert_eq!(resolve_test(&provider, "raw-a"), Some(("Raw-A".into(), Some("high".into()))));
        assert!(resolve_test(&provider, "alias").is_none());
        assert!(resolve_test(&provider, "absent").is_none());
        assert_eq!(resolve(&provider, "alias"), Some(("Raw-A".into(), None)));
        assert_eq!(provider, original);
        provider["enabled"] = json!(false);
        assert!(resolve_test(&provider, "Raw-A").is_none());
    }
}
