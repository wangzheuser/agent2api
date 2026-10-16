//! GET/PUT /api/routing + GET /api/routing/queue —— 账号路由的设置与**队列可见化**。
//!
//! ── /api/routing（与 /api/timeouts 同一模式）─────────────────
//! 独立端点、允许部分字段、返回生效后的全量值、无副作用（不重启进程）。
//! 保存后对下一个请求立即生效（转发层逐请求读 `config::routing_settings()`
//! 内存快照，不读盘）。
//!
//! ── /api/routing/queue（队列可见化，账号页面板的数据源）────────
//! 回答「按当前策略，这个模型的候选账号会怎么排、谁被跳过、为什么」。
//! 候选过滤与转发选路共用 `routing` 的账号判定；本地转发实现没有上游
//! `PlanMeta` 队列快照，因此这里只做无副作用的候选预览。
//!
//! 与 `/api/session` 的 `routedAccountId` 的关系：那个字段只回答「队首是谁」
//! （★ 的数据源，保持不变），本端点给的是**整张清单 + 原因句** —— 面板要
//! 展示每一位，顺带把被跳过者也摆出来（禁用 / 冷却 / 并发满 / 限制器 /
//! 无凭证各有各的原因，用户不用再猜）。
//!
//! `model` 查询参数可省：省略（或空）时按「不限模型」的全局队列展开
//! （不收窄到「提供该模型的家」）—— 面板在没有模型上下文时也有得画。

use std::collections::{HashMap, HashSet};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config::{self, RoutingPatch, RoutingSettings};
use crate::server::core::providers::router;
use crate::server::core::routing;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::ServerState;

/// GET /api/routing
pub async fn get_routing(State(_state): State<ServerState>) -> Response {
    ok_json(routing_json(config::routing_settings()))
}

/// PUT /api/routing —— body 各键全部可选（不给 = 不动）
pub async fn put_routing(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let mut patch = RoutingPatch::default();
    if let Some(value) = object.get(config::KEY_ROUTING_STRATEGY) {
        let Some(text) = value.as_str() else {
            return errors::management_error(400, "strategy 必须是字符串");
        };
        let known = [
            config::ROUTING_STRATEGY_ORDER,
            config::ROUTING_STRATEGY_ROTATE,
            config::ROUTING_STRATEGY_PROVIDER,
        ];
        if !known.contains(&text) {
            return errors::management_error(
                400,
                format!("未知的路由策略「{text}」（只支持 {}）", known.join(" / ")),
            );
        }
        patch.strategy = Some(config::RouteStrategy::from_id(text));
    }
    if let Some(value) = object.get(config::KEY_ROUTING_STICKY) {
        let Some(text) = value.as_str() else {
            return errors::management_error(400, "sticky 必须是字符串");
        };
        let known = [
            config::STICKY_AUTO,
            config::STICKY_SESSION,
            config::STICKY_TURN,
            config::STICKY_OFF,
        ];
        if !known.contains(&text) {
            return errors::management_error(
                400,
                format!("未知的会话粘性档位「{text}」（只支持 {}）", known.join(" / ")),
            );
        }
        patch.sticky = Some(config::StickyMode::from_id(text));
    }
    let bool_field = |key: &str| -> Result<Option<bool>, Response> {
        match object.get(key) {
            None => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(errors::management_error(400, format!("{key} 必须是布尔值"))),
        }
    };
    let failover_backoff = match bool_field(config::KEY_ROUTING_FAILOVER_BACKOFF) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let sink = match bool_field(config::KEY_ROUTING_SINK) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let breaker_enabled = match bool_field(config::KEY_ROUTING_BREAKER_ENABLED) {
        Ok(value) => value,
        Err(response) => return response,
    };
    patch.failover_backoff = failover_backoff;
    patch.sink = sink;
    patch.breaker_enabled = breaker_enabled;
    let int_field = |key: &str, min: i64, max: i64| -> Result<Option<i64>, Response> {
        match object.get(key) {
            None => Ok(None),
            Some(value) => match value.as_i64() {
                Some(count) if (min..=max).contains(&count) => Ok(Some(count)),
                _ => Err(errors::management_error(
                    400,
                    format!("{key} 必须是 {min}~{max} 的整数"),
                )),
            },
        }
    };
    patch.breaker_threshold = match int_field(
        config::KEY_ROUTING_BREAKER_THRESHOLD,
        config::ROUTING_BREAKER_MIN_THRESHOLD,
        config::ROUTING_BREAKER_MAX_THRESHOLD,
    ) {
        Ok(value) => value,
        Err(response) => return response,
    };
    patch.breaker_timeout_seconds = match int_field(
        config::KEY_ROUTING_BREAKER_TIMEOUT,
        config::ROUTING_BREAKER_MIN_TIMEOUT,
        config::ROUTING_BREAKER_MAX_TIMEOUT,
    ) {
        Ok(value) => value,
        Err(response) => return response,
    };
    patch.breaker_success_threshold = match int_field(
        config::KEY_ROUTING_BREAKER_SUCCESS,
        config::ROUTING_BREAKER_MIN_SUCCESS,
        config::ROUTING_BREAKER_MAX_SUCCESS,
    ) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !config::set_routing(patch) {
        return errors::management_error(500, "路由设置落盘失败");
    }
    ok_json(routing_json(config::routing_settings()))
}

fn routing_json(settings: RoutingSettings) -> Value {
    json!({
        config::KEY_ROUTING_STRATEGY: settings.strategy.as_str(),
        config::KEY_ROUTING_STICKY: settings.sticky.as_str(),
        config::KEY_ROUTING_FAILOVER_BACKOFF: settings.failover_backoff,
        config::KEY_ROUTING_SINK: settings.sink,
        config::KEY_ROUTING_BREAKER_ENABLED: settings.breaker_enabled,
        config::KEY_ROUTING_BREAKER_THRESHOLD: settings.breaker_threshold,
        config::KEY_ROUTING_BREAKER_TIMEOUT: settings.breaker_timeout_seconds,
        config::KEY_ROUTING_BREAKER_SUCCESS: settings.breaker_success_threshold,
    })
}

/// GET /api/routing/queue?model=
pub async fn get_routing_queue(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let model = params
        .get("model")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let snapshot = config::current();
    let accounts = routing::accounts_of(&state.store().list_accounts());
    let providers: Vec<String> = match &model {
        Some(model) => router::route_for_forward(model),
        None => Vec::new(),
    };
    let scoped: Vec<Value> = match model.as_deref() {
        Some(_) if providers.is_empty() => accounts.clone(),
        Some(_) => accounts
            .iter()
            .filter(|account| providers.iter().any(|provider| provider == routing::provider_of(account)))
            .cloned()
            .collect(),
        None => accounts.clone(),
    };
    let now = crate::server::logging::now_ms();
    let counts: HashMap<String, usize> = state
        .upstream()
        .connections()
        .snapshot()
        .into_iter()
        .collect();
    let keys = routing::CooldownKeys::new(model.as_deref().unwrap_or(""));
    let settings = config::routing_settings();
    let facts = crate::server::core::usage_records::balance_facts();
    let token_facts = crate::server::core::limiter::token_facts();
    let limiter_blocked: HashSet<String> = scoped
        .iter()
        .filter(|account| {
            crate::server::core::usage_records::balance_blocked(account, &facts)
                || crate::server::core::limiter::token_skip_blocked(account, &token_facts, now)
        })
        .filter_map(|account| routing::account_id(account).map(str::to_string))
        .collect();
    let candidates: Vec<Value> = scoped
        .iter()
        .filter(|account| {
            routing::account_id(account)
                .map(|id| !limiter_blocked.contains(id))
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    let picked = routing::pick_account_peek(&candidates, &keys, &counts, &[], now);
    let picked_id = picked
        .as_ref()
        .and_then(|account| routing::account_id(account))
        .unwrap_or("")
        .to_string();
    let entry_json = |account: &Value, state: &str, reason: &str| {
        let id = routing::account_id(account).unwrap_or("");
        let provider = routing::provider_of(account).to_string();
        let provider_label = crate::server::core::providers::kind_from_id(&provider)
            .map(|kind| crate::server::core::providers::meta(kind).label.to_string())
            .unwrap_or_else(|| provider.clone());
        let reset_at = routing::rate_limit_reset_at(account, &keys, now);
        json!({
            "id": id,
            "name": routing::account_name(account, id),
            "provider": provider,
            "providerLabel": provider_label,
            "state": state,
            "reason": reason,
            "resetAt": if reset_at > 0 { Value::from(reset_at) } else { Value::Null },
            "priority": account.get("priority").cloned().unwrap_or(Value::Null),
        })
    };
    let mut ordered = Vec::new();
    let mut blocked = Vec::new();
    for account in &scoped {
        let Some(id) = routing::account_id(account) else { continue };
        if limiter_blocked.contains(id) {
            blocked.push(entry_json(account, "blocked", "限制器拦截"));
            continue;
        }
        let usability = routing::account_usability(account, &keys, now);
        if !usability.usable {
            blocked.push(entry_json(account, "blocked", usability.reason.unwrap_or("不可用")));
            continue;
        }
        let max = routing::max_concurrent_of(account);
        let busy = max > 0 && counts.get(id).copied().unwrap_or(0) as u64 >= max;
        if busy {
            blocked.push(entry_json(account, "blocked", "并发已满"));
        } else {
            ordered.push(entry_json(
                account,
                if id == picked_id { "picked" } else { "available" },
                if id == picked_id { "当前选中" } else { "候选" },
            ));
        }
    }
    ok_json(json!({
        "strategy": settings.strategy.as_str(),
        "sticky": settings.sticky.as_str(),
        "model": model.clone().map(Value::String).unwrap_or(Value::Null),
        "lastRequestModel": snapshot.last_request_model(),
        "pickedId": picked_id,
        "ordered": ordered,
        "blocked": blocked,
    }))
}
