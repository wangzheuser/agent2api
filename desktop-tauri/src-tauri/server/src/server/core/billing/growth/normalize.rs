use super::GrowthError;
use serde_json::{json, Value};

pub fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|n| n.is_finite() && *n >= 0.0)
}
pub fn string_id(value: Option<&Value>) -> Option<String> {
    value.and_then(|v| {
        v.as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    })
}
pub fn rewards(value: &Value) -> Value {
    json!({"credits":number(value.get("reward_credit").or_else(||value.get("credit"))),
        "energy":number(value.get("reward_energy").or_else(||value.get("energy"))),
        "buddy":value.get("reward_buddy").and_then(Value::as_bool).map(|b|if b {1}else{0}),
        "lotteryChances":number(value.get("chances")),"makeupCards":number(value.get("cards"))})
}
fn time(value: Option<&Value>) -> Option<i64> {
    value.and_then(|v| {
        number(Some(v))
            .filter(|n| *n > 0.0)
            .map(|n| {
                if n < 1e12 {
                    (n * 1000.0) as i64
                } else {
                    n as i64
                }
            })
            .or_else(|| {
                v.as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.timestamp_millis())
            })
    })
}
pub(super) fn tasks(value: &Value, source: &str) -> Result<Vec<Value>, GrowthError> {
    let rows = value
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| GrowthError::new(502, "任务列表结构异常"))?;
    if rows.len() > 512 {
        return Err(GrowthError::new(502, "任务列表超过上限"));
    }
    Ok(rows.iter().filter_map(|row| {
        let code=row.get("task_code").and_then(Value::as_str).filter(|s|super::valid_code(s))?;
        let accept=row.get("accept_status").and_then(Value::as_str).unwrap_or("");
        let current=number(row.pointer("/progress/current").or_else(||row.get("current")));
        let target=number(row.pointer("/progress/target").or_else(||row.get("target")));
        let claimed=accept=="claimed" || row.get("claimed").and_then(Value::as_bool)==Some(true);
        let locked=row.get("locked").and_then(Value::as_bool)==Some(true);
        let expires=time(row.get("valid_end").or_else(||row.get("expires_at")));
        let now=crate::server::logging::now_ms();
        let expired=expires.is_some_and(|end|end<=now);
        let future=time(row.get("valid_start")).is_some_and(|start|start>now);
        let completed=current.zip(target).is_some_and(|(c,t)|t>0.0 && c>=t);
        let accepted=matches!(accept,"accepted"|"in_progress"|"claimed"|"completed");
        let claimable=!claimed && !locked && !expired && !future && (completed || row.get("claimable").and_then(Value::as_bool)==Some(true));
        let executable=!claimed && !locked && !expired && !future && !completed && source=="growth" && super::CHAT_CODES.contains(&code) && (!accepted || target.is_some()) && super::chat_model(code).is_some();
        let state=if claimed {"claimed"} else if expired {"expired"} else if locked || future {"locked"} else if claimable {"claimable"} else if accepted {"active"} else {"available"};
        let url=row.get("jump_url").and_then(Value::as_str).filter(|s|s.starts_with("https://www.workbuddy.cn/") || s.starts_with("workbuddy://"));
        Some(json!({"code":code,"title":row.get("title").and_then(Value::as_str).unwrap_or(code),"source":source,"state":state,
            "current":current,"target":target,"accepted":accepted,"claimed":claimed,"expiresAt":expires,"rewards":rewards(row),
            "canAccept":!accepted && !claimed && !locked && !expired && !future,"canClaim":claimable,"canExecute":executable,
            "execution":if executable {"automatic"} else {"manual"},"reason":if code=="Model_chat_GLM5.2" && !executable {"指定模型当前不可用或任务不具备执行条件，请按官方指引操作"} else {row.get("task_desc").or_else(||row.get("description")).and_then(Value::as_str).unwrap_or("请在官方客户端完成真实操作，达标后可领取")},"actionUrl":url}))
    }).collect())
}
pub(super) fn travel(status: &Value, config: &Value, buddy: &Value) -> Value {
    let has_buddy = buddy.get("buddy").is_some_and(|v| v.is_object());
    let no_buddy = buddy.get("buddy") == Some(&Value::Null);
    let raw = status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let limited = status.get("daily_limit_reached").and_then(Value::as_bool) == Some(true);
    let state = if no_buddy {
        "no_buddy"
    } else if limited && raw == "idle" {
        "daily_limit"
    } else if matches!(raw, "idle" | "traveling" | "arrived") {
        raw
    } else {
        "unknown"
    };
    let locations:Vec<Value>=config.get("locations").and_then(Value::as_array).into_iter().flatten().filter_map(|row|{
        let id=string_id(row.get("id"))?;
        Some(json!({"id":id,"name":row.get("name"),"enabled":row.get("enabled").and_then(Value::as_bool)!=Some(false),
            "durationSecondsMin":number(row.get("duration_hours_min")).map(|n|n*3600.0),"durationSecondsMax":number(row.get("duration_hours_max")).map(|n|n*3600.0),
            "rewardCreditsMin":number(row.get("reward_credit_min")),"rewardCreditsMax":number(row.get("reward_credit_max"))}))
    }).collect();
    let record = string_id(status.get("record_id")).filter(|s| s != "0");
    let can_depart =
        state == "idle" && has_buddy && !limited && locations.iter().any(|l| l["enabled"] == true);
    json!({"state":state,"recordId":record,"buddyId":string_id(status.get("buddy_id")).or_else(||string_id(buddy.pointer("/buddy/instance_id"))).or_else(||string_id(buddy.pointer("/buddy/id"))),
        "buddyName":buddy.pointer("/buddy/name"),"locationId":string_id(status.get("location_id").or_else(||status.pointer("/location/id")).or_else(||status.get("location"))),"arrivesAt":time(status.get("arrive_at")),"serverNow":time(status.get("server_now")),
        "completedToday":null,"dailyLimit":null,"canAdopt":no_buddy,"agreementRequired":no_buddy,"agreementUrl":"https://www.workbuddy.cn/profile/growth-center",
        "canDepart":can_depart,"canClaim":state=="arrived" && record.is_some(),"locations":locations,"rewards":rewards(status)})
}
pub(super) fn streak(value: &Value, heatmap: &Value) -> Value {
    let status = &value["redemption_status"];
    let tiers:Vec<Value>=status.get("tiers").and_then(Value::as_array).into_iter().flatten().filter_map(|row|{
        let id=string_id(row.get("tier"))?;
        let state=status.get(format!("tier_{id}_status")).and_then(Value::as_str).unwrap_or("unknown");
        Some(json!({"id":id,"requiredDays":number(row.get("days")),"state":state,
            "claimable":matches!(state,"available"|"redeemable"|"unlocked"|"claimable"|"ready"),"claimed":matches!(state,"claimed"|"redeemed"),"rewards":rewards(row)}))
    }).collect();
    let today = heatmap.pointer("/today/date").and_then(Value::as_str);
    let timezone = value.get("timezone").and_then(Value::as_str);
    let launch = value.get("launch_date").and_then(Value::as_str);
    let already = value
        .pointer("/streak/makeup_dates")
        .and_then(Value::as_array);
    let missed: Vec<Value> = heatmap
        .get("cells")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|cell| {
            let date = cell.get("date").and_then(Value::as_str)?;
            if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err()
                || timezone.is_none()
                || !today.is_some_and(|t| date < t)
                || launch.is_some_and(|l| date < l)
                || number(cell.get("score")) != Some(0.0)
                || already.is_some_and(|list| list.iter().any(|v| v.as_str() == Some(date)))
            {
                return None;
            }
            Some(Value::String(date.into()))
        })
        .collect();
    json!({"days":number(value.pointer("/streak/days")),"makeupCards":number(value.pointer("/makeup_cards/balance")),
        "missedDates":missed,"makeupDates":already,"timezone":timezone,"tiers":tiers,"nextTier":value.pointer("/streak/next_tier"),
        "nextTierRemaining":number(value.pointer("/streak/next_tier_remaining"))})
}
