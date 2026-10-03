//! 成长福利管理入口；账号身份、互斥和上游调用均在 core。
use crate::server::{
    core::workbuddy_growth,
    errors::management_error,
    http::{ok_json, parse_body, query_param},
    ServerState,
};
use axum::{body::Bytes, response::Response};

pub async fn state(state: &ServerState, query: &str) -> Response {
    let Some(id) = query_param(query, "id").filter(|id| !id.is_empty()) else {
        return management_error(400, "缺少账号 id");
    };
    match workbuddy_growth::state(state.store(), &id).await {
        Ok(value) => ok_json(value),
        Err(error) => management_error(error.status, error.message),
    }
}
pub async fn action(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) if value.is_object() => value,
        _ => return management_error(400, "成长动作请求不是有效 JSON 对象"),
    };
    match workbuddy_growth::action(state.store().clone(), payload).await {
        Ok(value) => ok_json(value),
        Err(error) => management_error(error.status, error.message),
    }
}

pub fn policy(state: &ServerState, query: &str) -> Response {
    let Some(id) = query_param(query, "id").filter(|id| !id.is_empty()) else {
        return management_error(400, "缺少账号 id");
    };
    match workbuddy_growth::target(state.store(), &id) {
        Ok(target) => ok_json(crate::server::core::workbuddy_policy::account_policy(
            &id,
            &target.identity,
        )),
        Err(error) => management_error(error.status, error.message),
    }
}
pub fn patch_policy(state: &ServerState, query: &str, body: &Bytes) -> Response {
    let Some(id) = query_param(query, "id").filter(|id| !id.is_empty()) else {
        return management_error(400, "缺少账号 id");
    };
    let mut payload = match parse_body(body) {
        Ok(value) if value.is_object() => value,
        _ => return management_error(400, "策略请求不是有效 JSON 对象"),
    };
    match workbuddy_growth::target(state.store(), &id) {
        Ok(target) => {
            if payload["expectedIdentity"].as_str() != Some(target.identity.as_str()) {
                return management_error(409, "账号身份已变化，请刷新后重新设置");
            }
            if let Some(object) = payload.as_object_mut() {
                object.remove("expectedIdentity");
            }
            match crate::server::core::workbuddy_policy::patch_policy(
                &id,
                &target.identity,
                &payload,
            ) {
                Ok(value) => ok_json(value),
                Err(error) => management_error(400, error),
            }
        }
        Err(error) => management_error(error.status, error.message),
    }
}
