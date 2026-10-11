//! 账号级签到：目标集合解析 + 串行执行（对照 Node 版 workbuddy-account-routes.mjs
//! 的 `resolveCheckinTargets` / `checkinFor` / `runCheckin` 三个函数逐条移植）。
//!
//! ── 为什么下沉到 core ───────────────────────────────────────
//! 定时签到（core::auto_checkin，对照 workbuddy-auto-checkin.mjs）与
//! `POST /api/accounts/checkin` 必须是**同一段逻辑**。Node 版靠依赖注入做到这点：
//! `createAutoCheckin({ runCheckin: id => accountRoutes.runCheckin(id) })` ——
//! 调度器拿到的就是账号路由里那个函数，所以「范围过滤、版本分流、串行防风」
//! 的规则只维护一份，不存在两套行为。
//!
//! Rust 侧的 core 不能依赖 api（core 不认识 axum，见 core/mod.rs 的约定），
//! 于是把这段共享逻辑放到这里：api/accounts.rs 与 core/auto_checkin 各自持有
//! store / billing 句柄调用它，规则依旧只有一份。调用方负责把 `CheckinError`
//! 翻成响应（api 层用管理信封，调度器只取 message 记进 lastResult）。//!
//! ── 签到不看 `enabled`（本次改动；此前两轮口径相反）─────────
//! `enabled` 管的是「别让这个账号承接转发」，签到则是用户对某个账号显式发起的
//! 一次动作（定时签到则是调度器对所有账号的统一动作），与转发无关：一个被禁用的
//! 账号依然可以每天签到攒积分。所以单账号与批量两条路径都**不看** `enabled` ——
//! 禁用账号照常进入签到目标集合，界面上照常有签到按钮。
//!
//! ── 历史（别又改回去）────────────────────────────────────────
//! 这里先后有过两种相反口径：先是单账号路径漏查 `enabled`（当时算 bug ——
//! 「显式指定就什么都不看」被过度执行了，于是对禁用账号点签到会真的打上游），
//! 修成「单账号 400 / 批量过滤」；再是现在这次全部放开。中间那版把「禁用转发」
//! 与「禁止签到」当成了一件事 —— 但签到消耗的是**积分额度**，与转发配额不是
//! 同一个池子，用户对禁用账号点「签到」本身就是明确意图，替他拦下来反而多余。
//!
//! 仍然要看的只剩两处，两条路径各自一致：`available`（批量路径过滤，单账号不看 ——
//! Node 版既定语义：账号暂时不可用不影响手动操作）与 `supports_checkin`
//! （没有签到/活跃任务的家，两条路径都排除；WorkBuddy 国际版由活跃任务链放行）。
//!
//! ── `skipped` 的分母 ────────────────────────────────────────
//! 「可用账号总数 − 可签到数」，只可能由**不支持签到/活跃任务的版本**与**范围外
//! 提供商**两类构成（`enabled` 不再参与），与 /api/accounts/usage 的「只算被禁用的」
//! 口径不同 ——
//! 两个动作的「不适用」集合本来就不一样。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::billing::BillingService;
use crate::server::logging;

/// 签到路径上的错误。对应 Node 版抛出的 AccountStoreError：
/// 404「账号不存在」与 400「国际版账号暂不支持签到」。
#[derive(Clone, Debug)]
pub struct CheckinError {
    pub message: String,
    pub status_code: i32,
}

impl CheckinError {
    fn new(message: impl Into<String>, status_code: i32) -> Self {
        Self {
            message: message.into(),
            status_code,
        }
    }
}

impl std::fmt::Display for CheckinError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 计算账号是否进入自动/手动签到目标集合。
///
/// **Qoder 国际版已不再吃这条判据**（2026-10，issue #140）：国际版有独立的
/// 签到链路（带设备风控身份领「每日 100 Credits」，见 `providers::qoder::checkin`
/// 与 `qoder::risk`），在下面的 provider 判定里显式放行 —— 与 WorkBuddy
/// 国际版同一手法。拆家后两家 provider id 各自独立，放行判据按 id 写。

///
/// Accio 系（两个地区）**整家**也没有签到活动：上游客户端全包检索不到
/// 「签到 / checkin / 每日任务」的任何痕迹（见 `providers::accio` 的模块头）。
/// 它按 **provider id** 排除而不是 edition —— 两个地区都没有活动，而 provider
/// 是落盘契约，不会因为凭证里多一个字段而改变判定。
///
/// 这一步是**必需的**：不在范围的家会落到 `checkin_for` 的分派里，拿另一家的
/// 令牌去打错的签到接口只会稳定报错（见那里的最后两条分支）。
pub fn supports_checkin(account: &Value) -> bool {
    let provider = account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID);
    // WorkBuddy 国内版走计费签到，国际版走每日活跃任务；两者都复用本入口。
    if provider == crate::server::core::providers::DEFAULT_PROVIDER_ID {
        return true;
    }
    // WorkBuddy 国际版沿用本地奖励扩展的每日活跃任务；拆家后它拥有独立
    // provider id，但签到入口仍由同一批调度目标覆盖。
    if provider == "workbuddy-intl" {
        return true;
    }
    // Qoder 两个地区共用 campaigns 入口；有没有活动由上游实时列表决定，
    // 不能再按 edition 把国际版提前过滤掉。
    if provider == "qoder" {
        return true;
    }
    // MiniMax Code / LobsterAI are native providers with their own reward
    // protocol.  They are not custom providers and must bypass the generic
    // edition filter below.
    if matches!(provider, "minimax-code" | "lobsterai") {
        return true;
    }
    // AutoClaw 国际版也有独立的 daily_signin 任务链路，不能被通用 intl 判据挡掉。
    if provider == crate::server::core::account_store::AUTOCLAW_INTL_PROVIDER_ID {
        return true;
    }
    // 自定义 provider 只有配置了独立 rewardProfile 与凭证时，才由
    // `supports_reward_checkin` 放行。这里必须先排除，不能让通用 edition
    // 判据把没有奖励适配器的自定义账号误纳入签到并打错上游接口。
    if provider.starts_with(crate::server::core::custom_providers::ID_PREFIX) {
        return false;
    }

    // Qoder 国际版有独立的设备风控签到链路，不能被 edition 过滤。
    if provider == "qoder-intl" {
        return true;
    }
    if account.get("edition").and_then(Value::as_str) == Some("intl") {
        return false;
    }
    // CodeArts 没有「签到」链路，必须先排除：
    // `checkin_for` 的分派 match 把「不在范围里的家」报成「未接入」，而这家
    // 的按钮在界面上由能力位 `checkin: false` 收起 —— 这一层是批量路径
    // （`resolve_checkin_targets` 的 filter）与 API 直调的兜底，双保险。
    // 注意 CodeArts 的每日福利**不是**签到（那是 ops 福利领取，独立的「领福利」
    // 按钮，见 `providers::codearts::welfare`），与这条链无交集。
    if provider == crate::server::core::account_store::codearts_accounts::CODEARTS_PROVIDER_ID {
        return false;
    }
    !crate::server::core::account_store::is_accio_family(provider)
}

/// 账号的提供商 id（缺失时按默认 provider 处理，与账号存储的兜底口径一致）。
///
/// 签到范围的判定按提供商分派：WorkBuddy 走腾讯的每日签到接口，小浣熊走
/// 桌面登录积分链路（`providers::raccoon::balance::claim_daily_grant`）——
/// 两家的接口互不相通，拿小浣熊的 token 去打腾讯的签到接口只会稳定报错。
fn provider_of(account: &Value) -> &str {
    account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or(crate::server::core::providers::DEFAULT_PROVIDER_ID)
}

/// 该账号是否在本次签到的提供商范围内
fn matches_provider_filter(account: &Value, providers: &[String]) -> bool {
    let provider = provider_of(account);
    let provider = if provider == "workbuddy"
        && account.get("edition").and_then(Value::as_str) == Some("intl")
    {
        "workbuddy-intl"
    } else {
        provider
    };
    providers.iter().any(|id| {
        id == provider
            // `reward-custom` is a virtual scheduler option.  The concrete
            // provider ids are generated by custom provider storage and must
            // not be copied into the scheduler config.
            || (*id == "reward-custom"
                && provider.starts_with(crate::server::core::custom_providers::ID_PREFIX))
    })
}

/// Whether a custom account has opted into one of the reward adapters.
///
/// Unlike the ordinary custom provider path, reward accounts carry a second
/// credential.  Keep this check here (rather than teaching `supports_checkin`
/// about storage) so the pure provider capability predicate remains usable by
/// existing callers and tests.
pub(crate) fn supports_reward_checkin(store: &AccountStore, account: &Value) -> bool {
    let provider = provider_of(account);
    provider.starts_with(crate::server::core::custom_providers::ID_PREFIX)
        && account
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| store.custom_reward_credential_by_id(id))
            .is_some()
}

/// 账号快照里的「可用」判定（Node: `account.available !== false`）
fn is_available(account: &Value) -> bool {
    account
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// 账号列表快照（`store.listAccounts().accounts`）
fn accounts_of(store: &AccountStore) -> Vec<Value> {
    store
        .list_accounts()
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 签到目标集合。
///
/// 批量（`id` 为空）：可用账号 ∩ **提供商在 `providers` 范围内** ∩ 支持签到/活跃任务，
/// `skipped` = 可用总数 − 可执行数。范围由配置给出（WorkBuddy / 小浣熊 / AutoClaw
/// 可勾选），定时签到与账号页批量签到共用同一份口径。**禁用账号照常参与** ——
/// 签到与转发是两件事（见模块头「签到不看 enabled」）。
///
/// 指定 id：命中即用（**不过滤 available，也不过滤 provider**），
/// 不支持签到/活跃任务的账号直接报 400 —— 用户点的是谁就执行谁，与「显式指定就执行」的既有语义一致；
/// 批量路径必须过滤 available 与 provider，否则会把范围外的账号也签一遍。
///
/// ── 两条路径的「不满足条件」为什么语义不同（有意如此）──────────
///   批量路径 → **静默跳过**（计入 `skipped`）：定时任务会一次扫过几十个账号，
///     用户没在看着，为一个「不支持签到/活跃任务」把整轮任务报错没有意义。
///   单账号路径 → **明确 400 + 原因**：用户显式点了某个账号的按钮，
///     他需要知道为什么不行。静默成功或静默跳过都会让他以为签到了。
/// 所以 `supports_checkin` 在单账号路径报错、在批量路径过滤掉 ——
/// **不要为了「统一」把其中一处改掉**。
pub fn resolve_checkin_targets(
    store: &AccountStore,
    providers: &[String],
    id: Option<&str>,
) -> Result<(Vec<Value>, usize), CheckinError> {
    let all = accounts_of(store);
    if let Some(id) = id.filter(|value| !value.is_empty()) {
        let found: Vec<Value> = all
            .into_iter()
            .filter(|account| account.get("id").and_then(Value::as_str) == Some(id))
            .collect();
        if found.is_empty() {
            return Err(CheckinError::new("账号不存在", 404));
        }
        // 账号页明细面板与这里共用「不支持签到」语义；WorkBuddy 国际版已经
        // 由活跃任务分支放行，Qoder 等其它不支持的版本仍在这里给出明确 400。
        if !supports_checkin(&found[0]) && !supports_reward_checkin(store, &found[0]) {
            return Err(CheckinError::new("该账号暂不支持签到或每日活跃任务", 400));
        }
        return Ok((found, 0));
    }
    let available: Vec<Value> = all.into_iter().filter(is_available).collect();
    let total = available.len();
    let eligible: Vec<Value> = available
        .into_iter()
        .filter(|account| supports_checkin(account) || supports_reward_checkin(store, account))
        .filter(|account| matches_provider_filter(account, providers))
        .collect();
    let skipped = total - eligible.len();
    Ok((eligible, skipped))
}

/// 单个账号签到。已签到（上游非 0 code）不算错误，原样返回结果 ——
/// 前端把「今天已签到」显示成一条 warn 提示。
///
/// ── 按提供商分派（各家的接口互不相通）────────────────────────
///   - **WorkBuddy 国内版**：计费服务的每日签到（`billing.claim_daily_checkin`）；
///   - **WorkBuddy 国际版**：活跃探测、条件领取与免费模型保活
///     （`billing.workbuddy_daily_activity`）；
///   - **小浣熊**：桌面登录积分链路（`providers::raccoon::balance::claim_daily_grant`）；
///   - **AutoClaw**：通用任务接口的 `daily_signin` 任务
///     （`providers::autoclaw::checkin::claim_daily_signin`）；
///   - **Qoder**：活动（campaign）领取链路，中国版直领；国际版要先带设备
///     风控身份重新拉活动列表（`providers::qoder::checkin` + `qoder::risk`）；

///     （`providers::qoder::checkin::claim_daily_checkin`）；
///   - **Trae**：国内 SOLO 每日积分签到（`providers::trae::checkin::claim_daily`）。
///   - **MiniMax Code**：原生 Provider 的每日签到；
///   - **LobsterAI**：原生 Provider 的活动奖励；
///   - **预置 API 奖励**：自定义 Provider 按 `rewardProfile` 先查状态、再领取；
///     奖励凭证与模型 API Key 分开存储。
///
/// 拿一家的 token 去打另一家的签到接口只会稳定报错，所以这条分派是必需的而不是
/// 优化。各分支的收尾（claim → 结果行 + 日志）完全一致，共用 [`claim_result`]；
/// 各家的 claim 都由各自的实现对齐成 `{success, msg}` 形状。最后的兜底**只认**
/// 默认那家（WorkBuddy），未知家明确报「未接入」——见那里的说明。
pub async fn checkin_for(store: &AccountStore, billing: &BillingService, account: &Value) -> Value {
    let mut row = dispatch_checkin(store, billing, account).await;
    // 业务码由各家定义；汇总必须保留实际分派来源，不能把所有 1001 都当登录失效。
    let provider = provider_of(account);
    row["provider"] = Value::String(
        if provider == "workbuddy" && account.get("edition").and_then(Value::as_str) == Some("intl")
        {
            "workbuddy-intl".to_string()
        } else {
            provider.to_string()
        },
    );
    row
}

async fn dispatch_checkin(
    store: &AccountStore,
    billing: &BillingService,
    account: &Value,
) -> Value {
    let id = account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = account.get("name").cloned().unwrap_or(Value::Null);
    let display = name.as_str().unwrap_or(&id).to_string();
    // 分派的键就是账号的 provider id（`provider_of` 已归一）；AutoClaw 两个
    // 地区各是一个 provider，因此下面按 `region.provider_id()` 反查地区，
    // 而不是写死 `"autoclaw"`（那样国际版账号会掉进 `_` 分支）
    let provider_id = provider_of(account);
    match provider_id {
        // Custom providers opt into reward adapters independently from their
        // model API key.  Check status first so a second scheduler tick never
        // posts a duplicate claim after the upstream has already marked the
        // account complete.
        provider if provider.starts_with(crate::server::core::custom_providers::ID_PREFIX) => {
            let Some(reward) = store.custom_reward_credential_by_id(&id) else {
                return with_reward_kind(
                    json!({
                    "id": id,
                    "name": name,
                    "claim": Value::Null,
                    "error": "未配置奖励凭证",
                    }),
                    "preset_api_reward",
                );
            };
            let result = async {
                let profile =
                    crate::server::core::reward_profiles::parse_profile(&reward.profile_id)
                        .map_err(|error| redact_reward_error(&error.message, &reward.credential))?;
                let status =
                    crate::server::core::reward_profiles::status(profile, &reward.credential, None)
                        .await
                        .map_err(|error| redact_reward_error(&error.message, &reward.credential))?;
                if status.already_completed || !status.claimable {
                    return Ok(reward_status_value(&status));
                }
                let claim =
                    crate::server::core::reward_profiles::claim(profile, &reward.credential, None)
                        .await
                        .map_err(|error| redact_reward_error(&error.message, &reward.credential))?;
                Ok(reward_claim_value(&claim))
            }
            .await;
            with_reward_kind(
                claim_result(id, name, &display, true, result),
                "preset_api_reward",
            )
        }
        "minimax-code" => {
            let claim = crate::server::core::providers::minimax_code::checkin::claim_daily_checkin(
                store, &id,
            )
            .await
            .map_err(|error| error.to_string());
            with_reward_kind(
                claim_result(id, name, &display, true, claim),
                "daily_checkin",
            )
        }
        "lobsterai" => {
            let claim = crate::server::core::providers::lobsterai::checkin::claim_activity_reward(
                store, &id,
            )
            .await
            .map_err(|error| error.to_string());
            with_reward_kind(
                claim_result(id, name, &display, true, claim),
                "activity_reward",
            )
        }
        "raccoon" => {
            let claim =
                crate::server::core::providers::raccoon::balance::claim_daily_grant(store, &id)
                    .await
                    .map_err(|error| error.message);
            with_reward_kind(
                claim_result(id, name, &display, true, claim),
                "daily_checkin",
            )
        }
        "autoclaw" | "autoclaw-intl" => {
            let region =
                crate::server::core::providers::autoclaw::Region::from_provider_id(provider_id)
                    .unwrap_or(crate::server::core::providers::autoclaw::Region::Cn);
            let claim = crate::server::core::providers::autoclaw::checkin::claim_daily_signin(
                region, store, &id,
            )
            .await
            .map_err(|error| error.message);
            with_reward_kind(
                claim_result(id, name, &display, true, claim),
                "daily_checkin",
            )
        }
        "qoder" | "qoder-intl" => {
            // Qoder 的每日权益以活动（campaign）形式下发，两个地区走同一条
            // 实现按地区分支：中国版直接领；国际版要先带设备风控身份重新拉
            // 活动列表（见 `qoder::checkin` 的模块头）。地区由 provider id
            // 反查（拆家后它就是身份），与 AutoClaw 两个地区的分派同款。
            let region = crate::server::core::providers::qoder::endpoints::Region::from_provider_id(
                provider_id,
            )
            .unwrap_or(crate::server::core::providers::qoder::endpoints::Region::Cn);

            let claim =
                crate::server::core::providers::qoder::checkin::claim_daily_checkin(store, region, &id)
                    .await
                    .map_err(|error| error.message);
            with_reward_kind(
                claim_result(id, name, &display, true, claim),
                "activity_reward",
            )
        }
        "trae" => {
            let claim = crate::server::core::providers::trae::checkin::claim_daily(store, &id)
                .await
                .map_err(|error| error.message);
            with_reward_kind(
                claim_result(id, name, &display, true, claim),
                "daily_checkin",
            )
        }

        "loomy" => {
            let claim =
                crate::server::core::providers::loomy::checkin::claim_daily_login(store, &id)
                    .await
                    .map_err(|error| error.message);
            with_reward_kind(
                claim_result(id, name, &display, true, claim),
                "daily_checkin",
            )
        }
        "kuku" => {
            // KukuAI：「免费领积分」活动的每日任务（每日登录 / 完成一次对话），
            // 接口幂等（重复领 reward_point=0），见 `kuku::checkin` 的模块头。
            // 业务会话由实现内部换发 genflowpro STOKEN（`kuku::engine`）保障。
            let claim =
                crate::server::core::providers::kuku::checkin::claim_daily_checkin(store, &id)
                    .await
                    .map_err(|error| error.message);
            claim_result(id, name, &display, true, claim)
        }
        // WorkBuddy 国内版与国际版共用签到入口：按 provider/edition 选择任务形态。
        _ if provider_id == crate::server::core::providers::DEFAULT_PROVIDER_ID
            || provider_id == "workbuddy-intl" =>
        {
            let Some(entry) = store.get_session_by_id(&id) else {
                return with_reward_kind(
                    json!({
                        "id": id,
                        "name": name,
                        "claim": Value::Null,
                        "error": "没有可用凭证",
                    }),
                    "daily_checkin",
                );
            };
            let is_intl = provider_id == "workbuddy-intl"
                || account.get("edition").and_then(Value::as_str) == Some("intl");
            if is_intl {
                let activity = billing
                    .workbuddy_daily_activity(&entry.session, super::WorkbuddyActivity::Full)
                    .await;
                let claim = activity.get("claim").cloned().unwrap_or_else(
                    || json!({ "success": false, "code": -1, "msg": "活跃保活失败" }),
                );
                let claim_success = claim
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let message = claim.get("msg").and_then(Value::as_str).unwrap_or("");
                if claim_success {
                    logging::log("[Accounts]", &format!("账号 {display}: 签到成功"));
                } else {
                    logging::log(
                        "[Accounts]",
                        &format!("账号 {display}: WorkBuddy 国际版 {message}"),
                    );
                }
                return with_reward_kind(
                    json!({
                    "id": id,
                    "name": name,
                    "claim": claim,
                    "activity": activity.get("activity").cloned().unwrap_or(Value::Null),
                    "error": Value::Null,
                    }),
                    "activity_reward",
                );
            }
            let claim = billing
                .claim_daily_checkin(Some(&entry.session))
                .await
                .map_err(|error| error.message);
            with_reward_kind(
                claim_result(id, name, &display, false, claim),
                "daily_checkin",
            )
        }
        other => with_reward_kind(
            json!({
                "id": id,
                "name": name,
                "claim": Value::Null,
                "error": format!(
                    "{} 的签到链路尚未接入",
                    crate::server::core::providers::label_of(other)
                ),
            }),
            "daily_checkin",
        ),
    }
}

fn reward_status_value(status: &crate::server::core::reward_profiles::RewardStatus) -> Value {
    json!({
        "success": false,
        "alreadyCompleted": status.already_completed,
        "claimable": status.claimable,
        "reward": status.reward.clone(),
        "wallet": status.wallet.clone(),
        "expiresAt": status.expires_at.clone(),
        "msg": status.message.clone().unwrap_or_else(|| {
            if status.already_completed { "今日已领取".to_string() } else { "当前没有可领取的奖励".to_string() }
        }),
        "status": if status.already_completed { "already_claimed" } else { "available" },
    })
}

fn reward_claim_value(claim: &crate::server::core::reward_profiles::RewardClaim) -> Value {
    json!({
        "success": claim.success,
        "alreadyCompleted": claim.already_completed,
        "claimable": claim.claimable,
        "reward": claim.reward.clone(),
        "wallet": claim.wallet.clone(),
        "expiresAt": claim.expires_at.clone(),
        "msg": claim.message.clone().unwrap_or_else(|| {
            if claim.success { "领取成功".to_string() } else if claim.already_completed { "今日已领取".to_string() } else { "当前没有可领取的奖励".to_string() }
        }),
        "status": if claim.success { "claimed" } else if claim.already_completed { "already_claimed" } else { "not_claimed" },
    })
}

fn redact_reward_error(message: &str, credential: &str) -> String {
    let mut result = message.replace(credential, "[REDACTED]");
    for key in [
        "apiKey",
        "api_key",
        "accessToken",
        "access_token",
        "cookie",
        "Cookie",
    ] {
        result = result.replace(key, "credential");
    }
    result
}

/// 给统一结果行标注奖励来源。保留既有 `{id,name,claim,error}` 外形，
/// 在结果行顶层增加稳定的 `rewardKind`，前端可据此区分每日签到、活动奖励和
/// 预置 API 奖励，不必通过文案猜测；claim 内的同名字段保留，兼容本轮早期客户端。
fn with_reward_kind(mut row: Value, reward_kind: &str) -> Value {
    if let Some(object) = row.as_object_mut() {
        object.insert(
            "rewardKind".to_string(),
            Value::String(reward_kind.to_string()),
        );
    }
    if let Some(claim) = row.get_mut("claim").and_then(Value::as_object_mut) {
        claim.insert(
            "rewardKind".to_string(),
            Value::String(reward_kind.to_string()),
        );
    }
    row
}

/// 把一次签到调用翻成统一的结果行（`{id, name, claim, error}`）。
///
/// ── `log_success_msg` 为什么是一个参数而不是统一口径 ─────────
/// 小浣熊与 AutoClaw 的 claim `msg` 带**具体收益**（「今日积分 +100」
/// 「签到成功，获得 100 积分」），拼进日志才有排查价值；WorkBuddy 保持原样
/// （照抄 Node 版，不在这里做「顺手统一」—— 那会改变它既有的日志文案，
/// 而日志是用户已经在看的输出）。失败分支三家一致。
fn claim_result(
    id: String,
    name: Value,
    display: &str,
    log_success_msg: bool,
    result: Result<Value, String>,
) -> Value {
    match result {
        Ok(claim) => {
            let success = claim
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let msg = claim.get("msg").and_then(Value::as_str).unwrap_or("");
            if success {
                if log_success_msg && !msg.is_empty() {
                    logging::log("[Accounts]", &format!("账号 {display}: 签到成功（{msg}）"));
                } else {
                    logging::log("[Accounts]", &format!("账号 {display}: 签到成功"));
                }
            } else {
                logging::log(
                    "[Accounts]",
                    &format!("账号 {display}: 签到未领取（{msg}）"),
                );
            }
            json!({ "id": id, "name": name, "claim": claim, "error": Value::Null })
        }
        Err(message) => {
            logging::log(
                "[Accounts]",
                &format!("账号 {display}: 签到失败（{message}）"),
            );
            json!({
                "id": id,
                "name": name,
                "claim": Value::Null,
                "error": message,
            })
        }
    }
}

/// 这次签到是否意味着「今天已经签过了」—— 决定要不要落 `checkinAt`。
///
/// 三种情况都算，因为它们在「今天不能再领」这件事上没有区别：
///   1. `success === true`：本次真的领到了；
///   2. `alreadyCompleted === true`：上游明确告知今天已完成
///      （AutoClaw 的 `daily_signin` 会带这个字段，见 `providers::autoclaw::checkin`）；
///   3. `msg` 里含「已签到 / 已领取」：WorkBuddy 只把「今日已签到」放在文案里，
///      没有专门的码位可用，所以这里只能看文案。
///
/// 小浣熊不需要第 3 条：它的「今天已领过」是通过**账单核对**发现的 ——
/// 今天有入账记录就会算出 `granted_today > 0`，于是自然落到第 1 条。
///
/// ── 为什么不能宽到「只要没报错就算」─────────────────────────
/// 失败行（网络错误 / 凭证失效 / 5xx）与「今天已签到」是两回事：前者意味着今天
/// 可能一次都没签上，把它当作已签到去置灰按钮会白丢一天。这不是假想 ——
/// 实测见过自动签到 4 个账号全部未领取、17 秒后手动逐个重试全部成功
/// （批次撞上上游风控），所以判据必须收紧到「上游说签过了」。
fn checkin_completed_today(claim: &Value) -> bool {
    if claim.get("success").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if matches!(
        claim.get("status").and_then(Value::as_str),
        Some("claimed" | "already_claimed" | "balance_refreshed")
    ) {
        return true;
    }
    if claim.get("alreadyCompleted").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let message = claim.get("msg").and_then(Value::as_str).unwrap_or("");
    message.contains("已签到") || message.contains("已领取")
}

/// 这条领取结果是「中性未领取」—— 不是成功、不是已签、也不是失败，而是
/// 「此刻没有可领的活动」。Qoder 国际版的活动窗口每天 10:00（UTC+8）才开，
/// 定时签到在这个窗口之前跑完时结果必然长这样；自动签到调度据此判断
/// 「今天要不要保持未落账、过段时间重试」（见 `auto_checkin::tick`）。
///
/// 判据刻意与 [`checkin_completed_today`] 相反且**只认中性文案**：把失败
/// （网络错误 / 凭证失效）也当成 pending 会让坏账号整天反复重试打上游。
pub fn claim_pending(claim: &Value) -> bool {
    if checkin_completed_today(claim) {
        return false;
    }
    // error 字段非空 = 执行出错（网络 / 凭证），那不是「等窗口」能解决的
    if claim.get("error").and_then(Value::as_str).is_some_and(|text| !text.trim().is_empty()) {
        return false;
    }
    let message = claim.get("msg").and_then(Value::as_str).unwrap_or("");
    // 两个地区的「无活动」文案都含这一句；实现改措辞时这里要跟着对齐
    //（`providers::qoder::checkin::no_claim` 与 trae 的「未领取 + 原因」不同形，
    // trae 不走这条 pending 逻辑 —— 它没有窗口概念）。
    message.contains("没有可领取的签到活动") || message.contains("未下发每日积分活动")
}

/// 执行一次签到并汇总（Node 版 `runCheckin(id)`）。
///
/// `id` 为 None 时签全部符合条件的账号（定时签到走这条），范围由 `providers`
/// 决定（配置里勾选的提供商，缺省全选；**指定 id 单签时不受范围限制**）。
/// `reason` 只在批量轮次（id=None）进签到历史台账（`checkin_history`，签到中心
/// 时间线的来源）；单账号签到不进台账，只更新账号的 `checkinAt`。
/// **串行**：避免多账号同时打上游触发 11-128 风控。
pub async fn run_checkin(
    store: &AccountStore,
    billing: &BillingService,
    providers: &[String],
    id: Option<&str>,
    reason: &str,
) -> Result<Value, CheckinError> {
    let (targets, skipped) = resolve_checkin_targets(store, providers, id)?;
    let mut results = Vec::with_capacity(targets.len());
    for account in &targets {
        let row = checkin_for(store, billing, account).await;
        // 落签到时间：手动单签与定时签到走的是**这一段**（两条链都调本函数），
        // 所以账号页的「已签到」在两种路径下都会亮起来，不需要各自记一次。
        // 写盘失败只记日志、不改签到结果 —— 上游那边积分已经领到了，
        // 因为一次落盘失败就把成功的签到报成失败是本末倒置。
        if let Some(account_id) = row.get("id").and_then(Value::as_str) {
            let completed = row
                .get("claim")
                .map(checkin_completed_today)
                .unwrap_or(false);
            if completed && !store.mark_checkin(account_id, logging::now_ms()) {
                logging::verbose(
                    "[Accounts]",
                    &format!("账号 {account_id} 的签到时间未能落盘（账号可能已被删除）"),
                );
            }
        }
        results.push(row);
    }
    // 「真实领取」与「今日已领取」都表示本日签到已完成；只有活跃保活不计入
    // succeeded，避免把普通签到时间与活跃任务混为一谈 —— 保活读数单独统计在
    // `active`（`activity.pokeSucceeded`），它的成功不落 `checkinAt`。
    let succeeded = results
        .iter()
        .filter(|item| {
            item.get("claim")
                .map(checkin_completed_today)
                .unwrap_or(false)
        })
        .count();
    let active = results
        .iter()
        .filter(|item| {
            item.get("activity")
                .and_then(|activity| activity.get("pokeSucceeded"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    let eligible = results.len();
    let available = eligible + skipped;
    if skipped > 0 {
        logging::log(
            "[Accounts]",
            &format!(
                "签到目标解析：可用 {available} 个，可执行 {eligible} 个，跳过 {skipped} 个（不支持签到/活跃任务或不在签到范围内）"
            ),
        );
    }
    logging::log(
        "[Accounts]",
        &format!(
            "签到完成：成功 {succeeded}/{available} 个（实际执行 {eligible}，跳过 {skipped}），{active} 个账号完成活跃保活有效对话（日活奖励尚未确认）"
        ),
    );
    // 批量轮次进台账（签到中心时间线）。写盘失败只记日志：上游那边积分已经
    // 领到，台账缺一条不该让这次签到的响应报错。
    if id.is_none() {
        crate::server::core::checkin_history::record(
            &json!({
                "succeeded": succeeded,
                "active": active,
                "total": results.len(),
                "skipped": skipped,
                "results": results,
            }),
            reason,
        );
    }
    Ok(json!({
        "results": results,
        "succeeded": succeeded,
        "active": active,
        // `total` 保持原有语义：实际执行的账号数；新增 `available` 让面板和
        // 日志可以准确解释「成功/总数」与「跳过」的关系。
        "total": eligible,
        "eligible": eligible,
        "available": available,
        "skipped": skipped,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workbuddy_international_is_included_in_checkin_targets() {
        assert!(supports_checkin(&json!({
            "provider": "workbuddy",
            "edition": "intl",
        })));
        assert!(supports_checkin(&json!({
            "provider": "workbuddy",
            "edition": "cn",
        })));
    }

    #[test]
    fn qoder_both_regions_are_checked_by_campaigns_but_accio_stays_excluded() {
        assert!(supports_checkin(&json!({
            "provider": "qoder",
            "edition": "intl",
        })));
        assert!(supports_checkin(&json!({
            "provider": "qoder",
            "edition": "cn",
        })));
        assert!(!supports_checkin(&json!({
            "provider": "accio",
            "edition": "intl",
        })));
        assert!(supports_checkin(&json!({
            "provider": "autoclaw-intl",
            "edition": "intl",
        })));
        assert!(supports_checkin(&json!({ "provider": "minimax-code" })));
        assert!(supports_checkin(&json!({ "provider": "lobsterai" })));
    }

    #[test]
    fn reward_custom_filter_matches_generated_provider_ids_only_via_virtual_option() {
        assert!(matches_provider_filter(
            &json!({ "provider": "custom-ab12" }),
            &["reward-custom".to_string()]
        ));
        assert!(!matches_provider_filter(
            &json!({ "provider": "custom-ab12" }),
            &["workbuddy".to_string()]
        ));
        assert!(!matches_provider_filter(
            &json!({ "provider": "workbuddy" }),
            &["reward-custom".to_string()]
        ));
    }

    #[test]
    fn custom_provider_is_not_regular_checkin_without_reward_opt_in() {
        assert!(!supports_checkin(&json!({
            "provider": "custom-ab12",
            "edition": "cn",
        })));
        assert!(!supports_checkin(&json!({
            "provider": "custom-ab12",
            "edition": "intl",
        })));
    }

    #[test]
    fn activity_keepalive_does_not_mark_regular_checkin() {
        assert!(!checkin_completed_today(&json!({
            "success": false,
            "msg": "活跃保活完成",
        })));
        assert!(checkin_completed_today(&json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已领取",
        })));
        assert!(checkin_completed_today(&json!({ "success": true })));
    }

    #[test]
    fn reward_kind_is_attached_to_claim_or_error_rows() {
        let claimed = with_reward_kind(
            json!({ "id": "a", "claim": { "success": true } }),
            "activity_reward",
        );
        assert_eq!(claimed["claim"]["rewardKind"], "activity_reward");
        let failed = with_reward_kind(
            json!({ "id": "a", "claim": Value::Null, "error": "offline" }),
            "daily_checkin",
        );
        assert_eq!(failed["rewardKind"], "daily_checkin");
    }

    // 签到「今天已签」判据的边界。
    //
    // ── 为什么这里要有一条**闭环**测试 ──────────────────────────
    // 上游（`billing::call_billing`）与下游（本模块的 `checkin_completed_today`）
    // 各判一次「今天已领过」，两处都靠**文案**：前者决定要不要把 HTTP 400 放行成
    // `Ok`，后者决定要不要落 `checkinAt`。两处必须同源 —— 只放宽一处，就会出现
    // 「放行了却仍判不出已完成」（面板不亮）或「判得出但先被丢成 Err」（`claim`
    // 为 null）这两种半截修复。`closed_loop_tolerated_400_reaches_completed` 把
    // 这条链整条钉住，是本次修复（上游 issue #137）唯一的端到端回归保护。
    use crate::server::core::billing::request::BillingCall;
    use crate::server::core::billing::{is_duplicate_claim, normalize_daily_claim, BillingService};
    use serde_json::Value;

    /// 上游真实的 400 文案（WorkBuddy 国内版当日已签到时的原话）。
    const REAL_DUPLICATE: &str = "今天已签到，请明天再来";

    // ── 下游判据本身 ────────────────────────────────────────────

    #[test]
    fn success_and_already_completed_are_done() {
        // 第 1 条：本次真的领到了
        assert!(checkin_completed_today(&json!({"success": true, "code": 0})));
        // 第 2 条：上游明确告知（AutoClaw 的 daily_signin 带这个字段）
        assert!(checkin_completed_today(&json!({"success": false, "alreadyCompleted": true})));
    }

    #[test]
    fn message_wording_marks_today_done() {
        // 第 3 条：WorkBuddy 只把「今日已签到」放在文案里，没有码位可用
        assert!(checkin_completed_today(&json!({"success": false, "msg": REAL_DUPLICATE})));
        assert!(checkin_completed_today(&json!({"success": false, "msg": "该奖励已领取"})));
    }

    #[test]
    fn real_failures_are_never_mistaken_for_done() {
        // 核心不变量（见本文件上方 `checkin_completed_today` 的注释）：失败行与
        // 「今天已签到」是两回事，把前者当作已签到会白丢一天积分 —— 实测见过
        // 批次撞风控 4 个账号全部未领取。这几条必须全部为 false。
        for claim in [
            json!({"success": false, "code": -1, "msg": "计费接口请求失败: 连接上游超时"}),
            json!({"success": false, "code": -1, "msg": "登录态已过期或被拒绝，无法调用计费接口"}),
            json!({"success": false, "code": -1, "msg": ""}),
            // 键缺失：`claim` 为 null 时上游路径根本没走到判据，这里模拟 msg 缺失
            json!({"success": false, "code": 40001}),
        ] {
            assert!(!checkin_completed_today(&claim), "不该被判成已完成: {claim}");
        }
    }

    // ── 闭环：上游放行 → 下游判得出 ─────────────────────────────

    #[test]
    fn closed_loop_tolerated_400_reaches_completed() {
        // 模拟 `call_billing` 在 `tolerate_duplicate_claim: true` 时对 400 的放行：
        // 返回 `Ok(BillingCall { code, msg, data: Null, .. })`，msg 是上游文案。
        // `code` 取 null —— 上游 400 响应体里没有顶层 `code` 时就是这个形态。
        let tolerated = BillingCall {
            code: None,
            msg: Some(REAL_DUPLICATE.to_string()),
            request_id: None,
            data: Value::Null,
            raw: Some(json!({ "msg": REAL_DUPLICATE })),
        };

        // 1) 上游确实会放行这一条（判据命中）
        assert!(is_duplicate_claim(400, REAL_DUPLICATE));

        // 2) 放行后的归一化产出：失败形状，但 msg 保留上游文案
        let claim = normalize_daily_claim(tolerated);
        assert_eq!(claim.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(claim.get("msg").and_then(Value::as_str), Some(REAL_DUPLICATE));

        // 3) 下游据此判出「今天已完成」→ `mark_checkin` 会被调用、面板标识点亮。
        //    这一跳就是 #137 里断掉的那一环：原代码在 1) 之前就 Err 了，claim 为
        //    null，于是这里恒为 false，`checkinAt` 永远写不进去。
        assert!(
            checkin_completed_today(&claim),
            "放行了却判不出已完成 —— 上游与下游的文案判据已经脱钩"
        );
    }

    #[test]
    fn closed_loop_non_duplicate_400_stays_failed() {
        // 反向：同为 400，文案与「重复领取」无关时上游**不**放行（仍 Err），
        // 因此不会有 claim 走到下游。这里直接断言判据不命中，钉住「不能过度放宽」。
        for detail in ["参数错误", "请求内容不是有效 JSON", "活动已结束"] {
            assert!(!is_duplicate_claim(400, detail), "detail={detail:?} 不该被放行");
        }
    }

    // ── HTTP 接线：真正驱动 `call_billing` 的修复分支 ────────────
    //
    // ⚠️ 上面那些测试**都不足以保护真正的修复点**。`closed_loop_*` 是手工构造
    // `BillingCall` 再调 `normalize_daily_claim`，绕过了 `call_billing` —— 把
    // `mod.rs` 里 `if options.tolerate_duplicate_claim && is_duplicate_claim(...)`
    // 整段删掉或改成 `if false`，它们**依然全绿**（`is_duplicate_claim` 自己的
    // 测试也照样过，因为那函数还在）。下面三条起真的 HTTP 服务，让 400 从 socket
    // 上走一遍，把「修复分支本身」钉住 —— 这是 #137 的回归保护里唯一不可绕过的
    // 一层。

    /// 起一个只回固定响应的 mock 上游（照抄 `upstream::provider_loop::tests`
    /// 的手写 axum 约定，项目既有风格，不引入 wiremock）。
    /// 返回 (base_url, 命中计数, 任务句柄)；句柄由调用方持有到用例结束。
    async fn mock_billing(
        status: u16,
        body: &'static str,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let app = axum::Router::new().route(
            "/v2/billing/meter/daily-checkin",
            axum::routing::post(move || {
                count.fetch_add(1, Ordering::SeqCst);
                async move {
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, hits, task)
    }

    /// 指向 mock 的国内版 session —— `call_billing` 从这里取 endpoint 与请求头。
    /// `edition: "cn"` 是关键：为 `intl` 会走国际版活跃任务链，根本不打这个端点。
    fn cn_session(base: &str) -> Value {
        json!({
            "endpoint": base,
            "edition": "cn",
            "auth": { "accessToken": "test-token" },
            "account": { "uid": "u1" },
        })
    }

    /// 建一个可用的 `BillingService`（空账号库即可 —— session 由调用方直接给）。
    fn billing_service(label: &str) -> (BillingService, crate::server::db::test_temp::TempDb) {
        use crate::server::core::account_store::AccountStore;
        let (db, guard) = crate::server::db::test_temp::TempDb::open(label);
        let store = AccountStore::with_db(Some(db));
        (
            BillingService::new(crate::server::core::auth::AuthService::for_store(store)),
            guard,
        )
    }

    #[tokio::test]
    async fn tolerated_400_from_real_http_reaches_completed() {
        let (billing, _guard) = billing_service("billing-checkin-400");
        let (base, hits, _task) = mock_billing(400, r#"{"msg":"今天已签到，请明天再来"}"#).await;

        let claim = billing
            .claim_daily_checkin(Some(&cn_session(&base)))
            .await
            .expect("400 +「今天已签到」应被容错放行，而不是 Err");

        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "只该打一发上游（重复领取不做重试）"
        );
        assert_eq!(claim.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(claim.get("msg").and_then(Value::as_str), Some(REAL_DUPLICATE));
        // 端到端闭环：这一跳就是 #137 断掉的地方（原代码在上一行就 Err 了，
        // claim 为 null，于是这里恒为 false，checkinAt 永远写不进去）。
        assert!(checkin_completed_today(&claim), "#137：放行后仍判不出已完成");
    }

    #[tokio::test]
    async fn non_duplicate_400_from_real_http_still_errors() {
        let (billing, _guard) = billing_service("billing-checkin-400-other");
        let (base, _hits, _task) = mock_billing(400, r#"{"msg":"参数错误"}"#).await;

        let error = billing
            .claim_daily_checkin(Some(&cn_session(&base)))
            .await
            .expect_err("文案与重复领取无关的 400 必须照旧报错");
        assert!(error.message.contains("400"), "message={}", error.message);
    }

    #[tokio::test]
    async fn server_error_from_real_http_still_errors() {
        let (billing, _guard) = billing_service("billing-checkin-500");
        // 500 是**真故障**：即便文案恰好含「已签到」也不能放行，否则一次没签上的
        // 日子会被记成已签（`checkin.rs` 注释里实测过的批次撞风控场景）。
        let (base, _hits, _task) = mock_billing(500, r#"{"msg":"今天已签到，请明天再来"}"#).await;

        billing
            .claim_daily_checkin(Some(&cn_session(&base)))
            .await
            .expect_err("500 不该被容错放行");

    }
}
