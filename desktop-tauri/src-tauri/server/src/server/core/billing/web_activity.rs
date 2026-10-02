//! 国际版网页任务：建会话后通过 ACP 真正执行一轮（参考 workbuddy2api-hub #90）。
//! 会话完成只证明通道有效；奖励资格和次日到账仍以上游结算为准。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder, Response};
use serde::Serialize;
use serde_json::{json, Value};

use crate::server::core::{egress, proxies::ResolvedProxy};

const ORIGIN: &str = "https://www.workbuddy.ai";
const PROMPT: &str = "Reply with just OK. Do not use tools.";
const MAX_BYTES: usize = 1 << 20;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Outcome {
    pub success: bool,
    pub conversation_id: Option<String>,
    pub status: String,
    pub output_chunks: usize,
    pub elapsed_ms: u128,
    pub error: Option<String>,
}

// 自动任务与手动入口共享在途去重；不持锁跨 await，也不把失败记成当天已完成。
static INFLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
struct Reservation(String);
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Ok(mut ids) = INFLIGHT.get_or_init(Default::default).lock() {
            ids.remove(&self.0);
        }
    }
}

pub(super) async fn run(session: &Value, model: &str) -> Outcome {
    let start = Instant::now();
    let mut outcome = Outcome::default();
    let result = tokio::time::timeout(TURN_TIMEOUT, async {
        let uid = required(session.pointer("/account/uid"), "账号标识缺失")?;
        let _reservation = {
            let mut ids = INFLIGHT
                .get_or_init(Default::default)
                .lock()
                .map_err(|_| "网页保活锁不可用".to_string())?;
            if !ids.insert(uid.to_string()) {
                return Err("该账号网页保活正在执行".to_string());
            }
            Reservation(uid.to_string())
        };
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
        execute(&client, ORIGIN, uid, token, model, &mut outcome).await
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

async fn send(request: RequestBuilder, stage: &str) -> Result<Response, String> {
    let response = tokio::time::timeout(HTTP_TIMEOUT, request.send())
        .await
        .map_err(|_| format!("{stage}超时"))?
        .map_err(|_| format!("{stage}网络请求失败"))?;
    if !response.status().is_success() {
        return Err(format!("{stage} HTTP {}", response.status().as_u16()));
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

async fn console(request: RequestBuilder, stage: &str) -> Result<Value, String> {
    let body = read_body(send(request, stage).await?).await?;
    let value: Value =
        serde_json::from_slice(&body).map_err(|_| format!("{stage}响应不是有效 JSON"))?;
    if value.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(format!(
            "{stage}业务失败（code={}）",
            value.get("code").and_then(Value::as_i64).unwrap_or(-1)
        ));
    }
    Ok(value["data"].clone())
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
) -> Result<(), String> {
    let base = format!("{origin}/console/as/conversations/");
    let request = web_request(client, reqwest::Method::POST, &base, uid, token)
        .json(&json!({"prompt": PROMPT, "model": model, "conversationOrigin": "workbuddy-app"}));
    let created = console(request, "创建网页会话").await?;
    let id = conversation_id(&created["id"])?;
    outcome.conversation_id = Some(id.clone());
    outcome.status = "CREATING".to_string();
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
    let response = send(
        client
            .get(link.clone())
            .bearer_auth(sandbox_token)
            .header("Accept", "text/event-stream"),
        "连接 ACP 事件流",
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
    channel.output_chunks = 0;
    let result = channel
        .rpc(
            3,
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
    // RPC 的 end_turn 与控制台落库状态都要确认；只收到 HTTP 202 或一段文字不算完成。
    loop {
        let detail = console(
            web_request(client, reqwest::Method::GET, &conversation_url, uid, token),
            "核对网页会话",
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
}

impl AcpChannel<'_> {
    async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Result<Value, String> {
        let response = send(
            self.client
                .post(self.link.clone())
                .bearer_auth(self.token)
                .header("Acp-Connection-Id", &self.connection)
                .header("Accept", "application/json, text/event-stream")
                .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})),
            method,
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
