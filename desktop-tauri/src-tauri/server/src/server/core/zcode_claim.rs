//! ZCode 手动与定时领取共用的账号上下文及落库逻辑。
use super::account_store::AccountStore;
use super::providers::zcode::{
    claim::{self, ClaimFailure, ClaimOutcome},
    region::Region,
};
use super::task_state::{self, Claim, ManualBackoff};
use crate::server::{config, logging};
use serde_json::{json, Value};

const RISK_BLOCK_RETRY_MS: i64 = 6 * 60 * 60_000;

/// 从账号记录里取出领取链路要的三样东西。
///
/// `jwt` 与 `deviceMid` **不走投影列**（它们是本家独有的字段，账号存储的
/// 列只认各家共用的那几个），因此这里读的是记录的 JSON 形态而不是会话形态。
/// `accessToken` 那条路（转发）才走会话（见 `providers::zcode::adapter`）。
pub(crate) struct ClaimTarget {
    /// 地区（决定上游 `provider` 取值与提示文案）
    pub region: Region,
    /// 套餐令牌（领取的必要条件）
    pub jwt: String,
    /// 设备标识（上游硬要求，UUID 形态；缺失时已由存储层生成并落盘）
    pub device_mid: Option<String>,
    /// 账号展示名（日志与错误文案用）
    pub name: String,
}

/// 载入目标账号；缺账号或缺 jwt 时返回一句给用户的话
///
/// 状态码用 `i32`（与 `management_error` 同型），避免在每个调用点反复转换。
///
/// ── 设备标识为什么在这里「补齐」而不是报错 ────────────────────
/// 上游把它当**硬参数**（缺了就是 3001，见 `providers::zcode::claim` 的模块头），
/// 而手工粘贴凭证建的老账号可能没有这个字段。这不是用户能自己修的东西
/// （他不知道该填什么，也不该被要求填一个 UUID），所以由存储层生成一次并落盘
/// （`zcode_device_mid_or_create`），此后一直复用同一个值。
pub(crate) fn load_target(
    store: &AccountStore,
    account_id: &str,
) -> Result<ClaimTarget, (i32, String)> {
    let record = store
        .zcode_account_record(account_id)
        .ok_or_else(|| (404, "找不到该 ZCode 账号".to_string()))?;
    let provider_id = record.get("provider").and_then(Value::as_str).unwrap_or("");
    let region = Region::from_provider_id(provider_id)
        .ok_or_else(|| (400, "该账号不是 ZCode 账号".to_string()))?;
    let text = |key: &str| {
        record
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let jwt = text("jwt");
    let name = text("name");
    if jwt.is_empty() {
        // 明确说清是哪一样缺了：本家有两个互不替代的凭证，用户很容易以为
        // 「填了一个就够」（见 `credentials.rs` 的模块头）
        return Err((
            400,
            format!(
                "该 ZCode {}账号没有套餐令牌（jwt），无法领取。请重新用「网页登录」添加，\
                 或在账号里补填 Coding Plan JWT",
                region.label()
            ),
        ));
    }
    let id = text("id");
    let device_mid = store
        .zcode_device_mid_or_create(if id.is_empty() { account_id } else { &id })
        .filter(|value| !value.trim().is_empty());
    Ok(ClaimTarget {
        region,
        jwt,
        device_mid,
        name,
    })
}

/// 领取链路可按地区独立直连；验证码配置、生产者与领取共用此选择。
/// 推理与余额仍使用账号出口。默认沿用账号代理，部署可显式设置
/// ZCODE_CLAIM_DIRECT=1 / ZCODE_INTL_CLAIM_DIRECT=1。
pub(crate) fn claim_proxy(
    store: &AccountStore,
    account_id: &str,
) -> Option<crate::server::core::proxies::ResolvedProxy> {
    let record = store.zcode_account_record(account_id)?;
    let region = record.get("provider").and_then(Value::as_str)
        .and_then(Region::from_provider_id)?;
    let direct = std::env::var(format!("{}CLAIM_DIRECT", region.env_prefix())).ok();
    let session = store.get_session_by_id(account_id)?.session;
    select_claim_proxy(&session, direct.as_deref())
}

fn select_claim_proxy(
    session: &Value,
    direct: Option<&str>,
) -> Option<crate::server::core::proxies::ResolvedProxy> {
    if direct.is_some_and(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true")) {
        None
    } else {
        crate::server::core::proxies::session_proxy(session)
    }
}

/// 台账只认套餐 ID，不用自然日过滤；昨天领过的同一份套餐也不重复提交。
fn already_recorded(record: &Value, plan_id: &str) -> bool {
    record
        .get("claimPlans")
        .and_then(|v| v.get(plan_id))
        .and_then(Value::as_i64)
        .is_some_and(|at| at > 0)
}

fn eligible(account: &Value) -> bool {
    account
        .get("provider")
        .and_then(Value::as_str)
        .and_then(Region::from_provider_id)
        .is_some()
        && account.get("enabled").and_then(Value::as_bool) != Some(false)
        && account.get("canClaim").and_then(Value::as_bool) == Some(true)
}

/// 先切换再记台账。存储失败时下轮仍可凭上游 already_claimed 恢复这一步。
fn complete(store: &AccountStore, id: &str, plan_id: &str) -> Result<(), String> {
    store
        .update_zcode_plan(id, &json!(super::providers::zcode::PLAN_START))
        .map_err(|error| error.message)?;
    if !store.mark_zcode_claim(id, plan_id, logging::now_ms()) {
        return Err("领取成功，但领取台账保存失败".into());
    }
    Ok(())
}

#[derive(Default)]
struct Report {
    checked: usize,
    claimed: usize,
    already: usize,
    skipped: usize,
    failed: usize,
    /// 活动投放中没有领取动作。与「已领过」「没资格」都不是
    /// 一码事：上游不给新套餐时本地无可动作，但**必须说出来**，否则一轮自动领取
    /// 只留下一句「领取 0 份」，用户分不清是排队等着、已经领过了，还是逻辑坏了。
    empty_preview: usize,
    error: Option<String>,
}

/// 将上游业务码和短消息带进自动任务日志，避免所有未识别响应都只显示“未知失败”。
fn failure_detail(failure: ClaimFailure, code: i64, message: &str) -> String {
    let message = message.trim().replace('\r', " ").replace('\n', " ");
    let message: String = message.chars().take(200).collect();
    if message.is_empty() {
        format!("{}（上游码 {code}）", failure.label())
    } else {
        format!("{}（上游码 {code}：{message}）", failure.label())
    }
}

impl Report {
    /// 整轮口径：定时任务页的那一行、`run_auto` 的日志。
    fn summary(&self) -> String {
        let base = format!(
            "检查 {} 个账号，领取 {} 份，已领取 {} 份，跳过 {} 项，失败 {} 项",
            self.checked, self.claimed, self.already, self.skipped, self.failed
        );
        if self.empty_preview > 0 {
            return format!(
                "{base}，无可领套餐 {} 个（上游活动投放未下发领取动作）",
                self.empty_preview
            );
        }
        base
    }

    /// 账号口径：签到中心那张卡上的一行字，要短、要能直接回答「为什么没领到」。
    ///
    /// 与 [`Report::summary`] 分开是因为两者出现的容器完全不同：卡片放不下
    /// 「检查 1 个账号，领取 0 份，…」这种整轮计数。
    fn account_note(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if self.empty_preview > 0 {
            return "本期无可领套餐（上游活动投放未下发领取动作）".to_string();
        }
        if self.claimed > 0 {
            return format!("已领取 {} 份", self.claimed);
        }
        if self.already > 0 {
            return format!("本期已领过 {} 份", self.already);
        }
        if self.skipped > 0 {
            return format!("{} 份仍在重试冷却中", self.skipped);
        }
        if self.failed > 0 {
            return format!("领取失败 {} 份", self.failed);
        }
        "本轮无可动作".to_string()
    }
}

async fn request_claim(
    store: &AccountStore,
    id: &str,
    target: &ClaimTarget,
    plan_id: &str,
    manual: bool,
) -> Result<Option<ClaimOutcome>, String> {
    let proxy = claim_proxy(store, id);
    let config = claim::captcha_config(target.region, proxy.as_ref())
        .await
        .map_err(|error| format!("获取验证码配置失败（HTTP {}）", error.status_code))?;
    let proof = match config.filter(|config| config.enabled) {
        Some(config) => crate::server::captcha_worker::proof_for_claim(store, id, &config).await?,
        None => (String::new(), String::new()),
    };
    // 等待验证码期间可能停用账号或关闭自动领取。
    if (!manual && !config::scheduled_settings().zcode_auto_claim.enabled)
        || store
            .zcode_account_record(id)
            .is_none_or(|r| r.get("enabled").and_then(Value::as_bool) == Some(false))
    {
        return Ok(None);
    }
    claim::claim(
        target.region,
        &target.jwt,
        plan_id,
        &proof.0,
        Some(&proof.1),
        target.device_mid.as_deref(),
        proxy.as_ref(),
    )
    .await
    .map(Some)
    .map_err(|error| format!("套餐领取请求失败（HTTP {}）", error.status_code))
}

/// 「本期为什么没领到」的最后一块拼图：这个账号上一次领取是什么时候、哪一份。
///
/// 只写进账号级状态（`last_result`，界面可见），不进汇总日志 —— 每 10 分钟一轮
/// 就写一遍太吵，而那句话本身在账号页上是要长期看的东西。
fn last_claim_note(store: &AccountStore, id: &str) -> String {
    let Some(record) = store.zcode_account_record(id) else {
        return "账号记录已不存在".to_string();
    };
    let at = record.get("claimAt").and_then(Value::as_i64).unwrap_or(0);
    if at <= 0 {
        return "本地尚无领取记录".to_string();
    }
    let plan = record
        .get("claimPlanId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("未知套餐");
    let hours = (logging::now_ms() - at).max(0) / 3_600_000;
    format!("最近一次领取：{hours} 小时前（{plan}）")
}

async fn scan_account(
    store: &AccountStore,
    id: &str,
    manual: bool,
    retry: &mut serde_json::Map<String, Value>,
) -> Result<Report, String> {
    let target = load_target(store, id).map_err(|(_, message)| message)?;
    let proxy = claim_proxy(store, id);
    let plans = claim::preview(
        target.region,
        &target.jwt,
        target.device_mid.as_deref(),
        proxy.as_ref(),
    )
    .await
    .map_err(|error| error.message)?;
    scan_plans(store, id, &target, manual, retry, plans, |plan_id| {
        let target = &target;
        async move { request_claim(store, id, target, &plan_id, manual).await }
    }).await
}

async fn scan_plans<F, Fut>(
    store: &AccountStore,
    id: &str,
    target: &ClaimTarget,
    manual: bool,
    retry: &mut serde_json::Map<String, Value>,
    mut plans: Vec<claim::ClaimablePlan>,
    mut request: F,
) -> Result<Report, String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<ClaimOutcome>, String>>,
{
    let mut report = Report { checked: 1, ..Report::default() };
    plans.sort_by(|a, b| b.priority.cmp(&a.priority));
    // 投放是增量消息：某轮未再次投放，不代表已登记的失败冷却失效。
    retry.retain(|_, at| at.as_i64().is_some_and(|at| at > logging::now_ms()));
    // 上游这次没给任何套餐：本地无可动作，但要如实记一笔（见 `empty_preview`）。
    if plans.is_empty() {
        report.empty_preview = 1;
        return Ok(report);
    }
    for plan in plans {
        if !manual && !config::scheduled_settings().zcode_auto_claim.enabled {
            break;
        }
        let record = store.zcode_account_record(id).ok_or("账号已被删除")?;
        if record.get("enabled").and_then(Value::as_bool) == Some(false) {
            break;
        }
        if already_recorded(&record, &plan.plan_id) {
            report.already += 1;
            continue;
        }
        if retry
            .get(&plan.plan_id)
            .and_then(Value::as_i64)
            .unwrap_or(0)
            > logging::now_ms()
        {
            report.skipped += 1;
            continue;
        }
        let outcome = match request(plan.plan_id.clone()).await {
            Ok(Some(outcome)) => outcome,
            Ok(None) => break,
            Err(error) => {
                report.failed += 1;
                report.error = Some(error);
                break;
            }
        };
        if matches!(
            &outcome,
            ClaimOutcome::Claimed { .. }
                | ClaimOutcome::Failed {
                    failure: ClaimFailure::AlreadyClaimed,
                    ..
                }
        ) {
            if let Err(error) = complete(store, id, &plan.plan_id) {
                report.failed += 1;
                report.error = Some(error);
                break;
            }
        }
        match outcome {
            ClaimOutcome::Claimed { .. } => {
                report.claimed += 1;
                logging::log(
                    "[Claim]",
                    &format!(
                        "ZCode {}账号「{}」自动领取成功：{}，已切换活动套餐",
                        target.region.label(),
                        target.name,
                        plan.plan_id
                    ),
                );
            }
            ClaimOutcome::Failed {
                failure: ClaimFailure::AlreadyClaimed,
                ..
            } => {
                report.already += 1;
            }
            ClaimOutcome::Failed {
                failure,
                code,
                message,
                failure_ends_at,
                ..
            } => {
                report.failed += 1;
                let detail = failure_detail(failure, code, &message);
                logging::log(
                    "[Claim]",
                    &format!(
                        "ZCode {}账号「{}」自动领取 {}：{}",
                        target.region.label(),
                        target.name,
                        plan.plan_id,
                        detail
                    ),
                );
                match failure {
                    ClaimFailure::QuotaExhausted | ClaimFailure::Ineligible => {
                        let at = failure_ends_at
                            .map(|at| at.saturating_mul(1000))
                            .filter(|at| *at > logging::now_ms())
                            .unwrap_or_else(|| logging::now_ms() + 60 * 60_000);
                        retry.insert(plan.plan_id, json!(at));
                    }
                    ClaimFailure::RiskBlocked => {
                        retry.insert(
                            plan.plan_id,
                            json!(logging::now_ms().saturating_add(RISK_BLOCK_RETRY_MS)),
                        );
                    }
                    ClaimFailure::NotFound | ClaimFailure::Unavailable => {}
                    _ => {
                        report.error = Some(detail);
                        break;
                    }
                }
            }
        }
    }
    Ok(report)
}

pub async fn run_auto(store: &AccountStore, manual: bool) -> Result<String, String> {
    let accounts = store.list_accounts();
    let list = accounts
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or("读取账号列表失败")?;
    let interval = config::scheduled_settings().zcode_auto_claim.interval * 60_000;
    let mut total = Report::default();
    for account in list.iter().filter(|account| eligible(account)) {
        if !manual && !config::scheduled_settings().zcode_auto_claim.enabled {
            break;
        }
        let Some(id) = account.get("id").and_then(Value::as_str) else {
            continue;
        };
        let key = format!("zcodeClaim:{id}");
        task_state::reschedule_on_interval(&key, interval)?;
        let mut retry = task_state::read(&key)?
            .value
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let guard = match task_state::claim(&key, interval, manual, ManualBackoff::Bypass, 1_000)?
        {
            Claim::Acquired(guard) => guard,
            Claim::Deferred(state) => {
                total.skipped += 1;
                logging::log_with_level(
                    "[Claim]",
                    &format!(
                        "ZCode 自动领取账号 {id} 跳过：{}，距下次可检查 {} 秒",
                        if state.running() { "正在执行" } else { "尚未到检查间隔" },
                        (state.due_at() - logging::now_ms()).max(0).saturating_add(999) / 1000,
                    ),
                    "info",
                );
                continue;
            }
        };
        let report = match scan_account(store, id, manual, &mut retry).await {
            Ok(report) => report,
            Err(error) => Report {
                failed: 1,
                error: Some(error),
                ..Report::default()
            },
        };
        total.checked += 1;
        // 账号级状态写的是**卡片口径**（短句 + 上次台账），不是整轮计数：
        // 它会被签到中心的账号行原样带出来，放不下「检查 1 个账号，领取 0 份…」。
        let summary = match (&report.error, report.empty_preview) {
            (Some(error), _) => error.clone(),
            // 探测为空时补一句「上次领到的是哪一份、多久以前」，界面才能回答
            // 「是已经领过了还是今天根本没放新套餐」。
            (None, empty) if empty > 0 => {
                format!("{}；{}", report.account_note(), last_claim_note(store, id))
            }
            _ => report.account_note(),
        };
        guard.finish_on_interval(
            report.error.is_none(),
            summary.clone(),
            Some(json!(retry)),
            interval,
        )?;
        if report.error.is_some() {
            logging::log("[Claim]", &format!("ZCode 自动领取账号 {id}：{summary}"));
        }
        total.claimed += report.claimed;
        total.already += report.already;
        total.skipped += report.skipped;
        total.failed += report.failed;
        total.empty_preview += report.empty_preview;
    }
    let summary = total.summary();
    logging::log_with_level(
        "[Claim]",
        &format!("ZCode 自动检查完成：{summary}"),
        if total.failed > 0 { "error" } else { "info" },
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_fixture(region: Region, label: &str) -> (AccountStore, String, crate::server::db::test_temp::TempDb) {
        use super::super::providers::zcode::credentials::ZcodeCredentials;
        let (db, temp) = crate::server::db::test_temp::TempDb::open(label);
        let store = AccountStore::with_db(Some(db));
        let credentials = ZcodeCredentials {
            region, user_id: "test".into(), jwt: "fixture".into(),
            access_token: "fixture".into(), device_mid: String::new(),
        };
        store.add_zcode_account(&credentials, Some("fixture"), "test").unwrap();
        (store, credentials.account_id(), temp)
    }

    fn offer(id: &str, priority: i64) -> claim::ClaimablePlan {
        claim::ClaimablePlan {
            plan_id: id.into(), name: id.into(), description: String::new(), priority,
        }
    }

    fn rejected(plan_id: String, failure: ClaimFailure, ends_at: Option<i64>) -> ClaimOutcome {
        ClaimOutcome::Failed {
            plan_id, failure, code: 1005, message: "fixture".into(), failure_ends_at: ends_at,
        }
    }

    #[tokio::test]
    async fn marketing_auto_claim_orders_deduplicates_and_persists_both_regions() {
        for region in Region::ALL {
            let (store, id, _temp) = account_fixture(region, "zcode-marketing-auto-success");
            let target = load_target(&store, &id).unwrap();
            assert!(store.mark_zcode_claim(&id, "old", logging::now_ms() - 86_400_000));
            let mut retry = serde_json::Map::new();
            retry.insert("cooling".into(), json!(logging::now_ms() + 60_000));
            let plans = vec![offer("new", 1), offer("old", 100), offer("cooling", 50), offer("remote", 90)];
            let mut called = Vec::new();
            let report = scan_plans(&store, &id, &target, false, &mut retry, plans.clone(), |plan_id| {
                called.push(plan_id.clone());
                std::future::ready(Ok(Some(if plan_id == "remote" {
                    rejected(plan_id, ClaimFailure::AlreadyClaimed, None)
                } else {
                    ClaimOutcome::Claimed { plan_id, starts_at: None, ends_at: None }
                })))
            }).await.unwrap();
            assert_eq!(called, ["remote", "new"]);
            assert_eq!((report.claimed, report.already, report.skipped, report.failed), (1, 2, 1, 0));
            let record = store.zcode_account_record(&id).unwrap();
            assert_eq!(super::super::providers::zcode::plan_of(&record), "start-plan");
            for plan_id in ["old", "remote", "new"] {
                assert!(already_recorded(&record, plan_id));
            }
            let repeat = scan_plans(&store, &id, &target, false, &mut retry, plans, |_| async {
                panic!("recorded or cooling plans must not be submitted again")
            }).await.unwrap();
            assert_eq!((repeat.claimed, repeat.already, repeat.skipped), (0, 3, 1));
        }
    }

    #[tokio::test]
    async fn marketing_auto_claim_keeps_failure_cooldowns_across_empty_deliveries() {
        let (store, id, _temp) = account_fixture(Region::ALL[0], "zcode-marketing-auto-cooldown");
        let target = load_target(&store, &id).unwrap();
        let mut retry = serde_json::Map::new();
        let now = logging::now_ms();
        let window = now / 1000 + 7200;
        for (name, failure, end, minimum) in [
            ("quota", ClaimFailure::QuotaExhausted, Some(window), window * 1000),
            ("ineligible", ClaimFailure::Ineligible, None, now + 60 * 60_000),
            ("risk", ClaimFailure::RiskBlocked, None, now + RISK_BLOCK_RETRY_MS),
        ] {
            let report = scan_plans(&store, &id, &target, false, &mut retry, vec![offer(name, 1)], |plan_id| {
                std::future::ready(Ok(Some(rejected(plan_id, failure, end))))
            }).await.unwrap();
            assert_eq!(report.failed, 1);
            assert_eq!(report.claimed, 0);
            assert!(retry[name].as_i64().unwrap() >= minimum);
            assert!(!already_recorded(&store.zcode_account_record(&id).unwrap(), name));
        }
        let expected = retry.clone();
        retry.insert("expired".into(), json!(now - 1));
        let empty = scan_plans(&store, &id, &target, false, &mut retry, vec![], |_| async {
            panic!("empty marketing must never request a claim")
        }).await.unwrap();
        assert_eq!(empty.empty_preview, 1);
        assert!(empty.error.is_none());
        assert_eq!(retry, expected);
        let held = scan_plans(&store, &id, &target, false, &mut retry, vec![offer("risk", 1)], |_| async {
            panic!("a redelivered plan must retain its failure cooldown")
        }).await.unwrap();
        assert_eq!(held.skipped, 1);
    }

    #[tokio::test]
    async fn marketing_auto_claim_stops_on_errors_cancellation_and_disabled_account() {
        let (store, id, _temp) = account_fixture(Region::ALL[1], "zcode-marketing-auto-stop");
        let target = load_target(&store, &id).unwrap();
        let mut retry = serde_json::Map::new();
        for outcome in [
            Err("captcha or network unavailable".to_string()),
            Ok(None),
            Ok(Some(rejected("first".into(), ClaimFailure::Captcha, None))),
        ] {
            let mut calls = 0;
            let report = scan_plans(&store, &id, &target, false, &mut retry,
                vec![offer("first", 2), offer("second", 1)], |_| {
                    calls += 1;
                    std::future::ready(outcome.clone())
                }).await.unwrap();
            assert_eq!(calls, 1);
            assert_eq!(report.failed, usize::from(!matches!(outcome, Ok(None))));
            assert_eq!(report.claimed, 0);
            assert!(!already_recorded(&store.zcode_account_record(&id).unwrap(), "first"));
        }
        let mut calls = 0;
        scan_plans(&store, &id, &target, false, &mut retry,
            vec![offer("first", 2), offer("second", 1)], |plan_id| {
                calls += 1;
                store.update_account(&id, &json!({"enabled": false})).unwrap();
                std::future::ready(Ok(Some(ClaimOutcome::Claimed { plan_id, starts_at: None, ends_at: None })))
            }).await.unwrap();
        assert_eq!(calls, 1);
        scan_plans(&store, &id, &target, true, &mut retry, vec![offer("first", 1)], |_| async {
            panic!("disabled accounts must not submit even manual claims")
        }).await.unwrap();
    }

    #[test]
    fn marketing_auto_claim_obeys_switch_without_mutating_host_config() {
        const CHILD: &str = "AGENT2API_TEST_MARKETING_AUTO_SWITCH";
        // 配置是进程全局状态，独立子进程只写临时库，避免影响并行测试或本机配置。
        if std::env::var_os(CHILD).is_none() {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "server::core::zcode_claim::tests::marketing_auto_claim_obeys_switch_without_mutating_host_config", "--nocapture"])
                .env(CHILD, "1").status().unwrap().success());
            return;
        }
        let (db, _temp) = crate::server::db::test_temp::TempDb::open("zcode-marketing-auto-switch");
        config::init(Some(db.clone()));
        task_state::install(Some(db.clone()));
        let store = AccountStore::with_db(Some(db));
        let set_enabled = |enabled| assert!(config::set_scheduled_task(
            config::KEY_ZCODE_AUTO_CLAIM,
            config::IntervalTaskPatch { enabled: Some(enabled), interval: None }, 1, 1440,
        ));
        set_enabled(false);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (accounts, id, _account_temp) = account_fixture(Region::ALL[0], "zcode-marketing-auto-switch-account");
            let target = load_target(&accounts, &id).unwrap();
            let mut retry = serde_json::Map::new();
            let stopped = scan_plans(&accounts, &id, &target, false, &mut retry, vec![offer("first", 1)], |_| async {
                panic!("disabled auto claim must not submit")
            }).await.unwrap();
            assert_eq!(stopped.claimed, 0);
            assert!(run_auto(&store, false).await.unwrap().contains("检查 0 个账号"));
            let update = super::super::update::UpdateManager::new(std::env::temp_dir());
            let summary = super::super::scheduled_tasks::run_now(
                &store, &update, super::super::scheduled_tasks::TASK_ZCODE_AUTO_CLAIM,
            ).await.unwrap();
            let state = task_state::read(config::KEY_ZCODE_AUTO_CLAIM).unwrap();
            assert!(summary.contains("检查 0 个账号"));
            assert_eq!(state.last_result.as_deref(), Some(summary.as_str()));
            assert!(state.last_success_at > 0 && !state.running());
            set_enabled(true);
            let mut calls = 0;
            let report = scan_plans(&accounts, &id, &target, false, &mut retry,
                vec![offer("first", 2), offer("second", 1)], |plan_id| {
                    calls += 1;
                    set_enabled(false);
                    std::future::ready(Ok(Some(ClaimOutcome::Claimed { plan_id, starts_at: None, ends_at: None })))
                }).await.unwrap();
            assert_eq!((calls, report.claimed), (1, 1));
        });
    }

    #[test]
    fn explicit_claim_direct_preserves_the_inference_proxy() {
        let session = json!({"proxy":{"source":"custom","protocol":"http",
            "host":"proxy.example","port":7890,"username":"account","password":"fixture"}});
        for direct in [None, Some(""), Some("0"), Some("false")] {
            let proxy = select_claim_proxy(&session, direct).unwrap();
            assert_eq!(proxy.host, "proxy.example");
            assert_eq!(proxy.username, "account");
        }
        for direct in [Some("1"), Some("true"), Some(" TRUE ")] {
            assert!(select_claim_proxy(&session, direct).is_none());
        }
        assert!(crate::server::core::proxies::session_proxy(&session).is_some());
        assert!(select_claim_proxy(&json!({}), None).is_none());
    }

    #[test]
    fn both_regions_require_enabled_claimable_accounts() {
        for provider in ["zcode", "zcode-intl"] {
            let mut account = json!({"provider": provider, "canClaim": true});
            assert!(eligible(&account));
            account["enabled"] = json!(false);
            assert!(!eligible(&account));
            account["enabled"] = json!(true);
            account["canClaim"] = json!(false);
            assert!(!eligible(&account));
        }
        assert!(!eligible(&json!({"provider":"workbuddy","canClaim":true})));
    }

    #[test]
    fn ledger_deduplicates_old_plans_without_blocking_new_ones() {
        let record = json!({"claimPlans":{"old-plan":1,"invalid":0}});
        assert!(already_recorded(&record, "old-plan"));
        assert!(!already_recorded(&record, "new-plan"));
        assert!(!already_recorded(&record, "invalid"));
    }

    #[test]
    fn completion_persists_plan_switch_and_ledger_for_both_regions() {
        use super::super::providers::zcode::credentials::ZcodeCredentials;
        use crate::server::db::test_temp::TempDb;
        let (db, _temp) = TempDb::open("zcode-auto-claim-completion");
        let store = AccountStore::with_db(Some(db));
        for region in Region::ALL {
            let credentials = ZcodeCredentials {
                region,
                user_id: "test".into(),
                jwt: "fixture".into(),
                access_token: "fixture".into(),
                device_mid: String::new(),
            };
            store
                .add_zcode_account(&credentials, Some("fixture"), "test")
                .unwrap();
            let id = credentials.account_id();
            complete(&store, &id, "plan-one").unwrap();
            complete(&store, &id, "plan-two").unwrap();
            let record = store.zcode_account_record(&id).unwrap();
            assert_eq!(
                super::super::providers::zcode::plan_of(&record),
                "start-plan"
            );
            assert!(already_recorded(&record, "plan-one"));
            assert!(already_recorded(&record, "plan-two"));
        }
        assert!(complete(&store, "missing", "plan").is_err());
    }

    #[test]
    fn summary_separates_empty_marketing_from_claims_and_errors() {
        let claimed = Report {
            checked: 1,
            claimed: 1,
            already: 2,
            ..Report::default()
        };
        assert_eq!(
            claimed.summary(),
            "检查 1 个账号，领取 1 份，已领取 2 份，跳过 0 项，失败 0 项"
        );
        assert!(claimed.error.is_none());

        // 探测成功但上游没给套餐：明确说出来，且不算失败
        let empty = Report {
            checked: 1,
            empty_preview: 1,
            ..Report::default()
        };
        let text = empty.summary();
        assert!(text.starts_with("检查 1 个账号，领取 0 份"), "{text}");
        assert!(text.contains("无可领套餐 1 个"), "{text}");
        assert!(text.contains("上游活动投放未下发领取动作"), "{text}");
        assert!(empty.error.is_none());

        let failed = Report {
            checked: 1,
            failed: 1,
            error: Some("套餐活动投放探测失败（HTTP 404）".to_string()),
            ..Report::default()
        };
        let text = failed.summary();
        assert!(text.contains("失败 1 项"), "{text}");
        assert!(!text.contains("无可领套餐"), "{text}");
        assert!(failed.account_note().contains("HTTP 404"));
    }

    /// 账号卡片上的那一行是**另一套口径**：它放进签到中心的账号行，
    /// 放不下整轮计数，所以每条分支都给一句能独立读懂的话。
    #[test]
    fn account_note_speaks_in_one_short_sentence() {
        // 正常领到了
        let claimed = Report {
            checked: 1,
            claimed: 2,
            ..Report::default()
        };
        assert_eq!(claimed.account_note(), "已领取 2 份");

        // 就是本轮要解释的那种情况：preview 空，不是本地失败
        let empty = Report {
            checked: 1,
            empty_preview: 1,
            ..Report::default()
        };
        let note = empty.account_note();
        assert!(note.contains("本期无可领套餐"), "{note}");
        assert!(note.contains("上游活动投放未下发领取动作"), "{note}");
        // 卡片口径不带整轮计数（那是 `summary` 的事）
        assert!(!note.contains("检查 1 个账号"), "{note}");

        // 台账里已有本期记录 / 仍在退避 / 真失败
        let already = Report {
            checked: 1,
            already: 1,
            ..Report::default()
        };
        assert_eq!(already.account_note(), "本期已领过 1 份");
        let skipped = Report {
            checked: 1,
            skipped: 1,
            ..Report::default()
        };
        assert_eq!(skipped.account_note(), "1 份仍在重试冷却中");
        let failed = Report {
            checked: 1,
            failed: 1,
            ..Report::default()
        };
        assert_eq!(failed.account_note(), "领取失败 1 份");

        // 账号级错误原文优先：那才是用户要的答案，不能被计数盖掉
        let broken = Report {
            checked: 1,
            failed: 1,
            error: Some("登录态已失效".into()),
            ..Report::default()
        };
        assert_eq!(broken.account_note(), "登录态已失效");

        // 什么都没有（理论上走不到这里，但不能返回空串把卡片留白）
        assert_eq!(Report::default().account_note(), "本轮无可动作");
    }

    #[test]
    fn last_claim_note_reports_the_ledger_state() {
        use super::super::providers::zcode::credentials::ZcodeCredentials;
        use crate::server::db::test_temp::TempDb;
        let (db, _temp) = TempDb::open("zcode-auto-claim-last-note");
        let store = AccountStore::with_db(Some(db));
        let credentials = ZcodeCredentials {
            region: Region::ALL[0],
            user_id: "test".into(),
            jwt: "fixture".into(),
            access_token: "fixture".into(),
            device_mid: String::new(),
        };
        store
            .add_zcode_account(&credentials, Some("fixture"), "test")
            .unwrap();
        let id = credentials.account_id();
        assert_eq!(last_claim_note(&store, &id), "本地尚无领取记录");
        let at = logging::now_ms() - 3 * 3_600_000;
        assert!(store.mark_zcode_claim(&id, "plan-x", at));
        let note = last_claim_note(&store, &id);
        assert!(note.contains("最近一次领取：3 小时前（plan-x）"), "{note}");
        assert_eq!(last_claim_note(&store, "missing"), "账号记录已不存在");
    }

    #[test]
    fn failure_detail_keeps_code_and_normalizes_line_breaks() {
        let detail = failure_detail(
            ClaimFailure::RiskBlocked,
            3012,
            "request has been blocked\n due to unusual activity",
        );
        assert_eq!(
            detail,
            "上游风控拦截（异常活动）（上游码 3012：request has been blocked  due to unusual activity）"
        );
    }
}
