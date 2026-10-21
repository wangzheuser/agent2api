//! WorkBuddy 福利偏好与身份；旧积分保底仅供启动迁移，选路由通用限制器负责。
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::{endpoints, task_state};
use super::providers::workbuddy::region::Region;

static WRITES: Mutex<()> = Mutex::new(());

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// 接受公开账号或会话形态；地区归一与既有登录/计费共用。
pub fn identity(value: &Value) -> Option<String> {
    let provider = value.get("provider").and_then(Value::as_str).unwrap_or("workbuddy");
    let region = Region::from_provider_id(provider)?;
    let account = value.get("account").unwrap_or(value);
    let uid = string(account, "uid");
    if uid.is_empty() {
        return None;
    }
    let edition = if region == Region::Intl { region.id() } else { endpoints::resolve_edition(value.get("edition").and_then(Value::as_str)).id };
    let bytes =
        serde_json::to_vec(&["workbuddy", edition, uid, string(account, "enterpriseId")]).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

fn stored(key: &str) -> Value {
    task_state::read(key)
        .ok()
        .and_then(|state| state.value)
        .unwrap_or(Value::Null)
}

fn key(prefix: &str, identity: &str) -> String {
    format!("workbuddy:{prefix}:{identity}")
}

fn defaults() -> Value {
    json!({"autoGrowth":false,"autoTravel":false})
}

pub fn account_policy(id: &str, identity: &str) -> Value {
    let prefs = stored(&key("policy", identity));
    let mut result = defaults();
    for field in ["autoGrowth", "autoTravel"] {
        if let Some(value) = prefs.get(field) {
            result[field] = value.clone();
        }
    }
    result["schemaVersion"] = json!(1);
    result["id"] = json!(id);
    result["identity"] = json!(identity);
    result
}

/// 读取仍保留在任务状态里的旧 WorkBuddy 保底配置，供账号启动迁移使用。
/// 旧 KV 不删除：迁移失败或回滚时仍有审计来源。
pub(crate) fn legacy_credit_floor(account: &Value, states: &HashMap<String, task_state::TaskState>) -> Option<f64> {
    let identity = identity(account)?;
    let prefs = states.get(&key("policy", &identity))?.value.as_ref()?;
    amount(&prefs["creditFloor"]).filter(|floor| *floor <= 1_000_000_000.0)
}

pub fn patch_policy(id: &str, identity: &str, patch: &Value) -> Result<Value, String> {
    let object = patch.as_object().ok_or("策略参数必须是对象")?;
    for (field, value) in object {
        match field.as_str() {
            "autoGrowth" | "autoTravel" if value.is_boolean() => {}
            "creditFloor" => return Err("积分保底已迁移到账号设置的「限制器」，请刷新页面后设置".into()),
            "selection" => return Err("WorkBuddy 专属选号已移除，请使用网关的「账号选路」".into()),
            _ => return Err(format!("无效的策略字段或值：{field}")),
        }
    }
    {
        let _guard = WRITES.lock().map_err(|_| "策略写入锁不可用")?;
        let mut prefs = stored(&key("policy", identity));
        if !prefs.is_object() {
            prefs = defaults();
        }
        for field in ["autoGrowth", "autoTravel"] {
            if let Some(value) = object.get(field) {
                prefs[field] = value.clone();
            }
        }
        task_state::store_value(&key("policy", identity), prefs)?;
    }
    Ok(account_policy(id, identity))
}

fn amount(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_floor_preserves_fraction_zero_and_identity() {
        let account = json!({"uid":"a","edition":"cn","provider":"workbuddy"});
        let account_identity = identity(&account).unwrap();
        assert_eq!(account_identity.len(), 64);
        assert_eq!(identity(&json!({"account":{"uid":"a"},"edition":"cn"})), Some(account_identity.clone()));
        let mut states = HashMap::new();
        for floor in [json!(0), json!(0.012345), json!(10.5), Value::Null, json!(-1), json!("0")] {
            let mut state = task_state::TaskState::default();
            state.value = Some(json!({"creditFloor":floor}));
            states.insert(key("policy", &account_identity), state);
            assert_eq!(legacy_credit_floor(&account, &states), amount(&floor));
            assert_eq!(legacy_credit_floor(&json!({"uid":"a","edition":"intl"}), &states), None);
        }
        assert!(patch_policy("a", &account_identity, &json!({"creditFloor":0})).is_err());
        assert!(patch_policy("a", &account_identity, &json!({"selection":"cost"})).is_err());
    }
}

#[cfg(test)]
mod region_restore_tests {
    use super::*;
    #[test]
    fn migrated_international_identity_retains_policy_and_does_not_collide_with_cn() {
        let legacy = json!({"uid":"fixture", "edition":"intl", "provider":"workbuddy"});
        let migrated = json!({"uid":"fixture", "edition":"intl", "provider":"workbuddy-intl"});
        assert_eq!(identity(&legacy), identity(&migrated));
        assert_eq!(identity(&migrated), identity(&json!({"uid":"fixture","provider":"workbuddy-intl"})));
        assert_ne!(identity(&migrated), identity(&json!({"uid":"fixture","provider":"workbuddy"})));
        assert_eq!(identity(&json!({"uid":"fixture","provider":"zcode"})), None);
    }
}
