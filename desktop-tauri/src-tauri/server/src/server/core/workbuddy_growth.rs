//! 成长福利的账号边界、持久互斥和短动作。客户端断线不取消已发出的业务操作。

use super::{
    account_store::AccountStore,
    billing::growth::{self, Client, GrowthError},
    task_state::{self, Claim, ManualBackoff},
    usage_query, workbuddy_policy,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::future::Future;

mod operations;
use operations::run;

const INTERVAL: i64 = 60 * 60_000;
pub struct GrowthTarget {
    pub account: Value,
    pub session: Value,
    pub identity: String,
    pub supported: bool,
}
pub fn target(store: &AccountStore, id: &str) -> Result<GrowthTarget, GrowthError> {
    let account = store
        .list_accounts()
        .get("accounts")
        .and_then(Value::as_array)
        .and_then(|rows| rows.iter().find(|a| a["id"].as_str() == Some(id)))
        .cloned()
        .ok_or_else(|| GrowthError::new(404, "账号不存在"))?;
    if account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("workbuddy")
        != "workbuddy"
    {
        return Err(GrowthError::new(400, "该账号不是 WorkBuddy 账号"));
    }
    let identity = workbuddy_policy::identity(&account)
        .ok_or_else(|| GrowthError::new(400, "账号身份不完整"))?;
    let entry = store
        .get_session_by_id(id)
        .ok_or_else(|| GrowthError::new(401, "账号没有可用登录态"))?;
    let mut session = entry.session;
    session["proxy"] = entry.proxy;
    session["proxyError"] = json!(entry.proxy_error);
    if workbuddy_policy::identity(&session).as_deref() != Some(identity.as_str()) {
        return Err(GrowthError::new(409, "账号身份已变化，请刷新后重试"));
    }
    let supported = personal_cn(&account) && personal_cn(&session);
    Ok(GrowthTarget {
        account,
        session,
        identity,
        supported,
    })
}
fn personal_cn(value: &Value) -> bool {
    let account = value.get("account").unwrap_or(value);
    let edition = value.get("edition").and_then(Value::as_str).unwrap_or("cn");
    edition == "cn"
        && ["enterpriseId", "tenantId", "tenant_id", "tenant"]
            .iter()
            .all(|key| account.get(*key).map_or(true, |v| v.is_null() || v == ""))
        && ["type", "accountType"].iter().all(|key| {
            account
                .get(*key)
                .map_or(true, |v| v.is_null() || v == "" || v == "personal")
        })
}
fn key(identity: &str) -> String {
    format!("workbuddyGrowth:{identity}")
}
fn operation_result(id: &str, action: &str, status: &str, message: &str) -> Value {
    json!({"id":id,"action":action,"at":crate::server::logging::now_ms(),"status":status,"message":message,
        "rewards":growth::rewards(&Value::Null),"receiptConfirmed":false,"stateConfirmed":false,
        "balanceBefore":null,"balanceAfter":null,"balanceDelta":null,"items":[],"state":null})
}
pub async fn state(store: &AccountStore, id: &str) -> Result<Value, GrowthError> {
    let target = target(store, id)?;
    if !target.supported {
        return Ok(
            json!({"schemaVersion":1,"id":id,"identity":target.identity,"fetchedAt":crate::server::logging::now_ms(),"supported":false,"reason":"国内个人成长福利不适用于该账号；国际日活沿用原签到功能","edition":target.account.get("edition"),"tasks":[],"capabilities":[],"errors":[],"travel":null,"streak":null,"lottery":null,"activities":[],"lastRun":null,"pending":null,"running":false}),
        );
    }
    let client = Client::new(store.clone(), id, &target.identity);
    let mut state = client.state().await?;
    let persisted =
        task_state::read(&key(&target.identity)).map_err(|e| GrowthError::new(500, e))?;
    state["running"] = json!(persisted.running());
    if let Some(value) = persisted.value {
        state["lastRun"] = value.get("lastRun").cloned().unwrap_or(Value::Null);
        state["pending"] = value.get("pending").cloned().unwrap_or(Value::Null);
        if let Some(pending) = state["pending"].as_object_mut() {
            pending.remove("token");
        }
    }
    Ok(state)
}

/// 与服务端调度共用，JoinHandle 被 HTTP 取消丢弃后工作仍由运行时持有。
pub async fn action(store: AccountStore, body: Value) -> Result<Value, GrowthError> {
    validate_action(&body)?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GrowthError::new(400, "缺少账号 id"))?
        .to_string();
    let action = body.get("action").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        action,
        "accept_task"
            | "claim_task"
            | "claim_available"
            | "execute_task"
            | "run_supported"
            | "travel_cycle"
            | "travel_depart"
            | "travel_claim"
            | "buddy_adopt"
            | "streak_redeem"
            | "makeup"
            | "lottery_draw"
            | "claim_gift"
            | "claim_compensation"
    ) {
        return Err(GrowthError::new(400, "未知成长动作"));
    }
    for field in ["taskCode", "tier"] {
        if let Some(code) = body.get(field).and_then(Value::as_str) {
            if !growth::valid_code(code) {
                return Err(GrowthError::new(400, "动作参数无效"));
            }
        }
    }
    let target = target(&store, &id)?;
    if body.get("expectedIdentity").and_then(Value::as_str) != Some(target.identity.as_str()) {
        return Err(GrowthError::new(409, "账号身份已变化，请刷新后重新操作"));
    }
    if !target.supported {
        return Ok(operation_result(
            &id,
            action,
            "not_applicable",
            "该账号不适用国内个人成长福利",
        ));
    }
    let identity = target.identity;
    let action_name = action.to_string();
    let token = body["clientToken"].as_str().unwrap_or("").to_string();
    let logical = logical_key(&body);
    let previous = task_state::read(&key(&identity))
        .map_err(|e| GrowthError::new(500, e))?
        .value
        .unwrap_or_else(|| json!({}));
    if let Some(result) = replay(&previous, &token, &logical)? {
        return Ok(result);
    }
    if previous
        .get("pending")
        .is_some_and(|p| !p.is_null() && p["clientToken"] != token)
    {
        return Err(GrowthError::new(
            409,
            "此前操作结果待确认，请刷新并继续原操作",
        ));
    }
    let claim = task_state::claim(
        &key(&identity),
        INTERVAL,
        true,
        ManualBackoff::Respect,
        1000,
    )
    .map_err(|e| GrowthError::new(500, e))?;
    let guard = match claim {
        Claim::Acquired(guard) => guard,
        Claim::Deferred(state) => {
            return Ok(operation_result(
                &id,
                &action_name,
                "busy",
                &state.waiting_message(),
            ))
        }
    };
    let job = tokio::spawn(async move {
        let mut journal = Journal::new(&identity)?;
        // 租约获取后再次判重，覆盖两个并发入口在占位前同时读取的窗口。
        if let Some(result) = replay(&journal.value, &token, &logical)? {
            guard
                .finish(true, "已回放先前执行结果".into(), None, 0, INTERVAL)
                .map_err(|e| GrowthError::new(500, e))?;
            return Ok(result);
        }
        journal.client_token = token.clone();
        journal.request = body.clone();
        journal.value["activeRequest"] =
            json!({"clientToken":token,"logicalKey":logical,"at":crate::server::logging::now_ms()});
        task_state::store_value(&journal.key, journal.value.clone())
            .map_err(|e| GrowthError::new(500, e))?;
        let client = Client::new(store.clone(), &id, &identity);
        let balance_before = query_balance(&store, &id, &identity).await;
        let before = client.state().await;
        let mut result = match before {
            Ok(before) => run(&client, &body, &before, &mut journal).await,
            Err(error) => Err(error),
        }
        .unwrap_or_else(|error| {
            operation_result(
                &id,
                &action_name,
                if error.uncertain {
                    "uncertain"
                } else {
                    "failed"
                },
                &error.message,
            )
        });
        if let Ok(after) = client.state().await {
            if result["receiptConfirmed"] == true {
                let confirmed = match result
                    .get("effectiveAction")
                    .and_then(Value::as_str)
                    .unwrap_or(&action_name)
                {
                    "travel_depart" => {
                        after.pointer("/travel/state").and_then(Value::as_str) == Some("traveling")
                            && after.pointer("/travel/locationId")
                                == Some(&result["operationTarget"])
                    }
                    "travel_claim" => {
                        matches!(
                            after.pointer("/travel/state").and_then(Value::as_str),
                            Some("idle" | "daily_limit")
                        ) && after
                            .pointer("/travel/recordId")
                            .is_some_and(|id| id.is_null() || id == &result["operationTarget"])
                    }
                    "buddy_adopt" => after
                        .pointer("/travel/buddyId")
                        .is_some_and(|v| !v.is_null()),
                    "makeup" => after
                        .pointer("/streak/makeupDates")
                        .and_then(Value::as_array)
                        .is_some_and(|dates| dates.contains(&body["date"])),
                    _ => result["stateConfirmed"] == true,
                };
                result["stateConfirmed"] = json!(confirmed);
            }
            result["state"] = after;
        }
        let balance_after = if journal.wrote {
            query_balance(&store, &id, &identity).await
        } else {
            balance_before
        };
        result["balanceBefore"] = json!(balance_before);
        result["balanceAfter"] = json!(balance_after);
        result["balanceDelta"] = json!(balance_before.zip(balance_after).map(|(a, b)| b - a));
        let mut saved = result.clone();
        if let Some(object) = saved.as_object_mut() {
            object.remove("state");
        }
        journal.value["lastRun"] = saved.clone();
        if result["status"] != "uncertain" {
            let mut requests = journal.value["requests"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            requests.push(json!({"clientToken":token,"logicalKey":logical,"result":saved}));
            if requests.len() > 32 {
                requests.drain(..requests.len() - 32);
            }
            journal.value["requests"] = json!(requests);
            journal.value["activeRequest"] = Value::Null;
        }
        let success = !matches!(result["status"].as_str(), Some("failed" | "uncertain"));
        guard
            .finish(
                success,
                result["message"]
                    .as_str()
                    .unwrap_or("成长福利执行结束")
                    .to_string(),
                Some(journal.value),
                0,
                INTERVAL,
            )
            .map_err(|e| GrowthError::new(500, e))?;
        Ok(result)
    });
    job.await
        .map_err(|_| GrowthError::new(500, "成长福利执行中断，请刷新状态核对"))?
}

struct Journal {
    key: String,
    value: Value,
    wrote: bool,
    client_token: String,
    request: Value,
}
impl Journal {
    fn new(identity: &str) -> Result<Self, GrowthError> {
        let key = key(identity);
        let value = task_state::read(&key)
            .map_err(|e| GrowthError::new(500, e))?
            .value
            .unwrap_or_else(|| json!({}));
        Ok(Self {
            key,
            value,
            wrote: false,
            client_token: String::new(),
            request: Value::Null,
        })
    }
    fn token(&self, action: &str, target: &str) -> String {
        let pending = &self.value["pending"];
        if pending["action"] == action && pending["target"] == target {
            if let Some(token) = pending["token"].as_str() {
                return token.into();
            }
        }
        use sha2::{Digest, Sha256};
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let seed = format!(
            "{}:{action}:{target}:{}:{}",
            self.key,
            crate::server::logging::now_ms(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        format!("{:x}", Sha256::digest(seed.as_bytes()))
    }
    fn clear(&mut self) -> Result<(), GrowthError> {
        self.value["pending"] = Value::Null;
        task_state::store_value(&self.key, self.value.clone()).map_err(|e| GrowthError::new(500, e))
    }
    async fn write<F>(
        &mut self,
        action: &str,
        target: &str,
        token: &str,
        future: F,
    ) -> Result<Value, GrowthError>
    where
        F: Future<Output = Result<Value, GrowthError>>,
    {
        if let Some(previous) = self.value["stepReceipts"].as_array().and_then(|rows| {
            rows.iter().find(|row| {
                row["clientToken"] == self.client_token
                    && row["action"] == action
                    && row["target"] == target
            })
        }) {
            return Ok(previous["receipt"].clone());
        }
        if let Some(pending) = self.value.get("pending").filter(|v| !v.is_null()) {
            if !(matches!(action, "lottery_draw" | "streak_redeem")
                && pending["action"] == action
                && pending["target"] == target
                && pending["clientToken"] == self.client_token)
            {
                return Err(GrowthError {
                    status: 409,
                    message: "此前动作结果尚未确认；请刷新官方状态后核对，未重复发送".into(),
                    uncertain: true,
                });
            }
        }
        self.value["pending"] = json!({"action":action,"target":target,"token":token,"clientToken":self.client_token,"request":self.request,"at":crate::server::logging::now_ms()});
        task_state::store_value(&self.key, self.value.clone())
            .map_err(|e| GrowthError::new(500, e))?;
        self.wrote = true;
        let result = future.await;
        if let Ok(receipt) = &result {
            self.value["lastReceipt"] = json!({"action":action,"target":target,"token":token,"rewards":growth::rewards(receipt),"at":crate::server::logging::now_ms()});
            let mut steps = self.value["stepReceipts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut fields = serde_json::Map::new();
            for field in [
                "reward_credit",
                "credit",
                "reward_energy",
                "energy",
                "already_claimed",
                "chances",
                "cards",
                "reward_buddy",
                "model",
                "completed",
                "credits",
            ] {
                if let Some(value) = receipt.get(field).filter(|v| {
                    v.is_null()
                        || v.is_number()
                        || v.is_boolean()
                        || v.as_str().is_some_and(|s| s.len() <= 128)
                }) {
                    fields.insert(field.into(), value.clone());
                }
            }
            if let Some(prize) = receipt.get("prize") {
                fields.insert("prize".into(),json!({"credit":growth::number(prize.get("credit")),"energy":growth::number(prize.get("energy")),"cards":growth::number(prize.get("cards")),"chances":growth::number(prize.get("chances"))}));
            }
            steps.push(json!({"clientToken":self.client_token,"action":action,"target":target,"receipt":fields}));
            if steps.len() > 128 {
                steps.drain(..steps.len() - 128);
            }
            self.value["stepReceipts"] = json!(steps);
        }
        if matches!(&result, Ok(_)) || matches!(&result,Err(error) if !error.uncertain) {
            self.clear()?;
        }
        result
    }
}

fn task<'a>(state: &'a Value, code: &str) -> Option<&'a Value> {
    state["tasks"]
        .as_array()?
        .iter()
        .find(|t| t["code"] == code)
}
fn yes(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_bool) == Some(true)
}
fn receipt(id: &str, action: &str, value: &Value) -> Value {
    let already = yes(value, "already_claimed");
    let mut result = operation_result(
        id,
        action,
        if already {
            "already_claimed"
        } else {
            "claimed"
        },
        if already {
            "上游确认此前已领取"
        } else {
            "上游确认操作成功，奖励以回执为准"
        },
    );
    result["receiptConfirmed"] = json!(true);
    result["rewards"] = if already {
        growth::rewards(&json!({"credit":0,"energy":0,"chances":0,"cards":0,"reward_buddy":false}))
    } else {
        growth::rewards(value.get("prize").unwrap_or(value))
    };
    result
}

/// 默认关闭的周期巡检只领奖/旅行，不发对话、不领养、不消耗补签或抽奖资源。
pub async fn scheduled_run(store: &AccountStore) -> Result<String, String> {
    let accounts = store.list_accounts()["accounts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut identities = HashSet::new();
    let mut total = 0;
    let mut failed = 0;
    for account in accounts {
        if account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("workbuddy")
            != "workbuddy"
        {
            continue;
        }
        let Some(id) = account["id"].as_str() else {
            continue;
        };
        let Ok(target) = target(store, id) else {
            continue;
        };
        if !target.supported || !identities.insert(target.identity.clone()) {
            continue;
        }
        let policy = workbuddy_policy::account_policy(id, &target.identity);
        for (enabled, operation) in [
            (yes(&policy, "autoGrowth"), "claim_available"),
            (yes(&policy, "autoTravel"), "travel_cycle"),
        ] {
            if !enabled {
                continue;
            }
            let Ok(current) = self::target(store, id) else {
                continue;
            };
            if !current.supported || current.identity != target.identity {
                continue;
            }
            let current_policy = workbuddy_policy::account_policy(id, &current.identity);
            if !yes(
                &current_policy,
                if operation == "claim_available" {
                    "autoGrowth"
                } else {
                    "autoTravel"
                },
            ) {
                continue;
            }
            total += 1;
            match action(store.clone(),json!({"id":id,"action":operation,"clientToken":super::upstream::request::new_request_id(),"expectedIdentity":current.identity})).await {Ok(result) if !matches!(result["status"].as_str(),Some("failed"|"uncertain"))=>{},_=>failed+=1}
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
    }
    let summary = format!("成长福利执行 {total} 项，失败 {failed} 项");
    if failed > 0 {
        Err(summary)
    } else {
        Ok(summary)
    }
}

async fn query_balance(store: &AccountStore, id: &str, identity: &str) -> Option<f64> {
    if target(store, id).ok()?.identity != identity {
        return None;
    }
    let report = usage_query::query_all(store, Some(id)).await.ok()?;
    if target(store, id).ok()?.identity != identity {
        return None;
    }
    report["results"]
        .as_array()?
        .iter()
        .find(|row| row["id"].as_str() == Some(id) && row["error"].is_null())
        .and_then(|row| {
            let details = &row["usage"]["creditDetails"];
            let at = details["fetchedAt"].as_i64()?;
            if details["complete"] != true || crate::server::logging::now_ms() - at > 900_000 {
                return None;
            }
            growth::number(details.get("remaining"))
        })
}

fn logical_key(body: &Value) -> String {
    let mut value = body.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("clientToken");
    }
    value.to_string()
}
fn replay(value: &Value, token: &str, logical: &str) -> Result<Option<Value>, GrowthError> {
    for request in value["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(value.get("activeRequest"))
    {
        if request["clientToken"] != token {
            continue;
        }
        if request["logicalKey"] != logical {
            return Err(GrowthError::new(400, "clientToken 已绑定其他动作或参数"));
        }
        if let Some(result) = request.get("result") {
            return Ok(Some(result.clone()));
        }
        // 回执已持久化而进程尚未来得及保存整体结果时，单步资源动作仍可回放。
        let request_body: Value = serde_json::from_str(logical).unwrap_or(Value::Null);
        let action = request_body["action"].as_str().unwrap_or("");
        if matches!(
            action,
            "lottery_draw"
                | "streak_redeem"
                | "makeup"
                | "buddy_adopt"
                | "travel_depart"
                | "travel_claim"
                | "travel_cycle"
                | "claim_task"
                | "accept_task"
                | "claim_gift"
                | "claim_compensation"
        ) {
            if let Some(step) = value["stepReceipts"]
                .as_array()
                .and_then(|steps| steps.iter().rev().find(|step| step["clientToken"] == token))
            {
                let mut result = receipt(
                    request_body["id"].as_str().unwrap_or(""),
                    action,
                    &step["receipt"],
                );
                result["effectiveAction"] = step["action"].clone();
                if matches!(
                    step["action"].as_str(),
                    Some("travel_depart" | "buddy_adopt" | "makeup" | "accept_task")
                ) {
                    result["status"] = json!("completed");
                }
                return Ok(Some(result));
            }
        }
    }
    Ok(None)
}
fn validate_action(body: &Value) -> Result<(), GrowthError> {
    let object = body
        .as_object()
        .ok_or_else(|| GrowthError::new(400, "动作请求必须为对象"))?;
    for (field, value) in object {
        match field.as_str() {
            "id" | "action" | "taskCode" | "source" | "tier" | "date" | "clientToken"
            | "expectedIdentity" => {
                if !value.is_null()
                    && !value
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && s.len() <= 256)
                {
                    return Err(GrowthError::new(400, "动作字符串参数无效"));
                }
            }
            "locationId" => {
                if !value.is_null() && growth::string_id(Some(value)).is_none() {
                    return Err(GrowthError::new(400, "目的地参数无效"));
                }
            }
            "agreementAccepted" => {
                if !value.is_boolean() {
                    return Err(GrowthError::new(400, "协议参数必须为布尔值"));
                }
            }
            _ => return Err(GrowthError::new(400, "包含未知动作参数")),
        }
    }
    let action = body["action"].as_str().unwrap_or("");
    if body["expectedIdentity"]
        .as_str()
        .map_or(true, str::is_empty)
    {
        return Err(GrowthError::new(400, "缺少账号身份 expectedIdentity"));
    }
    let token = body["clientToken"].as_str().unwrap_or("");
    if token.is_empty()
        || token.len() > 128
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(GrowthError::new(400, "缺少有效 clientToken"));
    }
    if let Some(source) = body["source"].as_str() {
        if !matches!(source, "growth" | "mini_program") {
            return Err(GrowthError::new(400, "未知任务来源"));
        }
    }
    let required = match action {
        "accept_task" | "claim_task" | "execute_task" => Some("taskCode"),
        "streak_redeem" => Some("tier"),
        "makeup" => Some("date"),
        _ => None,
    };
    if required.is_some_and(|field| body[field].as_str().map_or(true, str::is_empty)) {
        return Err(GrowthError::new(400, "动作缺少必需参数"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn action_contract_and_replay_bind_token_to_parameters() {
        assert!(personal_cn(&json!({"edition":"cn","type":"personal"})));
        for excluded in [
            json!({"edition":"intl"}),
            json!({"type":"enterprise"}),
            json!({"tenantId":"tenant"}),
            json!({"account":{"type":"enterprise","enterpriseId":""}}),
        ] {
            assert!(!personal_cn(&excluded));
        }
        assert!(validate_action(
            &json!({"id":"a","action":"lottery_draw","clientToken":"request-1","expectedIdentity":"identity"})
        )
        .is_ok());
        for invalid in [
            json!({"id":"a","action":"claim_task","clientToken":"request-1"}),
            json!({"id":"a","action":"lottery_draw","clientToken":""}),
            json!({"id":"a","action":"lottery_draw","clientToken":"request-1","source":"other"}),
            json!({"id":"a","action":"buddy_adopt","clientToken":"request-1","agreementAccepted":"true"}),
        ] {
            assert!(validate_action(&invalid).is_err());
        }
        let original = json!({"id":"a","action":"lottery_draw","clientToken":"request-1"});
        let key = logical_key(&original);
        let stored = json!({"requests":[{"clientToken":"request-1","logicalKey":key,"result":{"status":"claimed","rewards":{"credits":0}}}]});
        assert_eq!(
            replay(&stored, "request-1", &key).unwrap().unwrap()["rewards"]["credits"],
            0
        );
        assert!(replay(
            &stored,
            "request-1",
            &logical_key(&json!({"id":"a","action":"makeup","date":"2026-10-02"}))
        )
        .is_err());
        assert!(replay(&stored, "request-2", &key).unwrap().is_none());
    }
}
