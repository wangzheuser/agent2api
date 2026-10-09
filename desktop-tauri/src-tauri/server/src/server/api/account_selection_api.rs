//! GET/PUT /api/account-selection —— 账号选路策略。
//!
//! `balanced` 是默认策略：优先选择有效并发负载最低的账号，并在负载相同
//! 时轮换平局账号；`priority` 保留主备顺序；`roundRobin` 按可用账号轮询。
//! 配置写入运行期快照与统一配置库，保存后下一个请求立即使用新策略。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config::{self, AccountSelectionStrategy, KEY_ACCOUNT_SELECTION};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/account-selection
pub async fn get_account_selection(State(_state): State<ServerState>) -> Response {
    ok_json(selection_json(config::account_selection()))
}

/// PUT /api/account-selection —— body `{accountSelection: "balanced"|"priority"|"roundRobin"|"cacheAffinity"}`
pub async fn put_account_selection(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let Some(value) = object.get(KEY_ACCOUNT_SELECTION) else {
        return errors::management_error(400, format!("缺少 {KEY_ACCOUNT_SELECTION} 字段"));
    };
    let Some(raw) = value.as_str() else {
        return errors::management_error(400, format!("{KEY_ACCOUNT_SELECTION} 必须是字符串"));
    };
    let strategy = match raw {
        "balanced" => AccountSelectionStrategy::Balanced,
        "priority" => AccountSelectionStrategy::Priority,
        "roundRobin" => AccountSelectionStrategy::RoundRobin,
        "cacheAffinity" => AccountSelectionStrategy::CacheAffinity,
        _ => {
            return errors::management_error(
                400,
                format!(
                    "{KEY_ACCOUNT_SELECTION} 必须是 balanced、priority、roundRobin 或 cacheAffinity（收到: {raw}）"
                ),
            )
        }
    };
    let previous = config::account_selection();
    if !config::set_account_selection(strategy) {
        logging::log(
            "[Config]",
            "⚠️ 账号选路策略写入配置库失败，本次运行内仍立即生效",
        );
    }
    if previous != strategy {
        state.upstream().reset_affinity();
    }
    logging::log(
        "[Config]",
        &format!("账号选路策略已更新: {}", strategy.as_str()),
    );
    ok_json(selection_json(strategy))
}

fn selection_json(strategy: AccountSelectionStrategy) -> Value {
    json!({ KEY_ACCOUNT_SELECTION: strategy.as_str() })
}
