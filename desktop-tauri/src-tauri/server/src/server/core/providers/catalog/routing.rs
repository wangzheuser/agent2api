//! 对外模型名到上游模型的解析。路由与发送侧共用本解析，避免选中一家后再换一套规则。

use serde_json::Value;

use crate::server::core::models::model_id;
use crate::server::core::providers::{kind_from_id, kind_id, ProviderKind};
use crate::server::core::{custom_providers, model_rules};

use super::{adapter_for, all_kinds, manifest_for};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WireTarget {
    pub model: String,
    pub reasoning: Option<String>,
}

pub(super) fn entry_id_in_manifest(manifest: &[Value], requested: &str) -> Option<String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return None;
    }
    // 保留展示名兼容，但必须先匹配原始 ID；返回清单里的实际大小写。
    manifest
        .iter()
        .find(|entry| model_id(entry).eq_ignore_ascii_case(requested))
        .or_else(|| {
            manifest.iter().find(|entry| {
                entry
                    .get("name")
                .map(crate::server::core::models::shape_value_text)
                .is_some_and(|name| name.eq_ignore_ascii_case(requested))
            })
        })
        .map(model_id)
}

pub(super) fn builtin_target(
    rules: &model_rules::ModelRules,
    kind: ProviderKind,
    manifest: &[Value],
    requested: &str,
) -> Option<WireTarget> {
    let provider = kind_id(kind);
    if let Some(target) = default_target(rules, provider, manifest, requested) {
        if rules.default_enabled(provider, &target.model) {
            return Some(target);
        }
    }
    let candidates: Vec<_> = rules
        .mappings
        .iter()
        .filter(|mapping| mapping.alias.eq_ignore_ascii_case(requested))
        .filter(|mapping| !mapping.alias.eq_ignore_ascii_case(&mapping.target))
        .filter(|mapping| {
            mapping
                .provider
                .as_deref()
                .map_or(true, |id| id == provider)
        })
        .filter(|mapping| {
            rules
                .binding(provider, &mapping.alias, &mapping.target)
                .is_some_and(|effective| effective.enabled)
        })
        .filter_map(|mapping| {
            entry_id_in_manifest(manifest, &mapping.target).map(|target| (mapping, target))
        })
        .collect();
    let prefix = super::super::cline::models::Pool::from_provider_id(provider)
        .map(super::super::cline::models::Pool::target_prefix);
    let selected = candidates
        .iter()
        .filter(|(mapping, _)| mapping.provider.is_some())
        .find(|(_, target)| prefix.is_some_and(|prefix| target.starts_with(prefix)))
        .or_else(|| {
            candidates
                .iter()
                .find(|(mapping, _)| mapping.provider.is_some())
        })
        .or_else(|| candidates.first());
    selected.map(|(mapping, target)| WireTarget {
        model: target.clone(),
        reasoning: rules
            .binding(provider, &mapping.alias, &mapping.target)
            .and_then(|effective| effective.reasoning.clone()),
    })
}

fn default_target(rules: &model_rules::ModelRules, provider: &str, manifest: &[Value], requested: &str) -> Option<WireTarget> {
    let id = entry_id_in_manifest(manifest, requested)?;
    Some(WireTarget {
        reasoning: rules.binding(provider, &id, &id).and_then(|binding| binding.reasoning.clone()),
        model: id,
    })
}

/// 只解析该家目录中的原始模型，保留默认等级；不开放别名或修改开关。
pub fn test_target_for_provider(model: &str, provider: &str) -> Option<WireTarget> {
    if let Some(kind) = kind_from_id(provider) {
        return default_target(&model_rules::current(), kind_id(kind), &manifest_for(kind), model);
    }
    custom_providers::test_wire_model_for(provider, model).map(|(model, reasoning)| WireTarget { model, reasoning })
}

/// 原始能力查询不应用开关，供管理 API 展开旧版全局规则。
pub fn providers_for_model(model: &str) -> Vec<String> {
    let model = model.trim();
    if model.is_empty() {
        return Vec::new();
    }
    let manifests: Vec<_> = all_kinds()
        .into_iter()
        .map(|kind| (kind, manifest_for(kind)))
        .collect();
    let exact = manifests.iter().any(|(_, items)| {
        items
            .iter()
            .any(|item| model_id(item).eq_ignore_ascii_case(model))
    });
    let mut result: Vec<_> = manifests
        .into_iter()
        .filter(|(_, items)| {
        if exact {
                items
                    .iter()
                    .any(|item| model_id(item).eq_ignore_ascii_case(model))
        } else {
            entry_id_in_manifest(items, model).is_some()
        }
        })
        .map(|(kind, _)| kind_id(kind).to_string())
        .collect();
    result.extend(custom_providers::list().into_iter().filter_map(|provider| {
        let models = provider.get("models")?.as_array()?;
        models
            .iter()
            .any(|entry| model_id(entry).eq_ignore_ascii_case(model))
            .then(|| {
                provider
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
    }));
    result
}

/// 只返回有开启绑定的提供商，不把关闭的 ID 回落给默认提供商。
pub fn forwarding_providers(model: &str) -> Vec<String> {
    let rules = model_rules::current();
    let mut result: Vec<_> = all_kinds()
        .into_iter()
        .filter(|kind| builtin_target(&rules, *kind, &manifest_for(*kind), model).is_some())
        .map(|kind| kind_id(kind).to_string())
        .collect();
    result.extend(custom_providers::carriers_of_model(model));
    result
}

pub fn model_blocked_everywhere(model: &str) -> bool {
    if !forwarding_providers(model).is_empty() {
        return false;
    }
    !providers_for_model(model).is_empty()
        || model_rules::current().has_alias(model)
        || custom_providers::list().iter().any(|provider| {
            provider
                .get("mappings")
                .and_then(Value::as_array)
                .is_some_and(|mappings| {
                    mappings.iter().any(|mapping| {
                        mapping
                            .get("alias")
                            .and_then(Value::as_str)
                            .is_some_and(|alias| alias.eq_ignore_ascii_case(model))
                    })
            })
        })
}

pub fn wire_target_for_provider(
    model: &str,
    provider_id: &str,
    _account: Option<&Value>,
) -> WireTarget {
    let requested = model.trim();
    // 自定义提供商在自己的转发模块解析一次，避免把别名提前改写后丢失其等级绑定。
    let resolved = kind_from_id(provider_id).and_then(|kind| {
        builtin_target(
            &model_rules::current(),
            kind,
            &manifest_for(kind),
            requested,
        )
    });
    resolved.unwrap_or_else(|| WireTarget {
        model: requested.to_string(),
        reasoning: None,
    })
}

pub fn default_model_catalog() -> Vec<Value> {
    let rules = model_rules::current();
    let mut result = Vec::new();
    for kind in all_kinds()
        .into_iter()
        .filter(|kind| adapter_for(*kind).supports_default_model())
    {
        for entry in manifest_for(kind) {
            let id = model_id(&entry);
            if !id.is_empty()
                && rules.default_enabled(kind_id(kind), &id)
                && !result
                    .iter()
                    .any(|item| model_id(item).eq_ignore_ascii_case(&id))
            {
                result.push(entry);
            }
        }
    }
    result
}

pub fn default_model_usable(model: &str) -> bool {
    entry_id_in_manifest(&default_model_catalog(), model).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_test_target_keeps_disabled_default_and_does_not_follow_same_name_alias() {
        let raw = json!({"modelRules":{"disabled":[{"provider":"qoder","id":"Raw-A"}],"mappings":[
            {"provider":"qoder","alias":"Raw-A","target":"Raw-A","enabled":false,"reasoning":"high"}
        ]}});
        let mut rules = model_rules::ModelRules::from_raw(raw.as_object().unwrap());
        let manifest = vec![json!({"id":"Raw-A"}), json!({"id":"Other"})];
        assert!(builtin_target(&rules, ProviderKind::Qoder, &manifest, "Raw-A").is_none());
        let expected = WireTarget { model: "Raw-A".into(), reasoning: Some("high".into()) };
        assert_eq!(default_target(&rules, "qoder", &manifest, "raw-a"), Some(expected.clone()));
        assert!(default_target(&rules, "qoder", &manifest, "absent").is_none());
        let alias = json!({"modelRules":{"mappings":[{"provider":"qoder","alias":"Raw-A","target":"Other","enabled":true}]}});
        rules.mappings.extend(model_rules::ModelRules::from_raw(alias.as_object().unwrap()).mappings);
        assert_eq!(builtin_target(&rules, ProviderKind::Qoder, &manifest, "Raw-A").unwrap().model, "Other");
        assert_eq!(default_target(&rules, "qoder", &manifest, "Raw-A"), Some(expected));
    }
}
