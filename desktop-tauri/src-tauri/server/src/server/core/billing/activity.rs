//! 运营活动与组合动作（对照 Node 版 workbuddy-billing.mjs 的「活动」与
//! checkinAndReport 部分）。
//!
//!   GET  /v2/activity/workbuddy/banner    运营 banner（需客户端白名单头）
//!   GET  /v2/activity/ambassador/status    大使状态
//!   签到 + 查余额的组合动作（"签到并回报最新积分"）
//!   WorkBuddy 国际版的每日活跃任务（探测 → 条件领取 → 免费模型保活）
//!
//! 从 billing/mod.rs 拆出（单文件行数约定）。banner / ambassador **吞掉所有错误
//! 返回 null**（装饰性内容，拉不到不该让首屏报错）；组合动作里的签到失败则收敛成
//! `{success:false}` 交给前端显示成 warn，只有国际版无签到是硬错误 ——
//! 而国际版现在走的是下面的 `workbuddy_daily_activity`（活跃任务不是普通签到，
//! 它的所有失败都收敛为结果字段，不让自动签到任务因为上游活动开关而中断）。

use serde_json::{json, Map, Value};

use crate::server::core::endpoints::RESPONSE_CODE_OK;
use crate::server::logging;

use super::request::{
    CallOptions, ACTIVITY_AMBASSADOR, ACTIVITY_BANNER,
    BILLING_ACTIVITY_CHECKIN_STATUS, BILLING_DAILY_CHECKIN, js_truthy,
};
use super::{assert_checkin_supported, normalize_activity_status, normalize_daily_claim, BillingError, BillingService};
use super::activity_response::consume_activity_response;

/// 国际版每日活跃任务的**执行粒度**（对应签到中心的三颗手动按钮；
/// 定时与批量签到恒走 [`WorkbuddyActivity::Full`]，粒度细分只是手动入口的事）。
///
/// 三种粒度的边界就是"哪几步要打上游"：
///   * `Full`：探测 + 条件领取 + 保活 —— 一次点击完整跑一遍；
///   * `Claim`：探测 + 条件领取 —— 只想要奖励，不多打保活那发对话；
///   * `Keepalive`：只保活 —— 活动没开时只维持账号活跃，不打探测与领取。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkbuddyActivity {
    Full,
    Claim,
    Keepalive,
}

impl WorkbuddyActivity {
    /// API 层传来的 mode 字符串 → 粒度。未知值回落 Full —— 手动入口的
    /// 缺省行为是"完整跑一遍"，比"悄悄少做一步"更接近点击者的本意。
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("claim") => Self::Claim,
            Some("keepalive") => Self::Keepalive,
            _ => Self::Full,
        }
    }

    /// 结果行里回带的 mode（前端排查"这轮实际跑了哪几步"用）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Claim => "claim",
            Self::Keepalive => "keepalive",
        }
    }
}

impl BillingService {
    // ─── 活动 ───────────────────────────────────────────────

    /// 运营 banner：需客户端白名单头，否则被拦截（返回 null，与桌面端一致）。
    ///
    /// 这里**吞掉所有错误并返回 null** —— 与 Node 版一致：banner 是页面上
    /// 的一块装饰，拉不到就不显示，不该让首屏出现一条红色报错。
    pub async fn get_activity_banner(&self, session: Option<&Value>) -> Value {
        let active = match session {
            Some(session) => Some(session.clone()),
            None => match self.require_session().await {
                Ok(session) => Some(session),
                Err(error) => {
                    logging::log("[Activity]", &format!("banner 拉取失败: {}", error.message));
                    return Value::Null;
                }
            },
        };
        let result = match self
            .call_billing(
                ACTIVITY_BANNER,
                CallOptions {
                    session: active.as_ref(),
                    expect_code_ok: false,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                logging::log("[Activity]", &format!("banner 拉取失败: {}", error.message));
                return Value::Null;
            }
        };
        if result.code != Some(RESPONSE_CODE_OK) || result.data.is_null() {
            return Value::Null;
        }
        let raw = result.data;
        // action 只在 type 是 open_url / switch_model 时透出（其余类型前端不认识）。
        // 键的取舍照抄 Node 的对象字面量：`url` / `model_id` / `fallback_url`
        // 在缺失时是 undefined → **键整个消失**。实测上游的 open_url 就是
        // `{type, label, fallback_url}`（没有 url），所以响应里也不该有 url 键 ——
        // 前端 `action.url` 两种形态都读不到东西，但 `'url' in action` 会分叉。
        let action = raw.get("action").and_then(|action| {
            let action_type = action.get("type").and_then(Value::as_str).unwrap_or("");
            if action_type != "open_url" && action_type != "switch_model" {
                return None;
            }
            let mut object = Map::new();
            object.insert("type".to_string(), Value::String(action_type.to_string()));
            object.insert(
                "label".to_string(),
                action.get("label").cloned().unwrap_or(Value::Null),
            );
            if let Some(url) = action.get("url") {
                object.insert("url".to_string(), url.clone());
            }
            if let Some(model_id) = action.get("model_id") {
                object.insert("modelId".to_string(), model_id.clone());
            }
            if let Some(fallback) = action.get("fallback_url") {
                object.insert("fallbackUrl".to_string(), fallback.clone());
            }
            Some(Value::Object(object))
        });
        let mut banner = Map::new();
        banner.insert(
            "id".to_string(),
            raw.get("activity_id").cloned().unwrap_or(Value::Null),
        );
        // `raw.activity_online_status && raw.status === 'None'` ——
        // 前者是 JS 真值判定，后者是严格字符串比较
        banner.insert(
            "active".to_string(),
            Value::Bool(
                raw.get("activity_online_status")
                    .map(js_truthy)
                    .unwrap_or(false)
                    && raw.get("status").and_then(Value::as_str) == Some("None"),
            ),
        );
        banner.insert(
            "bannerContent".to_string(),
            raw.get("banner_content").cloned().unwrap_or(Value::Null),
        );
        banner.insert(
            "link".to_string(),
            raw.get("detail_link").cloned().unwrap_or(Value::Null),
        );
        banner.insert(
            "level".to_string(),
            raw.get("activity_level").cloned().unwrap_or(Value::Null),
        );
        banner.insert(
            "startTime".to_string(),
            raw.get("start_time").cloned().unwrap_or(Value::Null),
        );
        banner.insert(
            "endTime".to_string(),
            raw.get("end_time").cloned().unwrap_or(Value::Null),
        );
        // Node 的 `let action` 在类型不认识时保持 undefined → 键整个消失；
        // 能识别时一定是个对象（上面已构造）
        if let Some(action) = action {
            banner.insert("action".to_string(), action);
        }
        banner.insert("raw".to_string(), raw);
        Value::Object(banner)
    }

    /// 大使（推广）状态。同样吞错返回 null（verbose 级别日志）。
    pub async fn get_ambassador_status(&self, session: Option<&Value>) -> Value {
        let active = match session {
            Some(session) => Some(session.clone()),
            None => match self.require_session().await {
                Ok(session) => Some(session),
                Err(error) => {
                    logging::verbose(
                        "[Activity]",
                        &format!("ambassador 状态拉取失败: {}", error.message),
                    );
                    return Value::Null;
                }
            },
        };
        let result = match self
            .call_billing(
                ACTIVITY_AMBASSADOR,
                CallOptions {
                    session: active.as_ref(),
                    expect_code_ok: false,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                logging::verbose(
                    "[Activity]",
                    &format!("ambassador 状态拉取失败: {}", error.message),
                );
                return Value::Null;
            }
        };
        if result.code != Some(RESPONSE_CODE_OK) || result.data.is_null() {
            return Value::Null;
        }
        json!({
            "isAmbassador": result
                .data
                .get("isAmbassador")
                .map(js_truthy)
                .unwrap_or(false),
            "raw": result.data,
        })
    }

    // ─── 组合动作 ───────────────────────────────────────────

    /// 签到 + 查余额的组合动作（"签到并回报最新积分"）。
    ///
    /// 已签到时不重复领取，直接返回当前额度。
    /// 国际版没有签到活动：不吞成「签到失败」，而是直接抛出，
    /// 让调用方拿到明确原因（`checkinStatus` 那一步也会先抛）。
    ///
    /// 并发语义：Node 用 `Promise.all` 并发跑「查状态 + 领取」。
    /// 这里必须**顺序**执行 —— 两者共享同一个底层连接池没问题，
    /// 但并发会在上游留下两条几乎同时到达的签到请求，而 Node 版
    /// `checkinAndReport` 的注释明确说「额度可能因签到变化，稍等一下再查」，
    /// 顺序执行才保证 `usage` 反映的是领取之后的额度。
    pub async fn checkin_and_report(
        &self,
        session: Option<&Value>,
        locale: Option<&str>,
    ) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };

        // WorkBuddy 国际版没有国内版的普通签到接口；组合入口沿用同一条活跃任务
        // 链路，并把探测/领取/保活结果一起返回，避免重复打上游。
        if super::is_international(&active) {
            let activity = self.workbuddy_daily_activity(&active, WorkbuddyActivity::Full).await;
            let usage = match self.query_credits_summary(Some(&active), locale).await {
                Ok(value) => value,
                Err(error) => json!({ "error": error.message }),
            };
            return Ok(json!({
                "checkinStatus": activity.get("status").cloned().unwrap_or(Value::Null),
                "claim": activity.get("claim").cloned().unwrap_or(Value::Null),
                "activity": activity.get("activity").cloned().unwrap_or(Value::Null),
                "usage": usage,
            }));
        }
        assert_checkin_supported(&active)?;

        // 查状态与领取：Node 并发，这里顺序（理由见上）。
        // 领取失败不抛出 —— 收敛成 `{success:false, code:-1, msg}`，与 Node 的
        // `.catch(error => ({ success:false, code:-1, msg: error.message }))` 一致
        let status = self.get_checkin_status(Some(&active)).await?;
        let claim = match self.claim_daily_checkin(Some(&active)).await {
            Ok(value) => value,
            Err(error) => json!({ "success": false, "code": -1, "msg": error.message }),
        };
        // 额度可能因签到变化，稍等一下再查（对照 Node 的 sleep(500)）
        if claim
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let usage = match self.query_credits_summary(Some(&active), locale).await {
            Ok(value) => value,
            Err(error) => json!({ "error": error.message }),
        };
        Ok(json!({
            "checkinStatus": status,
            "claim": claim,
            "usage": usage,
        }))
    }

    // ─── WorkBuddy 国际版：每日活跃任务 ─────────────────────

    /// 国际版每日活跃任务：活动探测 → 条件领取 → 免费模型保活。
    ///
    /// [`WorkbuddyActivity`] 决定这次跑到哪一步 —— 界面上「保活 / 领取 /
    /// 保活+领取」三颗按钮各对应一种粒度；**定时与批量签到恒走 [`WorkbuddyActivity::Full`]**
    /// （自动任务要完整跑一遍，粒度细分只是手动入口的事）。
    ///
    /// 参考客户端把这三步挂在既有签到调度器上（rockswang/wild-work 的执行顺序），
    /// 本仓照同一顺序编排，并有三条纪律：
    ///   * 国际版活动未开放属于**正常状态**，所有探测/保活失败都收敛为结果字段
    ///     并记 verbose 日志，不让自动签到任务因为上游活动开关而中断；
    ///   * 活跃保活成功**不伪造普通签到成功** —— 调用方只有在 `claim.success`
    ///     或 `alreadyCompleted` 时才落 `checkinAt`（见 `billing::checkin` 的
    ///     `checkin_completed_today`），保活读数单独放在 `activity` 段；
    ///   * 领取只在「活动开放且今日未领」时打：`todayCheckedIn` 直接给
    ///     `alreadyCompleted`（上游幂等，但少一发是一发），活动没开就不打领取。
    ///
    /// 保活的模型链来自 `keepalive::models()`（签到中心可自定义，空回落缺省链），
    /// 逐个尝试、第一个成功的收口（见 `poke_daily_activity`）。
    pub async fn workbuddy_daily_activity(&self, session: &Value, mode: WorkbuddyActivity) -> Value {
        let balance_before = self.log_workbuddy_activity_balance(session, "前").await;

        let (status_available, active, today_checked_in, status_value) = if mode == WorkbuddyActivity::Keepalive {
            (false, false, false, Value::Null)
        } else {
        let status = self
            .call_billing(
                BILLING_ACTIVITY_CHECKIN_STATUS,
                CallOptions {
                    session: Some(session),
                    expect_code_ok: false,
                    ..Default::default()
                },
            )
            .await;

        match status {
            Ok(result) => {
                let status_available = result.code == Some(RESPONSE_CODE_OK) && !result.data.is_null();
                let active = status_available && result
                    .data
                    .get("active")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let today_checked_in = status_available && result
                    .data
                    .get("today_checked_in")
                    .or_else(|| result.data.get("todayCheckedIn"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !status_available {
                    logging::verbose(
                        "[Checkin]",
                        &format!(
                            "WorkBuddy 国际版签到活动状态查询失败（code={}）",
                            result.code.unwrap_or(-1)
                        ),
                    );
                }
                (
                    status_available,
                    active,
                    today_checked_in,
                    normalize_activity_status(&result),
                )
            }
            Err(error) => {
                logging::verbose(
                    "[Checkin]",
                    &format!("WorkBuddy 国际版活跃探测失败: {}", error.message),
                );
                (
                    false,
                    false,
                    false,
                    json!({
                        "active": false,
                        "todayCheckedIn": false,
                        "statusAvailable": false,
                        "code": -1,
                        "msg": error.message,
                    }),
                )
            }
        }
        };

        let mut claim = if mode == WorkbuddyActivity::Keepalive {
            json!({"success": false, "code": 0, "msg": "仅执行保活，未领取奖励"})
        } else if !status_available {
            json!({"success": false, "status": "failed", "code": -1,
                "msg": status_value.get("msg").and_then(Value::as_str).filter(|text| !text.is_empty()).unwrap_or("签到活动状态查询失败")})
        } else if today_checked_in {
            json!({
                "success": false,
                "code": 0,
                "msg": "今日已领取",
                "alreadyCompleted": true,
            })
        } else if active {
            match self
                .call_billing(
                    BILLING_DAILY_CHECKIN,
                    CallOptions {
                        session: Some(session),
                        expect_code_ok: false,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(result) => normalize_daily_claim(result),
                Err(error) => {
                    logging::verbose(
                        "[Checkin]",
                        &format!("WorkBuddy 国际版活跃奖励领取失败: {}", error.message),
                    );
                    json!({ "success": false, "code": -1, "msg": error.message })
                }
            }
        } else {
            json!({ "success": false, "code": 0, "msg": "活动未开启" })
        };

        let (api_poke_succeeded, poke_model, web) = if mode != WorkbuddyActivity::Claim {
            let (api, model) = self.poke_daily_activity(session).await;
            let models = super::keepalive::models();
            let web = super::web_activity::run(session, model.as_deref().unwrap_or(&models[0])).await;
            logging::log("[Checkin]", &format!(
                "WorkBuddy 国际版网页保活（status={}，outputChunks={}，success={}，error={}）",
                web.status, web.output_chunks, web.success, web.error.as_deref().unwrap_or("none"),
            ));
            (Some(api), model, Some(web))
        } else {
            (None, None, None)
        };
        let poke_succeeded = web.as_ref().map(|result| result.success);
        let balance_after = self.log_workbuddy_activity_balance(session, "执行后").await;
        let balance_delta = balance_before
            .as_ref()
            .zip(balance_after.as_ref())
            .and_then(|(before, after)| {
                Some(after.get("totalLeft")?.as_i64()? - before.get("totalLeft")?.as_i64()?)
            });
        let claim_succeeded = claim
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if mode == WorkbuddyActivity::Full && status_available && !claim_succeeded && !today_checked_in && !active {
            claim["msg"] = Value::String(if poke_succeeded == Some(true) {
                "网页会话完成，日活奖励尚未确认".to_string()
            } else {
                format!("网页保活失败：{}", web.as_ref().and_then(|result| result.error.as_deref()).unwrap_or("会话未完成"))
            });
        }
        if poke_succeeded == Some(false) {
            logging::verbose("[Checkin]", "WorkBuddy 国际版网页活跃保活未成功");
        }
        logging::log(
            "[Checkin]",
            &format!(
                "WorkBuddy 国际版活跃结果（账号={}，statusAvailable={status_available}，active={active}，todayCheckedIn={today_checked_in}，claimSuccess={claim_succeeded}，pokeSucceeded={poke_succeeded:?}，balanceDelta={}，rewardStatus=unverified；余额为整数摘要，日活奖励资格及到账以上游结算为准）",
                session.pointer("/account/nickname").and_then(Value::as_str)
                    .or_else(|| session.pointer("/account/uid").and_then(Value::as_str)).unwrap_or("unknown"),
                balance_delta.map(|v| v.to_string()).unwrap_or_else(|| "unknown".to_string()),
            ),
        );

        json!({
            "status": status_value,
            "claim": claim,
            "activity": {
                "mode": mode.as_str(),
                "active": if mode == WorkbuddyActivity::Keepalive { Value::Null } else { json!(active) },
                "todayCheckedIn": if mode == WorkbuddyActivity::Keepalive { Value::Null } else { json!(today_checked_in) },
                "statusAvailable": if mode == WorkbuddyActivity::Keepalive { Value::Null } else { json!(status_available) },
                "pokeSucceeded": poke_succeeded,
                "apiPokeSucceeded": api_poke_succeeded,
                "pokeModel": poke_model,
                "web": web,
                "balanceBefore": balance_before,
                "balanceAfter": balance_after,
                "balanceDelta": balance_delta,
                "rewardStatus": "unverified",
            },
        })
    }

    /// 记录活跃保活前后的积分快照，便于核对活动奖励是否入账。
    ///
    /// 余额查询是旁路诊断动作：失败只记录日志，不改变活跃任务的结果。
    async fn log_workbuddy_activity_balance(&self, session: &Value, phase: &str) -> Option<Value> {
        let account = session
            .get("account")
            .and_then(Value::as_object);
        let account_label = account
            .and_then(|value| value.get("nickname"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                account
                    .and_then(|value| value.get("uid"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or("unknown");

        match self.query_credits_summary(Some(session), None).await {
            Ok(summary) => {
                logging::log(
                    "[Checkin]",
                    &format!(
                        "WorkBuddy 国际版活跃保活{phase}余额（账号={account_label}，totalLeft={}，planLeft={}，bonusLeft={}）",
                        summary.get("totalLeft").unwrap_or(&Value::Null),
                        summary.get("planLeft").unwrap_or(&Value::Null),
                        summary.get("bonusLeft").unwrap_or(&Value::Null),
                    ),
                );
                Some(summary)
            }
            Err(error) => {
                logging::log(
                    "[Checkin]",
                    &format!(
                        "WorkBuddy 国际版活跃保活{phase}余额查询失败（账号={account_label}）：{}",
                        error.message
                    ),
                );
                None
            }
        }
    }

    async fn poke_daily_activity(&self, session: &Value) -> (bool, Option<String>) {
        const TIMEOUT_MS: u64 = 20_000;
        for model in super::keepalive::models() {
            let plan = match crate::server::core::providers::workbuddy::build_daily_activity_request(
                session, &model,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    logging::verbose("[Checkin]", &error);
                    continue;
                }
            };
            let response = match tokio::time::timeout(
                std::time::Duration::from_millis(TIMEOUT_MS),
                crate::server::core::upstream::request::send_chat_request(&plan),
            )
            .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    logging::verbose(
                        "[Checkin]",
                        &format!(
                            "WorkBuddy 国际版活跃模型 {model} 请求失败: {}",
                            error.message
                        ),
                    );
                    continue;
                }
                Err(_) => {
                    logging::verbose(
                        "[Checkin]",
                        &format!("WorkBuddy 国际版活跃模型 {model} 请求超时"),
                    );
                    continue;
                }
            };
            let status = response.status();
            if !status.is_success() {
                logging::verbose("[Checkin]", &format!(
                    "WorkBuddy 国际版活跃模型 {model} 返回 HTTP {}", status.as_u16()
                ));
                continue;
            }
            let body_result = tokio::time::timeout(
                std::time::Duration::from_millis(TIMEOUT_MS),
                consume_activity_response(response, 1 << 20),
            )
            .await
            .unwrap_or(Err("响应流读取超时"));
            if let Err(reason) = body_result {
                logging::verbose(
                    "[Checkin]",
                    &format!(
                        "WorkBuddy 国际版活跃模型 {model} 返回 HTTP {}，有效对话校验失败：{reason}",
                        status.as_u16()
                    ),
                );
                continue;
            }
            logging::verbose(
                "[Checkin]",
                &format!("WorkBuddy 国际版活跃保活有效对话完成（model={model}，日活奖励尚未确认）"),
            );
            return (true, Some(model));
        }
        (false, None)
    }

}
