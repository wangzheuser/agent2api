//! WorkBuddy 福利偏好、积分保底与候选顺序；默认完全保留主备优先级。
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::upstream::usage::TelemetrySnapshot;
use super::{endpoints, routing, task_state};
use super::providers::workbuddy::region::Region;
use crate::server::logging;

pub const BALANCE_MAX_AGE_MS: i64 = 900_000;
const COST_MAX_AGE_MS: i64 = 86_400_000;
const MIN_COST_SAMPLES: usize = 3;
const SELECTION_KEY: &str = "workbuddySelection";
// ponytail: 低频偏好/余额和请求收尾共用一把写锁；实测成为瓶颈时再分账号。
static WRITES: Mutex<()> = Mutex::new(());
static ROUTE_CACHE: Mutex<Option<(i64, Arc<HashMap<String, task_state::TaskState>>)>> =
    Mutex::new(None);

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
    json!({"autoGrowth":false,"autoTravel":false,"creditFloor":null})
}

pub fn account_policy(id: &str, identity: &str) -> Value {
    let prefs = stored(&key("policy", identity));
    let mut result = defaults();
    for field in ["autoGrowth", "autoTravel", "creditFloor"] {
        if let Some(value) = prefs.get(field) {
            result[field] = value.clone();
        }
    }
    result["schemaVersion"] = json!(1);
    result["id"] = json!(id);
    result["identity"] = json!(identity);
    result["selection"] = json!(selection(&stored(SELECTION_KEY)));
    result["selectionScope"] = json!("provider");
    result["balanceMaxAgeSeconds"] = json!(BALANCE_MAX_AGE_MS / 1000);
    result["cost"] = cost_summary(&stored(&key("cost", identity)), logging::now_ms());
    result
}

pub fn patch_policy(id: &str, identity: &str, patch: &Value) -> Result<Value, String> {
    let object = patch.as_object().ok_or("策略参数必须是对象")?;
    for (field, value) in object {
        match field.as_str() {
            "autoGrowth" | "autoTravel" if value.is_boolean() => {}
            "creditFloor"
                if value.is_null()
                    || amount(value).is_some_and(|number| number <= 1_000_000_000.0) => {}
            "selection" if matches!(value.as_str(), Some("priority" | "expiry" | "cost")) => {}
            _ => return Err(format!("无效的策略字段或值：{field}")),
        }
    }
    {
        let _guard = WRITES.lock().map_err(|_| "策略写入锁不可用")?;
        let mut prefs = stored(&key("policy", identity));
        if !prefs.is_object() {
            prefs = defaults();
        }
        for field in ["autoGrowth", "autoTravel", "creditFloor"] {
            if let Some(value) = object.get(field) {
                prefs[field] = value.clone();
            }
        }
        task_state::store_value(&key("policy", identity), prefs)?;
        if let Some(value) = object.get("selection") {
            task_state::store_value(SELECTION_KEY, value.clone())?;
        }
    }
    invalidate_routes();
    Ok(account_policy(id, identity))
}

fn selection(value: &Value) -> &str {
    match value.as_str() {
        Some("expiry") => "expiry",
        Some("cost") => "cost",
        _ => "priority",
    }
}

fn amount(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
}

/// 从刚完成的账号余额查询写入身份绑定快照；错误/不完整结果也覆盖旧成功，防止继续使用旧余额。
pub fn observe_usage(account: &Value, usage: &Value) {
    let Some(identity) = identity(account) else {
        return;
    };
    let details = usage.get("creditDetails").cloned().unwrap_or(Value::Null);
    let Ok(_guard) = WRITES.lock() else {
        return;
    };
    let result = task_state::store_value(&key("balance", &identity), json!({"details":details}));
    invalidate_routes();
    if let Err(error) = result {
        logging::verbose("[WorkBuddyPolicy]", &format!("余额观察保存失败：{error}"));
    }
}

/// 单次最终尝试的上游实扣；不用余额差估算，也不累计重复 SSE usage 帧。
pub fn observe_cost(snapshot: &TelemetrySnapshot, success: bool) {
    if snapshot.provider.as_deref().and_then(Region::from_provider_id).is_none() {
        return;
    }
    let (Some(identity), Some(credits)) = (
        snapshot.workbuddy_identity.as_deref(),
        snapshot.upstream_credits,
    ) else {
        return;
    };
    let now = logging::now_ms();
    let Ok(_guard) = WRITES.lock() else {
        return;
    };
    let state_key = key("cost", identity);
    let mut cost = stored(&state_key);
    if !cost.is_object() {
        cost = json!({"models":{}});
    }
    cost["lastCredits"] = json!(credits);
    cost["lastAt"] = json!(now);
    if success && !snapshot.upstream_model.is_empty() {
        if let Some(tokens) = snapshot.workbuddy_cost_tokens.filter(|value| *value > 0) {
            let model = &snapshot.upstream_model;
            if !cost["models"].is_object() {
                cost["models"] = json!({});
            }
            // 每模型最多 30 个近一天样本，每账号最多 32 个模型；只用于选号参考。
            let mut samples = valid_samples(&cost["models"][model], now);
            samples.push(json!({"at":now,"credits":credits,"tokens":tokens}));
            if samples.len() > 30 {
                samples.remove(0);
            }
            cost["models"][model] = json!(samples);
            if let Some(models) = cost["models"].as_object_mut() {
                while models.len() > 32 {
                    let oldest = models
                        .iter()
                        .filter(|(name, _)| name.as_str() != model)
                        .min_by_key(|(_, samples)| {
                            samples
                                .as_array()
                                .and_then(|items| items.last())
                                .and_then(|item| item["at"].as_i64())
                                .unwrap_or(0)
                        })
                        .map(|(name, _)| name.clone());
                    if let Some(name) = oldest {
                        models.remove(&name);
                    } else {
                        break;
                    }
                }
            }
        }
    }
    if let Err(error) = task_state::store_value(&state_key, cost) {
        logging::verbose("[WorkBuddyPolicy]", &format!("实扣样本保存失败：{error}"));
    }
}

fn valid_samples(value: &Value, now: i64) -> Vec<Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            let at = item["at"].as_i64().unwrap_or(0);
            at > 0
                && at <= now
                && now - at <= COST_MAX_AGE_MS
                && amount(&item["credits"]).is_some()
                && item["tokens"].as_i64().is_some_and(|tokens| tokens > 0)
        })
        .cloned()
        .collect()
}

fn cost_summary(cost: &Value, now: i64) -> Value {
    let mut models = Vec::new();
    if let Some(entries) = cost["models"].as_object() {
        for (model, values) in entries {
            let samples = valid_samples(values, now);
            if samples.is_empty() {
                continue;
            }
            let credits: f64 = samples
                .iter()
                .filter_map(|item| amount(&item["credits"]))
                .sum();
            let tokens: f64 = samples
                .iter()
                .filter_map(|item| item["tokens"].as_f64())
                .sum();
            models.push(json!({"model":model,"creditsPer1kTokens":credits / tokens * 1000.0,"samples":samples.len(),"lastAt":samples.last().and_then(|item| item.get("at"))}));
        }
    }
    json!({"lastCredits":cost.get("lastCredits"),"lastAt":cost.get("lastAt"),"samples":models.iter().filter_map(|item| item["samples"].as_u64()).sum::<u64>(),"models":models})
}

fn fresh_details<'a>(state: &'a Value, now: i64) -> Option<&'a Value> {
    let details = state.get("details")?;
    let at = details["fetchedAt"].as_i64()?;
    if details["complete"] != true
        || details["kind"] != "personal"
        || at <= 0
        || at > now
        || now - at > BALANCE_MAX_AGE_MS
    {
        return None;
    }
    // 快照生成后任何正余额段到期都使总额过时，等待现有余额任务刷新。
    if details["segments"].as_array()?.iter().any(|segment| {
        amount(&segment["remaining"]).is_some_and(|number| number > 0.0)
            && segment["expiresAt"].as_i64().is_some_and(|at| at <= now)
    }) {
        return None;
    }
    amount(&details["remaining"])?;
    Some(details)
}

fn invalidate_routes() {
    if let Ok(mut cache) = ROUTE_CACHE.lock() {
        *cache = None;
    }
}

/// 一次选择只读一次现有 KV 集合，保底与排序使用同一快照。
pub struct RoutePolicy {
    states: Arc<HashMap<String, task_state::TaskState>>,
}

impl RoutePolicy {
    pub fn load() -> Self {
        let now = logging::now_ms();
        let Ok(mut cache) = ROUTE_CACHE.lock() else {
            return Self {
                states: Arc::new(HashMap::new()),
            };
        };
        if let Some((at, states)) = cache.as_ref() {
            if now >= *at && now - *at < 2_000 {
                return Self {
                    states: states.clone(),
                };
            }
        }
        match task_state::read_all() {
            Ok(states) => {
                let states: Arc<HashMap<String, task_state::TaskState>> = Arc::new(
                    states
                        .into_iter()
                        .filter(|(key, _)| {
                            ["workbuddy:policy:", "workbuddy:balance:", "workbuddy:cost:"]
                                .iter()
                                .any(|prefix| key.starts_with(prefix))
                                || key == SELECTION_KEY
                        })
                        .collect(),
                );
                *cache = Some((now, states.clone()));
                Self { states }
            }
            Err(_) => Self {
                states: cache
                    .as_ref()
                    .map(|(_, states)| states.clone())
                    .unwrap_or_default(),
            },
        }
    }
    fn value(&self, key: &str) -> &Value {
        self.states
            .get(key)
            .and_then(|state| state.value.as_ref())
            .unwrap_or(&Value::Null)
    }
    pub fn blocked_reason(&self, account: &Value, _model: &str, now: i64) -> Option<&'static str> {
        let identity = identity(account)?;
        let floor = amount(&self.value(&key("policy", &identity))["creditFloor"])?;
        // 全局模型目录缺少地区/账号计费归属，暂不作免费豁免，未知模型按收费处理。
        let Some(details) = fresh_details(self.value(&key("balance", &identity)), now) else {
            return Some("balance-stale");
        };
        if amount(&details["remaining"]).unwrap_or(0.0) <= floor {
            Some("credit-floor")
        } else {
            None
        }
    }
    pub fn reorder(&self, candidates: &mut [Value], model: &str, now: i64) {
        let mode = selection(self.value(SELECTION_KEY));
        if mode == "priority" {
            return;
        }
        for edition in ["cn", "intl"] {
            // 无可靠指标的账号保留原槽位；其他 provider 以及地区之间也不互换。
            let mut scored: Vec<(usize, f64, Value)> = candidates
                .iter()
                .enumerate()
                .filter_map(|(index, account)| {
                    if Region::from_provider_id(routing::provider_of(account)).is_none()
                        || string(account, "edition") != edition
                    {
                        return None;
                    }
                    let identity = identity(account)?;
                    let score = if mode == "expiry" {
                        let details = fresh_details(self.value(&key("balance", &identity)), now)?;
                        details["segments"]
                            .as_array()?
                            .iter()
                            .filter(|item| {
                                amount(&item["remaining"]).is_some_and(|value| value > 0.0)
                            })
                            .filter_map(|item| item["expiresAt"].as_i64())
                            .filter(|at| *at > now)
                            .min()? as f64
                    } else {
                        let samples = valid_samples(
                            &self.value(&key("cost", &identity))["models"][model],
                            now,
                        );
                        if samples.len() < MIN_COST_SAMPLES {
                            return None;
                        }
                        let credit: f64 = samples
                            .iter()
                            .filter_map(|item| amount(&item["credits"]))
                            .sum();
                        let tokens: f64 = samples
                            .iter()
                            .filter_map(|item| item["tokens"].as_f64())
                            .sum();
                        credit / tokens
                    };
                    Some((index, score, account.clone()))
                })
                .collect();
            let slots: Vec<usize> = scored.iter().map(|entry| entry.0).collect();
            scored.sort_by(|left, right| left.1.total_cmp(&right.1));
            for (slot, (_, _, account)) in slots.into_iter().zip(scored) {
                candidates[slot] = account;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(entries: Vec<(&str, Value)>) -> RoutePolicy {
        RoutePolicy {
            states: Arc::new(
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        (key.to_string(), {
                            let mut state = task_state::TaskState::default();
                            state.value = Some(value);
                            state
                        })
                    })
                    .collect(),
            ),
        }
    }
    fn account(id: &str) -> Value {
        json!({"id":id,"uid":id,"edition":"cn","provider":"workbuddy"})
    }
    fn balance(remaining: f64, now: i64, expiry: i64) -> Value {
        json!({"details":{"complete":true,"kind":"personal","fetchedAt":now,"remaining":remaining,"segments":[{"remaining":remaining,"expiresAt":expiry}]}})
    }
    #[test]
    fn floor_uses_fresh_complete_identity_and_preserves_zero() {
        let now = 1_000_000;
        let a = account("a");
        let identity = identity(&a).unwrap();
        assert_eq!(identity.len(), 64);
        assert_ne!(
            Some(identity.clone()),
            super::identity(&json!({"uid":"a","edition":"intl"}))
        );
        assert_eq!(
            Some(identity.clone()),
            super::identity(&json!({"account":{"uid":"a"},"edition":"cn"}))
        );
        let mut p = policy(vec![
            (&key("policy", &identity), json!({"creditFloor":0})),
            (&key("balance", &identity), balance(0.0, now, now + 1)),
        ]);
        assert_eq!(p.blocked_reason(&a, "unknown", now), Some("credit-floor"));
        Arc::make_mut(&mut p.states)
            .get_mut(&key("balance", &identity))
            .unwrap()
            .value = Some(balance(0.1, now, now + 1));
        assert_eq!(p.blocked_reason(&a, "unknown", now), None);
        assert_eq!(
            p.blocked_reason(&a, "unknown", now + 1),
            Some("balance-stale")
        );
        assert_eq!(
            p.blocked_reason(&json!({"uid":"other","edition":"cn"}), "unknown", now),
            None
        );
        assert_eq!(amount(&json!("0")), None);
        assert_eq!(amount(&json!(-1)), None);
    }
    #[test]
    fn expiry_reorders_only_known_same_region_slots() {
        let now = 1_000_000;
        let a = account("a");
        let b = account("b");
        let other = json!({"id":"other","provider":"raccoon"});
        let unknown = account("unknown");
        let mut candidates = vec![a.clone(), other.clone(), unknown.clone(), b.clone()];
        let p = policy(vec![
            (SELECTION_KEY, json!("expiry")),
            (
                &key("balance", &identity(&a).unwrap()),
                balance(5.0, now, now + 200),
            ),
            (
                &key("balance", &identity(&b).unwrap()),
                balance(5.0, now, now + 100),
            ),
        ]);
        p.reorder(&mut candidates, "m", now);
        assert_eq!(candidates, vec![b, other, unknown, a]);
    }
    #[test]
    fn cost_requires_real_model_samples_and_respects_missing() {
        let now = 1_000_000;
        let a = account("a");
        let b = account("b");
        let samples = |credit| json!({"models":{"m":[{"at":now,"credits":credit,"tokens":100},{"at":now,"credits":credit,"tokens":100},{"at":now,"credits":credit,"tokens":100}]}});
        let p = policy(vec![
            (SELECTION_KEY, json!("cost")),
            (&key("cost", &identity(&a).unwrap()), samples(1.0)),
            (&key("cost", &identity(&b).unwrap()), samples(0.0)),
        ]);
        let mut candidates = vec![a.clone(), b.clone()];
        p.reorder(&mut candidates, "unknown", now);
        assert_eq!(candidates, vec![a.clone(), b.clone()]);
        p.reorder(&mut candidates, "m", now);
        assert_eq!(candidates, vec![b, a]);
        assert_eq!(
            cost_summary(&samples(0.0), now)["models"][0]["creditsPer1kTokens"].as_f64(),
            Some(0.0)
        );
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
