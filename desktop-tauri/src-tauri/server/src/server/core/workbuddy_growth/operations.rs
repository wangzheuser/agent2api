//! 成长中心的固定动作分派与执行后状态核对。
use super::*;

fn query_failures(body: &Value, state: &Value) -> Vec<Value> {
    let action = body["action"].as_str().unwrap_or("");
    let source = task(state, body["taskCode"].as_str().unwrap_or(""))
        .and_then(|row| row["source"].as_str())
        .or_else(|| body["source"].as_str());
    let sections: &[&str] = match action {
        "claim_available" => &["growth", "mini_program", "streak"],
        "run_supported" => &["growth", "mini_program"],
        "accept_task" | "claim_task" | "execute_task" => match source {
            Some("growth") => &["growth"],
            Some("mini_program") => &["mini_program"],
            _ => &["growth", "mini_program"],
        },
        "travel_cycle" | "travel_depart" | "travel_claim" => &["travel", "travel_config", "buddy"],
        "buddy_adopt" => &["travel", "buddy"],
        "streak_redeem" => &["streak"],
        "makeup" => &["streak", "heatmap"],
        "lottery_draw" => &["lottery"],
        _ => &[],
    };
    let mut failures: Vec<Value> = state["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|error| sections.contains(&error["section"].as_str().unwrap_or("")))
        .map(|error| {
            operation_result(
                body["id"].as_str().unwrap_or(""),
                action,
                "failed",
                &format!(
                    "{} 状态查询失败：{}",
                    error["section"].as_str().unwrap_or(""),
                    error["message"].as_str().unwrap_or("上游未返回可用状态"),
                ),
            )
        })
        .collect();
    if matches!(action, "travel_cycle" | "travel_depart" | "travel_claim")
        && failures.is_empty()
        && !matches!(
            state.pointer("/travel/state").and_then(Value::as_str),
            Some("no_buddy" | "idle" | "traveling" | "arrived" | "daily_limit")
        )
    {
        failures.push(operation_result(
            body["id"].as_str().unwrap_or(""),
            action,
            "failed",
            "旅行状态缺失或未知，请刷新后重试",
        ));
    }
    failures
}

pub(super) async fn run(
    client: &Client,
    body: &Value,
    before: &Value,
    journal: &mut Journal,
) -> Result<Value, GrowthError> {
    let action = body["action"].as_str().unwrap_or("");
    // 批量保留正常模块的动作；单步状态查询失败时不把它误判成正常跳过。
    let mut failures = query_failures(body, before);
    if !matches!(action, "claim_available" | "run_supported") && !failures.is_empty() {
        return Ok(failures.remove(0));
    }
    // 崩溃后先按公开状态消除已完成的一次性动作，再决定是否允许下一次写入。
    let pending = journal.value["pending"].clone();
    let target = pending["target"].as_str().unwrap_or("");
    let resolved = match pending["action"].as_str() {
        Some("claim_task") => task(before, target).is_some_and(|t| yes(t, "claimed")),
        Some("accept_task") => task(before, target).is_some_and(|t| yes(t, "accepted")),
        Some("travel_depart") => before
            .pointer("/travel/state")
            .and_then(Value::as_str)
            .is_some_and(|s| s == "traveling" || s == "arrived"),
        Some("travel_claim") => {
            before
                .pointer("/travel/recordId")
                .is_some_and(|id| id.is_null() || id.as_str() == Some(target))
                && matches!(
                    before.pointer("/travel/state").and_then(Value::as_str),
                    Some("idle" | "daily_limit")
                )
        }
        Some("buddy_adopt") => before
            .pointer("/travel/buddyId")
            .is_some_and(|v| !v.is_null()),
        Some("makeup") => before
            .pointer("/streak/makeupDates")
            .and_then(Value::as_array)
            .is_some_and(|dates| dates.iter().any(|date| date == target)),
        Some("execute_task") => target.rsplit_once(':').is_some_and(|(code, old)| {
            old.parse::<f64>()
                .ok()
                .zip(task(before, code).and_then(|row| growth::number(row.get("current"))))
                .is_some_and(|(old, current)| current > old)
        }),
        _ => false,
    };
    if resolved {
        journal.clear()?;
        if !matches!(action, "claim_available" | "run_supported" | "execute_task") {
            let mut result = operation_result(
                &client.id,
                action,
                "completed",
                "官方状态确认此前步骤已结束；发奖金额仍以回执为准，未重复发送动作",
            );
            result["stateConfirmed"] = json!(true);
            return Ok(result);
        }
    }
    if !journal.value["pending"].is_null()
        && !(matches!(
            pending["action"].as_str(),
            Some("lottery_draw" | "streak_redeem")
        ) && pending["clientToken"] == journal.client_token)
    {
        return Err(GrowthError {
            status: 409,
            message: "此前动作结果尚未确认；本轮仅核对状态，未重复发送".into(),
            uncertain: true,
        });
    }
    if matches!(action, "claim_available" | "run_supported") {
        let failed_queries = failures.len();
        let mut items = failures;
        for row in before["tasks"].as_array().into_iter().flatten().take(64) {
            let execute = action == "run_supported" && yes(row, "canExecute");
            if !yes(row, "canClaim") && !execute {
                continue;
            }
            let result = task_action(
                client,
                if execute {
                    "execute_task"
                } else {
                    "claim_task"
                },
                row,
                journal,
            )
            .await;
            let uncertain = matches!(&result,Err(error) if error.uncertain);
            items.push(result.unwrap_or_else(|error| {
                operation_result(
                    &client.id,
                    "claim_task",
                    if error.uncertain {
                        "uncertain"
                    } else {
                        "failed"
                    },
                    &error.message,
                )
            }));
            if uncertain || (execute && items.len() - failed_queries >= 3) {
                break;
            }
        }
        if action == "claim_available" && journal.value["pending"].is_null() {
            for tier in before
                .pointer("/streak/tiers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|t| yes(t, "claimable"))
                .take(8)
            {
                let id = tier["id"].as_str().unwrap_or("");
                let token = journal.token("streak_redeem", id);
                let result = journal
                    .write("streak_redeem", id, &token, client.redeem(id, &token))
                    .await;
                let uncertain = matches!(&result,Err(error) if error.uncertain);
                items.push(
                    result
                        .map(|value| receipt(&client.id, "streak_redeem", &value))
                        .unwrap_or_else(|error| {
                            operation_result(
                                &client.id,
                                "streak_redeem",
                                if error.uncertain {
                                    "uncertain"
                                } else {
                                    "failed"
                                },
                                &error.message,
                            )
                        }),
                );
                if uncertain {
                    break;
                }
            }
        }
        let status = if items.iter().any(|v| v["status"] == "uncertain") {
            "uncertain"
        } else if items.iter().any(|v| v["status"] == "failed") {
            "failed"
        } else if items.iter().any(|v| v["status"] == "pending") {
            "pending"
        } else {
            "completed"
        };
        let mut result = operation_result(
            &client.id,
            action,
            status,
            &if failed_queries == 0 {
                format!("已处理 {} 个可用动作；详情见逐项结果", items.len())
            } else {
                format!("已处理 {} 个可用动作；状态查询失败 {failed_queries} 项；详情见逐项结果", items.len() - failed_queries)
            },
        );
        result["items"] = json!(items);
        return Ok(result);
    }
    if matches!(action, "accept_task" | "claim_task" | "execute_task") {
        let code = body["taskCode"].as_str().unwrap_or("");
        let row =
            task(before, code).ok_or_else(|| GrowthError::new(400, "当前账号未下发该任务"))?;
        if body
            .get("source")
            .is_some_and(|source| !source.is_null() && source != &row["source"])
        {
            return Err(GrowthError::new(400, "任务来源与当前下发状态不一致"));
        }
        return task_action(client, action, row, journal).await;
    }
    let mut effective_action = action;
    let value = match action {
        "travel_cycle" | "travel_depart" | "travel_claim" => {
            let travel = &before["travel"];
            if yes(travel, "canClaim") && action != "travel_depart" {
                effective_action = "travel_claim";
                let record = travel["recordId"]
                    .as_str()
                    .ok_or_else(|| GrowthError::new(400, "旅行记录缺失"))?;
                let token = journal.token("travel_claim", record);
                journal
                    .write("travel_claim", record, &token, client.travel_claim(record))
                    .await?
            } else if yes(travel, "canDepart") && action != "travel_claim" {
                effective_action = "travel_depart";
                let requested = growth::string_id(body.get("locationId"));
                let location = travel["locations"]
                    .as_array()
                    .and_then(|rows| {
                        rows.iter().find(|l| {
                            yes(l, "enabled")
                                && requested
                                    .as_ref()
                                    .map_or(true, |id| l["id"].as_str() == Some(id))
                        })
                    })
                    .and_then(|v| v["id"].as_str())
                    .ok_or_else(|| GrowthError::new(400, "当前目的地不可用"))?;
                let token = journal.token("travel_depart", location);
                journal
                    .write(
                        "travel_depart",
                        location,
                        &token,
                        client.travel_depart(location),
                    )
                    .await?
            } else {
                return Ok(operation_result(
                    &client.id,
                    action,
                    "not_applicable",
                    "当前旅行状态没有可执行步骤",
                ));
            }
        }
        "buddy_adopt" => {
            if !yes(&before["travel"], "canAdopt") {
                return Ok(operation_result(
                    &client.id,
                    action,
                    "not_applicable",
                    "已有猫或领养状态未确认",
                ));
            }
            if !yes(body, "agreementAccepted") {
                return Ok(operation_result(
                    &client.id,
                    action,
                    "manual_required",
                    "请先阅读并明确同意官方领养协议",
                ));
            }
            let token = journal.token(action, "first");
            journal
                .write(action, "first", &token, client.adopt())
                .await?
        }
        "streak_redeem" => {
            let tier = body["tier"].as_str().unwrap_or("");
            let retry = pending["action"] == action && pending["target"] == tier;
            if !retry
                && !before
                    .pointer("/streak/tiers")
                    .and_then(Value::as_array)
                    .is_some_and(|rows| rows.iter().any(|t| t["id"] == tier && yes(t, "claimable")))
            {
                return Ok(operation_result(
                    &client.id,
                    action,
                    "not_applicable",
                    "该档位尚未解锁或已兑换",
                ));
            }
            let token = journal.token(action, tier);
            journal
                .write(action, tier, &token, client.redeem(tier, &token))
                .await?
        }
        "makeup" => {
            let date = body["date"].as_str().unwrap_or("");
            if growth::number(before.pointer("/streak/makeupCards")).unwrap_or(0.0) < 1.0
                || !before
                    .pointer("/streak/missedDates")
                    .and_then(Value::as_array)
                    .is_some_and(|list| list.iter().any(|d| d == date))
            {
                return Ok(operation_result(
                    &client.id,
                    action,
                    "not_applicable",
                    "该日期未确认可补签或补签卡不足",
                ));
            }
            let token = journal.token(action, date);
            journal
                .write(action, date, &token, client.makeup(date))
                .await?
        }
        "lottery_draw" => {
            if !yes(&before["lottery"], "canDraw") && pending["action"] != action {
                return Ok(operation_result(
                    &client.id,
                    action,
                    "not_applicable",
                    "当前没有可用抽奖次数",
                ));
            }
            let token = journal.token(action, "single");
            journal
                .write(action, "single", &token, client.lottery(&token))
                .await?
        }
        "claim_gift" | "claim_compensation" => {
            let token = journal.token(action, "once");
            journal
                .write(
                    action,
                    "once",
                    &token,
                    client.gift(action == "claim_compensation"),
                )
                .await?
        }
        _ => return Err(GrowthError::new(400, "未知成长动作")),
    };
    let mut result = receipt(&client.id, action, &value);
    result["effectiveAction"] = json!(effective_action);
    result["operationTarget"] = journal.value["lastReceipt"]["target"].clone();
    if matches!(effective_action, "travel_depart" | "buddy_adopt" | "makeup") {
        result["status"] = json!("completed");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn result(action: &str, state: Value) -> Value {
        let body = json!({"id":"test","action":action,"taskCode":"test-task"});
        let client = Client::new(AccountStore::with_db(None), "test", "test-identity");
        let mut journal = Journal {
            key: String::new(),
            value: json!({}),
            wrote: false,
            client_token: String::new(),
            request: body.clone(),
        };
        let result = run(&client, &body, &state, &mut journal).await.unwrap();
        assert!(!journal.wrote, "状态失败或正常跳过均不发送写操作");
        result
    }

    #[tokio::test]
    async fn query_failure_is_not_a_successful_skip_and_other_sections_still_run() {
        for (action, section) in [
            ("claim_available", "growth"),
            ("claim_available", "mini_program"),
            ("claim_available", "streak"),
            ("run_supported", "growth"),
            ("travel_cycle", "travel"),
            ("travel_depart", "travel_config"),
            ("travel_claim", "buddy"),
            ("claim_task", "growth"),
            ("streak_redeem", "streak"),
        ] {
            let outcome = result(action, json!({"errors":[{"section":section,"message":"查询超时"}],"tasks":[],"travel":null})).await;
            assert_eq!(outcome["status"], "failed", "{action}/{section}");
            assert!(!outcome["receiptConfirmed"].as_bool().unwrap());
        }
        for travel in [Value::Null, json!({"state":"unknown"})] {
            assert_eq!(result("travel_cycle", json!({"errors":[],"travel":travel})).await["status"], "failed");
        }
        let partial = result("claim_available", json!({"errors":[{"section":"mini_program","message":"查询超时"}],"tasks":[{"code":"test-task","source":"growth","canClaim":true,"claimed":true}]})).await;
        assert_eq!(partial["status"], "failed");
        assert_eq!(partial["items"][0]["status"], "failed");
        assert_eq!(partial["items"][1]["status"], "already_claimed");
        assert!(partial["message"].as_str().unwrap().contains("已处理 1 个可用动作；状态查询失败 1 项"));
        for travel_state in ["no_buddy", "traveling", "daily_limit"] {
            let skipped = result("travel_cycle", json!({"errors":[{"section":"lottery","message":"无关模块失败"}],"travel":{"state":travel_state,"canDepart":false,"canClaim":false}})).await;
            assert_eq!(skipped["status"], "not_applicable");
        }
        assert_eq!(result("claim_available", json!({"errors":[],"tasks":[],"streak":{"tiers":[]}})).await["status"], "completed");
        assert_eq!(result("claim_task", json!({"errors":[{"section":"mini_program","message":"其他来源失败"}],"tasks":[{"code":"test-task","source":"growth","claimed":true}]})).await["status"], "already_claimed");
    }
}

async fn task_action(
    client: &Client,
    action: &str,
    row: &Value,
    journal: &mut Journal,
) -> Result<Value, GrowthError> {
    let code = row["code"].as_str().unwrap_or("");
    let mini = row["source"] == "mini_program";
    if yes(row, "claimed") {
        return Ok(operation_result(
            &client.id,
            action,
            "already_claimed",
            "该任务此前已领取",
        ));
    }
    let mut current = row.clone();
    if action == "accept_task" || action == "execute_task" && yes(row, "canAccept") {
        if !yes(row, "canAccept") {
            return Ok(operation_result(
                &client.id,
                action,
                "not_applicable",
                "该任务当前不需要接取",
            ));
        }
        let token = journal.token("accept_task", code);
        journal
            .write("accept_task", code, &token, client.accept(code, mini))
            .await?;
        let fresh = client.tasks(mini).await?;
        current = fresh
            .into_iter()
            .find(|t| t["code"] == code)
            .ok_or_else(|| GrowthError::new(502, "接取后任务未返回"))?;
        if !yes(&current, "accepted") {
            return Ok(operation_result(
                &client.id,
                action,
                "pending",
                "接取请求已发送，官方状态尚未生效",
            ));
        }
        if action == "accept_task" {
            let mut result =
                operation_result(&client.id, action, "completed", "官方状态确认任务已接取");
            result["stateConfirmed"] = json!(true);
            return Ok(result);
        }
        if growth::number(current.get("target")).is_none() {
            return Ok(operation_result(
                &client.id,
                action,
                "pending",
                "任务已接取，官方目标进度尚未返回；未发送对话",
            ));
        }
    }
    let mut chats = 0;
    while action == "execute_task" && !yes(&current, "canClaim") && chats < 5 {
        if !yes(&current, "canExecute") {
            return Ok(operation_result(
                &client.id,
                action,
                "manual_required",
                "请按官方指引完成真实操作，达标后可领取",
            ));
        }
        let model = growth::chat_model(code)
            .ok_or_else(|| GrowthError::new(400, "任务要求的模型当前不可用"))?;
        let old_progress = growth::number(current.get("current"));
        let step = format!("{code}:{}", old_progress.unwrap_or(0.0));
        let token = journal.token("execute_task", &step);
        let chat = journal
            .write("execute_task", &step, &token, client.chat(&model))
            .await?;
        chats += 1;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let fresh = client.tasks(mini).await?;
        current = fresh
            .into_iter()
            .find(|t| t["code"] == code)
            .unwrap_or(Value::Null);
        if !yes(&current, "canClaim")
            && (!old_progress
                .zip(growth::number(current.get("current")))
                .is_some_and(|(old, new)| new > old)
                || chats >= 5)
        {
            let mut result = operation_result(
                &client.id,
                action,
                "pending",
                "真实对话已完成，官方任务尚未达标；进度未推进时停止继续对话",
            );
            result["actualAction"] = chat;
            result["conversationCount"] = json!(chats);
            return Ok(result);
        }
    }
    if !yes(&current, "canClaim") {
        return Ok(operation_result(
            &client.id,
            action,
            "not_applicable",
            "官方任务尚未达到领取条件",
        ));
    }
    let token = journal.token("claim_task", code);
    let value = journal
        .write("claim_task", code, &token, client.claim(code, mini))
        .await?;
    let mut result = receipt(&client.id, action, &value);
    result["taskCode"] = json!(code);
    if let Ok(fresh) = client.tasks(mini).await {
        result["stateConfirmed"] =
            json!(fresh.iter().any(|t| t["code"] == code && yes(t, "claimed")));
    }
    Ok(result)
}
