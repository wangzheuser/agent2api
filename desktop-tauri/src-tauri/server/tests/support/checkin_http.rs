//! 仅绑定回环地址的脚本上游；记录真实 HTTP 请求，额外请求返回失败。
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json, Router,
};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: String,
}

#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<VecDeque<(u16, Value)>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

pub struct MockUpstream {
    pub base: String,
    script: Script,
    task: tokio::task::JoinHandle<()>,
}

impl MockUpstream {
    pub async fn spawn(replies: Vec<(u16, Value)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let script = Script {
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let router = Router::new().fallback(reply).with_state(script.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { base, script, task }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.script.requests.lock().unwrap().clone()
    }

    pub fn assert_drained(&self) {
        assert!(
            self.script.replies.lock().unwrap().is_empty(),
            "有预期请求未执行"
        );
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn reply(State(script): State<Script>, request: Request) -> impl IntoResponse {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 64 * 1024).await.unwrap();
    script.requests.lock().unwrap().push(RecordedRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        headers: parts.headers,
        body: String::from_utf8(body.to_vec()).unwrap(),
    });
    let (status, value) = script
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or((500, json!({"error": "unexpected request"})));
    (StatusCode::from_u16(status).unwrap(), Json(value))
}
