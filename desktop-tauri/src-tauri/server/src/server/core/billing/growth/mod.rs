//! 国内个人成长中心。固定上游域、真实状态和明确回执，不合成客户端事件。

mod normalize;
mod transport;
pub use normalize::{number, rewards, string_id};
pub use transport::{Client, GrowthError};

use serde_json::{json, Value};

pub const CHAT_CODES: &[&str] = &["chat_5", "Model_chat_GLM5.2"];

pub fn chat_model(code: &str) -> Option<String> {
    let catalog = crate::server::core::models::global_catalog(
        crate::server::core::providers::workbuddy::Region::Cn,
    );
    if !catalog.remote_refreshed()
        || crate::server::logging::now_ms() - catalog.last_refreshed_at() > 86_400_000
    {
        return None;
    }
    let models = catalog.list();
    if code == "Model_chat_GLM5.2" {
        return models
            .iter()
            .find(|m| m["id"] == "glm-5.2")
            .map(|_| "glm-5.2".into());
    }
    if code != "chat_5" {
        return None;
    }
    models
        .iter()
        .find(|model| known_free(&model["credits"]))
        .and_then(|model| model["id"].as_str().map(str::to_string))
}
fn known_free(value: &Value) -> bool {
    value.as_f64() == Some(0.0)
        || value.as_str().is_some_and(|s| {
            s.trim()
                .trim_start_matches('x')
                .trim_end_matches("credits")
                .trim()
                .parse::<f64>()
                == Ok(0.0)
        })
}

impl Client {
    /// 各模块可部分失败；错误节保持 null，不把异常显示成正常空列表。
    pub async fn state(&self) -> Result<Value, GrowthError> {
        let (tasks, mini, travel, config, buddy, streak, heatmap, lottery) = tokio::join!(
            self.get("/v2/activity/growth/tasks", false),
            self.get("/v2/activity/growth/tasks", true),
            self.get("/activity/growth/buddy/travel/status", false),
            self.get("/activity/growth/buddy/travel/config", false),
            self.get("/activity/growth/buddy/info", false),
            self.get("/activity/growth/streak", false),
            self.get("/activity/growth/heatmap", false),
            self.get("/activity/growth/lottery/summary", false)
        );
        let mut errors = Vec::new();
        let mut capabilities = Vec::new();
        let mut all_tasks = Vec::new();
        for (source, result) in [("growth", tasks), ("mini_program", mini)] {
            match result.and_then(|value| normalize::tasks(&value, source)) {
                Ok(rows) => {
                    capabilities.push(source);
                    for row in rows {
                        if !all_tasks
                            .iter()
                            .any(|other: &Value| other["code"] == row["code"])
                        {
                            all_tasks.push(row);
                        }
                    }
                }
                Err(error) => errors.push(error.json(source)),
            }
        }
        let travel = match (travel, config, buddy) {
            (Ok(travel), config, buddy) => {
                let config = unwrap_section(config, "travel_config", &mut errors);
                let buddy = unwrap_section(buddy, "buddy", &mut errors);
                capabilities.push("travel");
                normalize::travel(&travel, &config, &buddy)
            }
            (Err(error), _, _) => {
                errors.push(error.json("travel"));
                Value::Null
            }
        };
        let streak = match streak {
            Ok(value) => {
                capabilities.push("streak");
                let heatmap = unwrap_section(heatmap, "heatmap", &mut errors);
                normalize::streak(&value, &heatmap)
            }
            Err(error) => {
                errors.push(error.json("streak"));
                Value::Null
            }
        };
        let lottery = match lottery {
            Ok(value) => {
                capabilities.push("lottery");
                json!({"chances": number(value.get("chances")), "canDraw": value.pointer("/module/enabled").and_then(Value::as_bool) == Some(true) && number(value.get("chances")).is_some_and(|n| n > 0.0)})
            }
            Err(error) => {
                errors.push(error.json("lottery"));
                Value::Null
            }
        };
        if capabilities.is_empty() {
            return Err(GrowthError::new(502, "成长中心各项状态查询失败"));
        }
        Ok(
            json!({"schemaVersion":1,"id":self.id,"identity":self.identity,
            "fetchedAt":crate::server::logging::now_ms(),"edition":"cn","supported":true,"reason":null,
            "capabilities":capabilities,"tasks":all_tasks,"travel":travel,"streak":streak,"lottery":lottery,
            "activities":[
                {"code":"gift","title":"新手礼包","state":"unknown","canClaim":false,"canAttempt":true,"reason":"资格由上游确认；仅手动单次尝试"},
                {"code":"compensation","title":"活动补偿","state":"unknown","canClaim":false,"canAttempt":true,"reason":"活动开放及资格由上游确认；仅手动单次尝试"}
            ],"errors":errors,"running":false,"lastRun":null}),
        )
    }

    pub async fn accept(&self, code: &str, mini: bool) -> Result<Value, GrowthError> {
        self.post(
            "/v2/activity/growth/tasks/accept",
            false,
            mini,
            json!({"task_codes":[code]}),
        )
        .await
    }

    pub async fn tasks(&self, mini: bool) -> Result<Vec<Value>, GrowthError> {
        let value = self.get("/v2/activity/growth/tasks", mini).await?;
        normalize::tasks(&value, if mini { "mini_program" } else { "growth" })
    }

    pub async fn claim(&self, code: &str, mini: bool) -> Result<Value, GrowthError> {
        if !valid_code(code) {
            return Err(GrowthError::new(400, "任务标识无效"));
        }
        self.post(
            &format!("/activity/growth/tasks/{code}/claim"),
            !mini,
            mini,
            json!({}),
        )
        .await
    }

    pub async fn travel_depart(&self, location: &str) -> Result<Value, GrowthError> {
        let location = location
            .parse::<u64>()
            .map_err(|_| GrowthError::new(400, "目的地标识无效"))?;
        self.post(
            "/activity/growth/buddy/travel/depart",
            false,
            false,
            json!({"location_id":location}),
        )
        .await
    }

    pub async fn travel_claim(&self, record: &str) -> Result<Value, GrowthError> {
        let record = record
            .parse::<u64>()
            .map_err(|_| GrowthError::new(400, "旅行记录标识无效"))?;
        self.post(
            "/activity/growth/buddy/travel/claim",
            false,
            false,
            json!({"record_id":record}),
        )
        .await
    }

    pub async fn adopt(&self) -> Result<Value, GrowthError> {
        self.post(
            "/activity/growth/buddy/agreement",
            false,
            false,
            json!({"agree":true}),
        )
        .await?;
        self.post("/activity/growth/buddy/first", false, false, json!({}))
            .await
    }

    pub async fn redeem(&self, tier: &str, token: &str) -> Result<Value, GrowthError> {
        self.post(
            "/activity/growth/redeem",
            false,
            false,
            json!({"tier":tier,"client_token":token}),
        )
        .await
    }

    pub async fn makeup(&self, date: &str) -> Result<Value, GrowthError> {
        self.post(
            "/activity/growth/makeup-cards/use",
            false,
            false,
            json!({"target_date":date}),
        )
        .await
    }

    pub async fn lottery(&self, token: &str) -> Result<Value, GrowthError> {
        self.post(
            "/activity/growth/lottery/draw",
            false,
            false,
            json!({"client_token":token}),
        )
        .await
    }

    pub async fn gift(&self, compensation: bool) -> Result<Value, GrowthError> {
        let path = if compensation {
            "/billing/meter/claim-compensation"
        } else {
            "/billing/meter/claim-gift"
        };
        self.post(path, false, false, json!({})).await
    }
}

fn unwrap_section(
    result: Result<Value, GrowthError>,
    section: &str,
    errors: &mut Vec<Value>,
) -> Value {
    match result {
        Ok(value) => value,
        Err(error) => {
            errors.push(error.json(section));
            Value::Null
        }
    }
}

pub fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 128
        && code
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests;
