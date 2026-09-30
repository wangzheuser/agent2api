//! ZCode 手动与定时领取共用的账号上下文及落库逻辑。
use super::account_store::AccountStore;
use super::providers::zcode::{
    claim::{self, ClaimFailure, ClaimOutcome, PreviewOutcome},
    region::Region,
};
use super::task_state::{self, Claim, ManualBackoff};
use crate::server::{config, logging};
use serde_json::{json, Value};

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

/// 取该账号配置的出口代理（理由见模块头：领取**要**挂代理）
pub(crate) fn account_proxy(
    store: &AccountStore,
    account_id: &str,
) -> Option<crate::server::core::proxies::ResolvedProxy> {
    let session = store.get_session_by_id(account_id)?.session;
    crate::server::core::proxies::session_proxy(&session)
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
    error: Option<String>,
}

impl Report {
    fn summary(&self) -> String {
        format!(
            "检查 {} 个账号，领取 {} 份，已领取 {} 份，跳过 {} 项，失败 {} 项",
            self.checked, self.claimed, self.already, self.skipped, self.failed
        )
    }
}

async fn request_claim(
    store: &AccountStore,
    id: &str,
    target: &ClaimTarget,
    plan_id: &str,
    manual: bool,
) -> Result<Option<ClaimOutcome>, String> {
    let proxy = account_proxy(store, id);
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

async fn scan_account(
    store: &AccountStore,
    id: &str,
    manual: bool,
    retry: &mut serde_json::Map<String, Value>,
) -> Result<Report, String> {
    let target = load_target(store, id).map_err(|(_, message)| message)?;
    let proxy = account_proxy(store, id);
    let mut plans = match claim::preview(
        target.region,
        &target.jwt,
        target.device_mid.as_deref(),
        proxy.as_ref(),
    )
    .await
    {
        Ok(PreviewOutcome::Plans(plans)) => plans,
        Ok(PreviewOutcome::NotDeployed) => return Ok(Report::default()),
        Err(error) => return Err(format!("套餐探测失败（HTTP {}）", error.status_code)),
    };
    plans.sort_by(|a, b| b.priority.cmp(&a.priority));
    retry.retain(|id, _| plans.iter().any(|plan| &plan.plan_id == id));
    let mut report = Report::default();
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
        let outcome = match request_claim(store, id, &target, &plan.plan_id, manual).await {
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
                failure_ends_at,
                ..
            } => {
                report.failed += 1;
                logging::log(
                    "[Claim]",
                    &format!(
                        "ZCode {}账号「{}」自动领取 {}：{}",
                        target.region.label(),
                        target.name,
                        plan.plan_id,
                        failure.label()
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
                    ClaimFailure::NotFound | ClaimFailure::Unavailable => {}
                    _ => {
                        report.error = Some(failure.label().into());
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
        task_state::reschedule(&key, interval)?;
        let mut retry = task_state::read(&key)?
            .value
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let guard = match task_state::claim(&key, interval, manual, ManualBackoff::Respect, 1_000)?
        {
            Claim::Acquired(guard) => guard,
            Claim::Deferred(_) => {
                total.skipped += 1;
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
        let summary = report.error.clone().unwrap_or_else(|| report.summary());
        guard.finish(
            report.error.is_none(),
            summary.clone(),
            Some(json!(retry)),
            0,
            interval,
        )?;
        if report.error.is_some() {
            logging::log("[Claim]", &format!("ZCode 自动领取账号 {id}：{summary}"));
        }
        total.claimed += report.claimed;
        total.already += report.already;
        total.skipped += report.skipped;
        total.failed += report.failed;
    }
    let summary = total.summary();
    logging::log("[Claim]", &format!("ZCode 自动检查完成：{summary}"));
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
