//! 国际版网页任务：建会话后通过 ACP 真正执行一轮（参考 workbuddy2api-hub #90）。
//! 会话完成只证明通道有效；奖励资格和次日到账仍以上游结算为准。
//! UTC 日期仅用于本地日级去重，不代表上游奖励结算日。

use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::core::{account_store::AccountStore, egress, proxies::ResolvedProxy, task_state};
use crate::server::logging;

const ORIGIN: &str = "https://www.workbuddy.ai";
const PROMPT: &str = "Reply with just OK. Do not use tools.";
const MAX_BYTES: usize = 1 << 20;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Outcome {
    pub success: bool,
    pub conversation_id: Option<String>,
    pub status: String,
    pub output_chunks: usize,
    #[serde(default)]
    pub turn_completed: bool,
    #[serde(default)]
    pub reconciled: bool,
    pub elapsed_ms: u128,
    pub error: Option<String>,
}

fn state_key(session: &Value) -> Result<String, String> {
    let identity = crate::server::core::workbuddy_policy::identity(session)
        .ok_or_else(|| "账号身份缺失".to_string())?;
    Ok(format!("workbuddy-web-activity:{identity}"))
}

fn saved_outcome(value: Option<&Value>, day: &str) -> Result<Option<Outcome>, String> {
    let Some(value) = value else { return Ok(None) };
    match value["phase"].as_str() {
        Some("success") => {
            let outcome: Outcome = serde_json::from_value(value["outcome"].clone())
                .map_err(|_| "网页活跃持久化结果无效，未发送请求".to_string())?;
            if !outcome.success
                || !outcome.turn_completed
                || outcome.status != "completed"
                || (outcome.output_chunks == 0 && !outcome.reconciled)
            {
                return Err("网页活跃持久化结果不完整，未发送请求".to_string());
            }
            let saved_day = value["day"].as_str().and_then(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
                .ok_or_else(|| "网页活跃记录日期无效，未发送请求".to_string())?;
            let today = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .map_err(|_| "网页活跃当前日期无效".to_string())?;
            if saved_day > today { return Err("网页活跃记录在未来，请核对系统时间".to_string()); }
            Ok((saved_day == today).then_some(outcome))
        }
        Some("rejected")
            if retryable_without_conversation(value["outcome"]["status"].as_str())
                && value["outcome"]["conversationId"].is_null() =>
        {
            Ok(None)
        }
        _ => Err("上次网页会话创建或执行结果待核对，不自动创建新会话".to_string()),
    }
}

/// 没有留下会话的状态都允许下一轮重新创建：上游明确拒绝、活跃会话数达到上限，
/// 或重启后核对确认上次会话已终止。只要记录里带会话 ID，就必须按「待核对」处理，
/// 不能因为用户后来手动结束了会话就重复创建。
fn retryable_without_conversation(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("CREATE_REJECTED" | "ACTIVE_LIMIT" | "RECONCILED_FAILED")
    )
}

pub(super) fn cached(session: &Value) -> Result<Option<Outcome>, String> {
    let state = task_state::read(&state_key(session)?)?;
    saved_outcome(state.value.as_ref(), &chrono::Utc::now().format("%Y-%m-%d").to_string())
}

enum ReconcileDecision {
    Completed(Outcome, String),
    Retry,
    Unresolved,
}

async fn reconcile_saved_conversation(
    client: &Client,
    origin: &str,
    uid: &str,
    token: &str,
    saved: Option<&Value>,
    safety: Option<(&AccountStore, &Value)>,
) -> ReconcileDecision {
    let Some(saved) = saved else { return ReconcileDecision::Retry };
    let Some(saved_id) = saved["conversationId"]
        .as_str()
        .or_else(|| saved["outcome"]["conversationId"].as_str())
    else {
        return ReconcileDecision::Unresolved;
    };
    let Ok(conversation_id) = conversation_id(&Value::String(saved_id.to_string())) else {
        return ReconcileDecision::Retry;
    };
    let saved_day = saved["day"].as_str().unwrap_or_default().to_string();
    let url = format!("{origin}/console/as/conversations/{conversation_id}");
    let detail = console(
        web_request(client, reqwest::Method::GET, &url, uid, token),
        "核对未完成网页会话",
        safety,
    )
    .await;
    let status = detail
        .ok()
        .and_then(|value| value.get("status").and_then(Value::as_str).map(str::to_string));
    match status.as_deref() {
        Some("completed") => {
            let mut outcome = serde_json::from_value::<Outcome>(saved["outcome"].clone())
                .unwrap_or_default();
            outcome.success = true;
            outcome.conversation_id = Some(conversation_id);
            outcome.status = "completed".into();
            outcome.turn_completed = true;
            outcome.reconciled = true;
            ReconcileDecision::Completed(outcome, saved_day)
        }
        Some("failed") | Some("error") | Some("cancelled") | Some("canceled") => {
            ReconcileDecision::Retry
        }
        _ if saved_day < chrono::Utc::now().format("%Y-%m-%d").to_string() => {
            // A prior day's non-terminal conversation cannot satisfy today's daily task.
            ReconcileDecision::Retry
        }
        _ => ReconcileDecision::Unresolved,
    }
}

pub(super) async fn run(store: &AccountStore, session: &Value, model: &str) -> Outcome {
    run_with_origin(store, session, model, ORIGIN).await
}

async fn run_with_origin(store: &AccountStore, session: &Value, model: &str, origin: &str) -> Outcome {
    let start = Instant::now();
    let mut outcome = Outcome::default();
    let result = tokio::time::timeout(TURN_TIMEOUT, async {
        let uid = required(session.pointer("/account/uid"), "账号标识缺失")?;
        if session
            .get("proxyError")
            .is_some_and(|v| !v.is_null() && v != "")
        {
            return Err("账号代理不可用".to_string());
        }
        let proxy = ResolvedProxy::from_json(session.get("proxy").unwrap_or(&Value::Null))
            .map_err(|_| "账号代理配置无效".to_string())?;
        let client = egress::client_without_redirects(proxy.as_ref())
            .map_err(|_| "网页保活客户端创建失败".to_string())?;
        let token = required(session.pointer("/auth/accessToken"), "账号未登录")?;
        crate::server::core::workbuddy_safety::check(store, session).map_err(|error| error.message)?;
        let key = state_key(session)?;
        let lease = match task_state::claim(&key, 0, true, task_state::ManualBackoff::Respect, 0)? {
            task_state::Claim::Acquired(lease) => lease,
            task_state::Claim::Deferred(state) => return Err(state.waiting_message()),
        };
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let saved_state = task_state::read(&key)?.value;
        match saved_outcome(saved_state.as_ref(), &day) {
            Ok(Some(saved)) => {
                outcome = saved;
                lease.finish(true, "今日网页活跃已完成".into(), None, 0, 0)?;
                return Ok(());
            }
            Ok(None) => {}
            Err(error) => {
                match reconcile_saved_conversation(
                    &client,
                    origin,
                    uid,
                    token,
                    saved_state.as_ref(),
                    Some((store, session)),
                )
                .await
                {
                    ReconcileDecision::Completed(reconciled, saved_day) => {
                        task_state::store_value(
                            &key,
                            json!({"day":saved_day,"phase":"success","outcome":reconciled}),
                        )?;
                        if saved_day == day {
                            outcome = reconciled;
                            lease.finish(true, "网页活跃已完成（重启后核对）".into(), None, 0, 0)?;
                            return Ok(());
                        }
                    }
                    ReconcileDecision::Retry => {
                        task_state::store_value(
                            &key,
                            json!({"day":day,"phase":"rejected","outcome":{
                                "success":false,
                                "conversationId":Value::Null,
                                "status":"RECONCILED_FAILED",
                                "outputChunks":0,
                                "turnCompleted":false,
                                "reconciled":true,
                                "elapsedMs":0,
                                "error":"上次网页会话已终止，允许重新创建"
                            }}),
                        )?;
                    }
                    ReconcileDecision::Unresolved => return Err(error),
                }
            }
        }
        task_state::store_value(&key, json!({"day":day,"phase":"pending_create"}))?;
        let result = execute(
            &client,
            origin,
            uid,
            token,
            model,
            &mut outcome,
            Some((store, session)),
            Some((&key, &day)),
        )
        .await;
        outcome.success = result.is_ok();
        let phase = if result.is_ok() { "success" }
            // 没留下会话（上游拒绝或活跃会话已达上限）不占用今天的机会，下一轮可以重建。
            else if outcome.conversation_id.is_none() && retryable_without_conversation(Some(&outcome.status)) { "rejected" }
            else { "uncertain" };
        lease.finish(result.is_ok(), match phase { "success" => "网页活跃已完成", "rejected" => "创建请求已拒绝，未创建会话", _ => "网页活跃结果待核对" }.into(),
            Some(json!({"day":day,"phase":phase,"outcome":outcome})), 0, 0)?;
        result
    })
    .await;
    outcome.success = matches!(&result, Ok(Ok(())));
    outcome.error = match result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => Some("网页保活超时，未确认会话完成".to_string()),
    };
    outcome.elapsed_ms = start.elapsed().as_millis();
    outcome
}

fn required<'a>(value: Option<&'a Value>, error: &str) -> Result<&'a str, String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error.to_string())
}

fn web_request(
    client: &Client,
    method: reqwest::Method,
    url: &str,
    uid: &str,
    token: &str,
) -> RequestBuilder {
    client
        .request(method, url)
        .bearer_auth(token)
        .header("X-User-Id", uid)
        .header("Origin", ORIGIN)
        .header("Referer", format!("{ORIGIN}/app"))
        .header("Accept", "application/json")
        .timeout(HTTP_TIMEOUT)
}

#[cfg(test)]
async fn send(request: RequestBuilder, stage: &str) -> Result<Response, String> {
    send_checked(request, stage, None).await
}

async fn send_checked(request: RequestBuilder, stage: &str, safety: Option<(&AccountStore, &Value)>) -> Result<Response, String> {
    let response = tokio::time::timeout(HTTP_TIMEOUT, request.send())
        .await
        .map_err(|_| format!("{stage}超时"))?
        .map_err(|_| format!("{stage}网络请求失败"))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        if let Some((store, session)) = safety {
            let body = read_body(response).await?;
            let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Some(error) = super::workbuddy_stop_error(store, session, status, &value) {
                return Err(error.message);
            }
        }
        return Err(format!("{stage} HTTP {status}"));
    }
    Ok(response)
}

async fn read_body(mut response: Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "响应读取失败")? {
        if chunk.len() > MAX_BYTES.saturating_sub(body.len()) {
            return Err("响应超过大小限制".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn console(request: RequestBuilder, stage: &str, safety: Option<(&AccountStore, &Value)>) -> Result<Value, String> {
    if let Some((store, session)) = safety {
        crate::server::core::workbuddy_safety::check(store, session).map_err(|error| error.message)?;
    }
    let body = read_body(send_checked(request, stage, safety).await?).await?;
    let value: Value =
        serde_json::from_slice(&body).map_err(|_| format!("{stage}响应不是有效 JSON"))?;
    if let Some((store, session)) = safety {
        if let Some(error) = super::workbuddy_stop_error(store, session, 200, &value) {
            return Err(error.message);
        }
    }
    if value.get("code").and_then(Value::as_i64) != Some(0)
        && value.get("code").and_then(Value::as_str) != Some("0")
    {
        return Err(format!(
            "{stage}业务失败（code={}）",
            value.get("code").and_then(Value::as_i64).unwrap_or(-1)
        ));
    }
    Ok(value["data"].clone())
}

async fn release_conversation(
    client: &Client,
    origin: &str,
    uid: &str,
    token: &str,
    conversation_id: &str,
    delete: bool,
    safety: Option<(&AccountStore, &Value)>,
) {
    let url = format!("{origin}/console/as/conversations/{conversation_id}");
    let stop = web_request(client, reqwest::Method::POST, &format!("{url}/stop"), uid, token)
        .json(&json!({}));
    let _ = console(stop, "结束网页会话", safety).await;
    if delete {
        let remove =
            web_request(client, reqwest::Method::POST, &format!("{url}/delete"), uid, token)
                .json(&json!({}));
        let _ = console(remove, "删除失败网页会话", safety).await;
    }
}

fn active_conversation_limit(status: u16, value: &Value) -> bool {
    if status != 429 {
        return false;
    }
    let code = value["code"]
        .as_i64()
        .map(|code| code.to_string())
        .or_else(|| value["code"].as_str().map(str::to_string))
        .unwrap_or_default();
    let message = value["msg"]
        .as_str()
        .or_else(|| value["message"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    code == "10105" || message.contains("maximum number of active conversations")
}

async fn prune_active_conversations(
    client: &Client,
    origin: &str,
    uid: &str,
    token: &str,
    safety: Option<(&AccountStore, &Value)>,
) {
    let url = format!("{origin}/console/as/conversations/?page=1&pageSize=50");
    let Ok(data) = console(
        web_request(client, reqwest::Method::GET, &url, uid, token),
        "读取网页会话列表",
        safety,
    )
    .await
    else {
        return;
    };
    let mut active = data["conversations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|conversation| {
            matches!(
                conversation["status"].as_str(),
                Some("working" | "creating" | "pending")
            )
        })
        .collect::<Vec<_>>();
    if active.len() < 10 {
        return;
    }
    active.sort_by_key(|conversation| {
        conversation["createdAt"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    });
    let mut stopped = 0usize;
    for conversation in active {
        if 10usize.saturating_sub(stopped) <= 8 {
            break;
        }
        if conversation["name"].as_str() != Some(PROMPT) {
            continue;
        }
        let Some(id) = conversation["id"].as_str() else {
            continue;
        };
        release_conversation(client, origin, uid, token, id, false, None).await;
        stopped += 1;
    }
}

async fn create_conversation(
    request: RequestBuilder,
    outcome: &mut Outcome,
    safety: Option<(&AccountStore, &Value)>,
    persistence: Option<(&str, &str)>,
) -> Result<String, String> {
    if let Some((store, session)) = safety {
        if let Err(error) = crate::server::core::workbuddy_safety::check(store, session) {
            outcome.status = "CREATE_REJECTED".into();
            return Err(error.message);
        }
    }
    let response = tokio::time::timeout(HTTP_TIMEOUT, request.send()).await
        .map_err(|_| "创建网页会话超时，创建结果待核对".to_string())?
        .map_err(|_| "创建网页会话请求未确认，创建结果待核对".to_string())?;
    let status = response.status().as_u16();
    let body = read_body(response).await?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| "创建网页会话响应不是有效 JSON".to_string())?;
    if active_conversation_limit(status, &value) {
        outcome.status = "ACTIVE_LIMIT".into();
    } else if matches!(status, 400 | 401 | 402 | 403 | 404 | 405 | 413 | 415 | 422 | 429) {
        outcome.status = "CREATE_REJECTED".into();
    }
    if let Ok(id) = conversation_id(&value["data"]["id"]) {
        outcome.conversation_id = Some(id);
        outcome.status = "CREATING".into();
        if let Some((key, day)) = persistence {
            task_state::store_value(key, json!({"day":day,"phase":"conversation_created","conversationId":outcome.conversation_id}))?;
        }
    }
    if let Some((store, session)) = safety {
        if let Some(error) = super::workbuddy_stop_error(store, session, status, &value) {
            if outcome.conversation_id.is_none() {
                outcome.status = "CREATE_REJECTED".into();
            }
            return Err(error.message);
        }
    }
    if !(200..300).contains(&status) {
        return Err(format!("创建网页会话 HTTP {status}"));
    }
    if value["code"].as_i64() != Some(0) && value["code"].as_str() != Some("0") {
        return Err("创建网页会话业务失败，结果待核对".into());
    }
    outcome.conversation_id.clone().ok_or_else(|| "会话 ID 缺失，创建结果待核对".into())
}

fn conversation_id(value: &Value) -> Result<String, String> {
    let id = value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|v| v.to_string()))
        .ok_or("会话 ID 缺失")?;
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("会话 ID 无效".to_string());
    }
    Ok(id)
}

fn sandbox_url(link: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(link).map_err(|_| "沙箱地址无效")?;
    // 仅接受官方控制台签发的 HTTPS 地址，不把账号 token 带到沙箱，也不跟随重定向。
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(url.host(), Some(url::Host::Domain(host)) if host.contains('.') && host != "localhost")
    {
        return Err("沙箱地址不是有效 HTTPS 域名".to_string());
    }
    Ok(url)
}

async fn execute(
    client: &Client,
    origin: &str,
    uid: &str,
    token: &str,
    model: &str,
    outcome: &mut Outcome,
    safety: Option<(&AccountStore, &Value)>,
    persistence: Option<(&str, &str)>,
) -> Result<(), String> {
    let result = execute_inner(
        client,
        origin,
        uid,
        token,
        model,
        outcome,
        safety,
        persistence,
    )
    .await;
    // 只清理失败/中断留下的会话：`completed` 的会话不再占用「活跃会话」名额，
    // 成功后再补一次 /stop 既没有收益，还可能吃掉本轮剩余的时间预算把成功翻成超时。
    if let (true, Some(conversation_id)) = (result.is_err(), outcome.conversation_id.as_deref()) {
        release_conversation(
            client,
            origin,
            uid,
            token,
            conversation_id,
            true,
            None,
        )
        .await;
    }
    result
}

async fn execute_inner(
    client: &Client,
    origin: &str,
    uid: &str,
    token: &str,
    model: &str,
    outcome: &mut Outcome,
    safety: Option<(&AccountStore, &Value)>,
    persistence: Option<(&str, &str)>,
) -> Result<(), String> {
    let base = format!("{origin}/console/as/conversations/");
    let create = || {
        web_request(client, reqwest::Method::POST, &base, uid, token).json(&json!({
            "prompt": PROMPT,
            "model": model,
            "conversationOrigin": "workbuddy-app"
        }))
    };
    let id = match create_conversation(create(), outcome, safety, persistence).await {
        Ok(id) => id,
        Err(_error) if outcome.status == "ACTIVE_LIMIT" => {
            prune_active_conversations(client, origin, uid, token, safety).await;
            create_conversation(create(), outcome, safety, persistence).await?
        }
        Err(error) => return Err(error),
    };
    let conversation_url = format!("{base}{id}");
    let mut session = Value::Null;
    for attempt in 0..12 {
        session = console(
            web_request(
                client,
                reqwest::Method::GET,
                &format!("{conversation_url}/session"),
                uid,
                token,
            ),
            "取得网页沙箱",
            safety,
        )
        .await?;
        if session
            .get("link")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
            && session
                .get("token")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        {
            break;
        }
        if attempt < 11 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    let link = sandbox_url(required(session.get("link"), "沙箱未就绪")?)?;
    let sandbox_token = required(session.get("token"), "沙箱令牌缺失")?;
    let session_id = required(session.get("sessionId"), "沙箱会话 ID 缺失")?;
    let cwd = session
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("/workspace");
    let response = send_checked(
        client
            .get(link.clone())
            .bearer_auth(sandbox_token)
            .header("Accept", "text/event-stream"),
        "连接 ACP 事件流",
        safety,
    )
    .await?;
    let connection = response
        .headers()
        .get("Acp-Connection-Id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .ok_or("ACP 连接 ID 缺失")?
        .to_string();
    let mut channel = AcpChannel {
        client,
        link,
        token: sandbox_token,
        connection,
        events: Events::new(response),
        output_chunks: 0,
        safety,
    };
    let initialized = channel.rpc(1, "initialize", json!({"protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}})).await?;
    if initialized.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        return Err("ACP 协议版本不兼容".to_string());
    }
    channel
        .rpc(
            2,
            "session/load",
            json!({"sessionId":session_id,"cwd":cwd,"mcpServers":[]}),
        )
        .await?;
    // 建会话体里的 model 只是 manifest；网页端还会在 ACP 中显式钉住本轮模型，
    // 否则上游可能按账号默认模型执行，保活会误扣积分。
    // 不认这个方法的实现可能既不回包也不报错，所以给它单独的短上限——
    // 设置失败只是回到「按账号默认模型执行」，不能让整轮保活拖到 TURN_TIMEOUT。
    let pinned = tokio::time::timeout(
        Duration::from_secs(15),
        channel.rpc(
            3,
            "session/set_model",
            json!({"sessionId":session_id,"modelId":model}),
        ),
    )
    .await
    .unwrap_or_else(|_| Err("设置模型请求超时，本轮继续".to_string()));
    if let Err(error) = pinned {
        logging::verbose("[Checkin]", &format!("网页保活设置免费模型失败，本轮继续：{error}"));
    }
    channel.output_chunks = 0;
    let result = channel
        .rpc(
            4,
            "session/prompt",
            json!({"sessionId":session_id,
        "prompt":[{"type":"text","text":PROMPT}]}),
        )
        .await;
    outcome.output_chunks = channel.output_chunks;
    let result = result?;
    if result.get("stopReason").and_then(Value::as_str) != Some("end_turn")
        || outcome.output_chunks == 0
    {
        return Err("ACP 未收到非空回答或本轮未正常结束".to_string());
    }
    outcome.turn_completed = true;
    if let Some((key, day)) = persistence {
        task_state::store_value(key, json!({"day":day,"phase":"conversation_created","conversationId":id,"outcome":outcome}))?;
    }
    // RPC 的 end_turn 与控制台落库状态都要确认；只收到 HTTP 202 或一段文字不算完成。
    loop {
        let detail = console(
            web_request(client, reqwest::Method::GET, &conversation_url, uid, token),
            "核对网页会话",
            safety,
        )
        .await?;
        outcome.status = detail
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        match outcome.status.as_str() {
            "completed" => return Ok(()),
            "failed" | "error" | "cancelled" | "canceled" => {
                return Err("网页会话未成功完成".to_string())
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

struct AcpChannel<'a> {
    client: &'a Client,
    link: url::Url,
    token: &'a str,
    connection: String,
    events: Events,
    output_chunks: usize,
    safety: Option<(&'a AccountStore, &'a Value)>,
}

impl AcpChannel<'_> {
    async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Result<Value, String> {
        if let Some((store, session)) = self.safety {
            crate::server::core::workbuddy_safety::check(store, session).map_err(|error| error.message)?;
        }
        let response = send_checked(
            self.client
                .post(self.link.clone())
                .bearer_auth(self.token)
                .header("Acp-Connection-Id", &self.connection)
                .header("Accept", "application/json, text/event-stream")
                .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})),
            method,
            self.safety,
        )
        .await?;
        // ACP 可用 202 或空 200 确认接收，结果走 GET SSE；也可在 POST 上直接
        // 返回 JSON / SSE。HTTP 接收确认本身不代表 RPC 已成功执行。
        let streamed = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let mut post_events = None;
        let mut direct = None;
        if response.status() != reqwest::StatusCode::ACCEPTED {
            if streamed {
                post_events = Some(Events::new(response));
            } else {
                let body = read_body(response).await?;
                if !body.iter().all(u8::is_ascii_whitespace) {
                    direct = Some(
                        serde_json::from_slice(&body)
                            .map_err(|_| format!("{method}响应不是有效 JSON"))?,
                    );
                }
            }
        }
        loop {
            let event = match direct.take() {
                Some(v) => v,
                None => match post_events.as_mut() {
                    Some(events) => tokio::select! {
                        biased;
                        event = self.events.next() => event?,
                        event = events.next() => event?,
                    },
                    None => self.events.next().await?,
                },
            };
            if let Some((store, session)) = self.safety {
                if let Some(error) = super::workbuddy_stop_error(store, session, 200, event.get("error").unwrap_or(&event)) {
                    return Err(error.message);
                }
            }
            if let Some(result) = process_event(&event, id, &mut self.output_chunks)? {
                return Ok(result);
            }
        }
    }
}

fn process_event(event: &Value, id: u64, chunks: &mut usize) -> Result<Option<Value>, String> {
    if event.get("id").and_then(Value::as_u64) == Some(id) {
        if event.get("error").is_some() {
            return Err("ACP 返回业务错误".to_string());
        }
        return event
            .get("result")
            .cloned()
            .map(Some)
            .ok_or_else(|| "ACP 缺少调用结果".to_string());
    }
    if event.get("method").and_then(Value::as_str) == Some("session/update") {
        let update = &event["params"]["update"];
        if update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["type"] == "text"
            && update["content"]["text"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty())
        {
            *chunks += 1;
        }
    } else if event.get("method").is_some() && event.get("id").is_some() {
        // 不向上游授予文件、终端或交互式授权能力，避免后台保活执行额外操作。
        return Err("ACP 请求了保活不支持的客户端能力".to_string());
    }
    Ok(None)
}

struct Events {
    response: Response,
    buffer: Vec<u8>,
    data: Vec<String>,
    total: usize,
}
impl Events {
    fn new(response: Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            data: Vec::new(),
            total: 0,
        }
    }
    async fn next(&mut self) -> Result<Value, String> {
        loop {
            while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                let bytes: Vec<_> = self.buffer.drain(..=end).collect();
                let line = std::str::from_utf8(&bytes)
                    .map_err(|_| "ACP 事件不是 UTF-8")?
                    .trim_end_matches(['\r', '\n']);
                if line.is_empty() && !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data).join("\n");
                    return serde_json::from_str(&data)
                        .map_err(|_| "ACP 事件不是有效 JSON".to_string());
                }
                if let Some(value) = line.strip_prefix("data:") {
                    self.data.push(value.trim_start().to_string());
                }
                if line
                    .strip_prefix("event:")
                    .is_some_and(|v| v.trim() == "error")
                {
                    return Err("ACP 返回错误事件".to_string());
                }
            }
            let chunk = self
                .response
                .chunk()
                .await
                .map_err(|_| "ACP 事件流读取失败")?
                .ok_or("ACP 事件流提前结束")?;
            self.total += chunk.len();
            if self.total > MAX_BYTES {
                return Err("ACP 事件超过大小限制".to_string());
            }
            self.buffer.extend_from_slice(&chunk);
        }
    }
}

#[cfg(test)]
mod tests;
