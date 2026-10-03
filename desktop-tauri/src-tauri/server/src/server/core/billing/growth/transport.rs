use crate::server::core::{
    account_store::AccountStore, auth::AuthService, egress, proxies::ResolvedProxy,
};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct GrowthError {
    pub status: i32,
    pub message: String,
    pub uncertain: bool,
}
impl GrowthError {
    pub fn new(status: i32, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            uncertain: false,
        }
    }
    pub fn json(&self, section: &str) -> Value {
        json!({"section":section,"code":self.status,"message":self.message})
    }
}

#[derive(Clone)]
pub struct Client {
    pub id: String,
    pub identity: String,
    store: AccountStore,
}
impl Client {
    pub fn new(store: AccountStore, id: &str, identity: &str) -> Self {
        Self {
            store,
            id: id.into(),
            identity: identity.into(),
        }
    }

    fn session(&self) -> Result<Value, GrowthError> {
        let target = crate::server::core::workbuddy_growth::target(&self.store, &self.id)?;
        if target.identity != self.identity || !target.supported {
            return Err(GrowthError::new(409, "账号身份已变化，请重新打开成长福利"));
        }
        Ok(target.session)
    }

    pub(super) async fn get(&self, path: &str, mini: bool) -> Result<Value, GrowthError> {
        let value = self.request("GET", path, false, mini, None, true).await?;
        if !value.is_object() {
            return Err(GrowthError::new(502, "成长状态响应不是对象"));
        }
        Ok(value)
    }
    pub(super) async fn post(
        &self,
        path: &str,
        web: bool,
        mini: bool,
        body: Value,
    ) -> Result<Value, GrowthError> {
        self.request("POST", path, web, mini, Some(body), true)
            .await
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        web: bool,
        mini: bool,
        body: Option<Value>,
        envelope: bool,
    ) -> Result<Value, GrowthError> {
        for attempt in 0..2 {
            let session = self.session()?;
            if session
                .get("proxyError")
                .is_some_and(|v| !v.is_null() && v != "")
            {
                return Err(GrowthError::new(502, "账号代理不可用"));
            }
            let proxy = ResolvedProxy::from_json(session.get("proxy").unwrap_or(&Value::Null))
                .map_err(|_| GrowthError::new(502, "账号代理配置无效"))?;
            let client = egress::client_without_redirects(proxy.as_ref())
                .map_err(|_| GrowthError::new(502, "成长中心客户端创建失败"))?;
            let domain = if web {
                "https://www.workbuddy.cn"
            } else {
                "https://copilot.tencent.com"
            };
            let mut request = client
                .request(
                    if method == "GET" {
                        reqwest::Method::GET
                    } else {
                        reqwest::Method::POST
                    },
                    format!("{domain}{path}"),
                )
                .timeout(Duration::from_secs(25));
            let mut headers = super::super::request::whitelist_headers(&session);
            if web {
                headers.extend([
                    ("Origin".into(), domain.into()),
                    ("Referer".into(), format!("{domain}/profile/growth-center")),
                    ("X-Client-Platform".into(), "web".into()),
                ]);
            }
            if mini {
                headers.push(("X-Client-Platform".into(), "miniprogram".into()));
            }
            for (name, value) in super::super::request::build_headers(&session, &headers) {
                request = request.header(name, value);
            }
            if let Some(body) = &body {
                request = request.json(body);
            }
            let mut response = request.send().await.map_err(|_| GrowthError {
                status: 504,
                message: "成长中心请求未确认，请刷新状态后核对".into(),
                uncertain: method != "GET",
            })?;
            let status = response.status().as_u16();
            if status == 401 && attempt == 0 {
                self.session()?;
                AuthService::for_store(self.store.clone())
                    .refresh_account(&self.id)
                    .await
                    .map_err(|_| GrowthError::new(401, "登录态刷新失败"))?;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(GrowthError {
                    status: status as i32,
                    message: format!("成长中心请求 HTTP {status}"),
                    uncertain: method != "GET" && status >= 500,
                });
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| GrowthError {
                status: 502,
                message: "成长中心响应中断，结果待核对".into(),
                uncertain: method != "GET",
            })? {
                if bytes.len() + chunk.len() > 1_048_576 {
                    return Err(GrowthError {
                        status: 502,
                        message: "成长中心响应超过大小限制".into(),
                        uncertain: method != "GET",
                    });
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| GrowthError {
                status: 502,
                message: "成长中心响应格式异常".into(),
                uncertain: method != "GET",
            })?;
            self.session().map_err(|mut error| {
                error.uncertain = method != "GET";
                error
            })?;
            if !envelope {
                return Ok(value);
            }
            let accepted = value
                .get("code")
                .and_then(Value::as_i64)
                .map_or(true, |code| code == 0);
            return decode(value).map_err(|mut error| {
                error.uncertain = method != "GET" && accepted;
                error
            });
        }
        Err(GrowthError::new(401, "登录态已失效"))
    }

    /// 真实短对话仅证明动作完成，任务是否计分必须另外读回。
    pub async fn chat(&self, model: &str) -> Result<Value, GrowthError> {
        // 共享目录可能来自另一地区；发出对话前从当前国内会话核对模型及免费声明。
        let config = self.get("/v3/config", false).await?;
        let advertised = config["models"]
            .as_array()
            .and_then(|models| models.iter().find(|item| item["id"] == model));
        let Some(advertised) = advertised else {
            return Err(GrowthError::new(400, "当前地区模型目录未包含任务模型"));
        };
        if model != "glm-5.2" && !super::known_free(&advertised["credits"]) {
            return Err(GrowthError::new(
                400,
                "当前地区未明确该模型免费，未发送自动对话",
            ));
        }
        let session = self.session()?;
        let mut session = session.clone();
        session["endpoint"] = json!("https://copilot.tencent.com");
        let plan = crate::server::core::providers::workbuddy::build_daily_activity_request(
            &session, model,
        )
        .map_err(|_| GrowthError::new(400, "真实对话请求构造失败"))?;
        let result = tokio::time::timeout(Duration::from_secs(90), async {
            let response = crate::server::core::upstream::request::send_chat_request(&plan)
                .await
                .map_err(|_| GrowthError::new(502, "真实对话发送失败"))?;
            if !response.status().is_success() {
                return Err(GrowthError::new(
                    response.status().as_u16() as i32,
                    "真实对话上游拒绝请求",
                ));
            }
            super::super::activity_response::consume_activity_response(response, 1 << 20)
                .await
                .map_err(|_| GrowthError::new(502, "真实对话未返回完整内容及结束标记"))
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(mut error)) => {
                error.uncertain = error.status >= 500;
                return Err(error);
            }
            Err(_) => {
                return Err(GrowthError {
                    status: 504,
                    message: "真实对话超时，完成状态待确认".into(),
                    uncertain: true,
                })
            }
        }
        Ok(json!({"model":model,"completed":true,"credits":null}))
    }
}

pub(super) fn decode(value: Value) -> Result<Value, GrowthError> {
    if value.get("code").and_then(Value::as_i64) != Some(0) {
        let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
        return Err(GrowthError::new(
            502,
            format!("成长中心业务失败（code={code}）"),
        ));
    }
    value
        .get("data")
        .cloned()
        .ok_or_else(|| GrowthError::new(502, "成长中心响应缺少 data"))
}
