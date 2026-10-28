//! Qoder 每日签到（campaign 领取链路；中国版 / 国际版两条分支）。

//!
//! ── 上游长什么样（2026-09-27 在真实账号上实测）────────────────
//! 每日权益以**活动（campaign）**形式下发，走 `sash` 一族接口：
//!
//! ```text
//! GET  {openapi}/sash/api/v1/me/campaigns?forceRefresh=true
//!   → {uid, showCampaign, claimable, campaigns:[{campaignId, campaignKey,
//!        actionType, claimStatus, startAt, endAt,
//!        benefit:{kind:"CREDITS", amount:100,
//!                 validity:{mode:"RELATIVE_DAYS", days:30}}}]}
//! POST {openapi}/sash/api/v1/me/campaigns/{campaignId}/claim   body {}
//!   → {status:"CLAIMED", replayed, benefit:{...},
//!      claimedAt, grantedAt, expiresAt}      // 三个时间都是 ISO 字符串
//! ```
//!
//! 实测（中国版 Pro Trial 账号 `act-20260923-339`）：
//!   - 活动窗口 `startAt=1790474400 → endAt=1790560740`，即**当天 10:00 → 次日 09:59**
//!     （UTC+8），与官方客户端「每天 10:00 刷新」一致；
//!   - 领取返回 `{"status":"CLAIMED","replayed":false,"benefit":{...,"amount":100}}`，
//!     紧接着查 `/api/v2/quota/usage`，`addOnQuota` 从「上游不返回该键」变成
//!     `{total:100, remaining:100, unit:"credits"}` —— **签到额度落在 `addOnQuota`
//!     这个桶里**（余额面板「资源包」那一行，见 `balance.rs`）；
//!   - **领取后活动行不消失**，`claimStatus` 由 `CLAIMABLE` 变 `CLAIMED`。
//!     所以「今天已领取」直接从活动列表读，不需要本地记账；
//!   - 顶层的 `claimable` 标志**不能当判据**：领完之后它仍是 `true`（同账号还有一条
//!     可领的促销活动），必须按 `actionType` 逐行筛。
//!
//! ── 国际版为什么要两段拉取（issue #140，CreditDaddy 实测结论）──
//! 国际版服务端**只对带设备风控头**（`Cosy-MachineToken` / `Cosy-MachineCode` /
//! `Cosy-MachineType`，由本机 Qoder 客户端 / qodercli 的 UMID 组件生成，见
//! `risk.rs`）的请求下发「每天领 100 Credits」活动：不带风控头时活动列表里
//! 永远只有 `VIEW_DETAILS` 促销 —— 本家旧注释「国际版没有签到计划」的真正
//! 原因就是缺头，不是没活动。风控身份按 **uid** 生成，而 uid 在活动列表的
//! **顶层字段**里，所以顺序必须是：先拉一次拿 uid → 叠风控头重拉 → 再领。
//! legacy 的 `daily-check-in` 路径两个地区都已停用（国际版 404、中国版恒
//! `DISABLED`），一次都不碰。
//!
//! ── 为什么不走 legacy 的 daily-check-in ─────────────────────
//! `GET /sash/api/v1/me/daily-check-in/status` 的 `status` 恒为 `DISABLED`
//! （国际版该路径直接 404），streak 与累计全 0；它的 claim 兄弟端点已被上游
//! 全局停用（对未领取的日子也回 409 且不发积分）。因此这里**只读活动列表**，
//! 一次都不碰 legacy。
//!
//! ── 盲发直领兜底（2026-10-19 引入，依据社区实测证据）───────────
//! 活动列表端点按**请求形态**分桶投放：同一账号、同一时刻，带完整设备风控头
//! 的请求形态可能看不到 `CLAIMABLE` 行，最小头（只带鉴权四件套）形态却能看
//! 到（smart-open/TraeWorkAssistant `qoder_checkin.rs` 2026-10-14 探针实证；
//! 且 **claim 端点不受列表过滤约束** —— 幂等，重复领取回 `replayed=true`）。
//! 因此「列表里没有可领行」不能判定「领不到」：本实现把成功领取过的活动 id
//! 记入本地学习表（`campaign_ids`：campaign id 全局有效、与账号无关，随活动
//! 批次轮换 —— 过期 id 直领返回 409 `CAMPAIGN_NOT_ACTIVE`，据此从表里剔除），
//! 列表无可领行时对「列表里非 CLAIMED 的 CLAIM_BENEFIT 行 + 学习表候选」
//! **盲发直领**。每个账号都用**自己的**设备身份发起（`machine::device_id` 按
//! 账号分配、永不轮换；本机 UMID 组件产出的风控三件套为真值形状）——上游若
//! 按 `Cosy-MachineId` 判设备名额，各账号互不占用；被 409 明确拒绝时如实
//! 记录并保持 `deviceLimited` 语义。缺 UMID 组件时盲发退化为最小头形态，
//! 这正是社区实证可领的请求形状（bzym2/QoderGateway 实证服务端对
//! machine_token 无强绑定校验；caigee-cmd/cli2api 最小头直连签到互证）。
//!
//! ── 没有签到活动的账号 ─────────────────────────────────────
//! 中国版 Free 套餐实测没有活动下发（Pro/试用才有）：返回一条中性结果
//! （`success:false` + 说明），不是错误 —— 上游给不给活动是账号侧的状态。
//! 国际版缺 UMID 组件时同样给中性说明（缺什么、怎么补），把「没得领」报成

//! 失败会让用户以为接口坏了。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件零 unwrap/expect/panic，取值全走 Option 链。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::ApiResponse;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::Credentials;
use super::endpoints::Region;
use super::{auth, device_claims, endpoints, machine, refresh, risk};

/// 活动类型：可领取权益（促销类活动是 `VIEW_DETAILS`，不是签到，不能领）
const ACTION_CLAIM_BENEFIT: &str = "CLAIM_BENEFIT";
/// 活动状态：可领取
const STATUS_CLAIMABLE: &str = "CLAIMABLE";
/// 活动状态：已领取
const STATUS_CLAIMED: &str = "CLAIMED";
/// 领取响应的 `result` 字段（上游在并发/重放时会给这个值）
const RESULT_ALREADY_CLAIMED: &str = "ALREADY_CLAIMED";
/// 上游没给 `benefit.amount` 时的缺省奖励额（实测就是 100）
const DEFAULT_REWARD: f64 = 100.0;
/// 上游 409 的「活动批次已过期」业务码（活动批次轮换，id 需重学）
const CAMPAIGN_NOT_ACTIVE_TAG: &str = "CAMPAIGN_NOT_ACTIVE";
/// 上游领取响应的「同一人今天已经领过」业务码（见 [`FAILURE_SAME_PERSON`] 的说明）
const FAILURE_SAME_PERSON_TAG: &str = "SAME_PERSON_ALREADY_CLAIMED";
/// 盲发直领的候选上限（防上游异常返回一大串活动时打爆请求）
const BLIND_CLAIM_MAX: usize = 5;
/// 本地学习表每地区保留的候选条数
const CAMPAIGN_ID_KEEP: usize = 5;
/// 学习表条目过期时长（活动批次一批只有数日窗口，过期 id 直领会 409）
const CAMPAIGN_ID_TTL_MS: i64 = 14 * 24 * 60 * 60 * 1000;

// ─── 盲发候选的本地学习表 ───────────────────────────────────
//
// 依据（见模块头）：campaign id 全局有效、与账号无关，claim 幂等；列表可能被
// 上游按请求形态分桶隐藏。把每次成功领取的 id 记下来，其它账号（或下一次
// 运行）即使列表里看不到活动，也能凭学到的 id 直接领取。
//
// 落盘形态 `{config_dir}/qoder-campaign-ids.json`：
// ```json
// {"version":1,"regions":{"cn":[{"id":"…","at":1790000000000}],"global":[…]}}
// ```
// 只保留最近 `CAMPAIGN_ID_KEEP` 条、丢弃 `CAMPAIGN_ID_TTL_MS` 之前的条目
// （活动批次一批只有数日窗口，过期 id 直领会 409，留着的意义是让那次 409
// 换来「上游还在发同一批」的确认 —— 14 天足够覆盖）。
mod campaign_ids {
    use std::collections::BTreeMap;
    use std::path::Path;

    use serde::{Deserialize, Serialize};

    use super::endpoints::Region;
    use super::{CAMPAIGN_ID_KEEP, CAMPAIGN_ID_TTL_MS};

    const CURRENT_VERSION: u32 = 1;

    #[derive(Serialize, Deserialize)]
    struct Entry {
        id: String,
        /// 学习时间（毫秒）
        at: i64,
    }

    #[derive(Serialize, Deserialize, Default)]
    struct File {
        #[serde(default)]
        version: u32,
        #[serde(default)]
        regions: BTreeMap<String, Vec<Entry>>,
    }

    fn key(region: Region) -> String {
        region.id().to_string()
    }

    fn load(path: &Path) -> File {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<File>(&text).ok())
            .unwrap_or_default()
    }

    fn save(path: &Path, file: &mut File) {
        file.version = CURRENT_VERSION;
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        // 临时名必须唯一：并发签到会同时写（与 machine.rs 的台账同一手法）
        static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("qoder-campaign-ids.json");
        let temp = parent.join(format!("{base}.{}.{seq}.tmp", std::process::id()));
        let Ok(text) = serde_json::to_vec_pretty(file) else { return };
        if std::fs::write(&temp, text).is_err() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        }
        #[cfg(windows)]
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::rename(temp, path);
    }

    fn prune(entries: &mut Vec<Entry>, now: i64) {
        entries.retain(|entry| !entry.id.trim().is_empty() && now.saturating_sub(entry.at) < CAMPAIGN_ID_TTL_MS);
        // 新学习的放最前（remember 插头部），保序去重后截断
        let mut seen: Vec<String> = Vec::new();
        entries.retain(|entry| {
            if seen.contains(&entry.id) {
                return false;
            }
            seen.push(entry.id.clone());
            true
        });
        entries.truncate(CAMPAIGN_ID_KEEP);
    }

    /// 读一个地区的候选（新→旧；过期条目在读取时顺带剔除并回写）。
    pub fn candidates_at(path: &Path, region: Region, now: i64) -> Vec<String> {
        let mut file = load(path);
        let ids: Vec<String>;
        let changed = match file.regions.get_mut(&key(region)) {
            Some(entries) => {
                let before = entries.len();
                prune(entries, now);
                ids = entries.iter().map(|entry| entry.id.clone()).collect();
                entries.len() != before
            }
            None => {
                ids = Vec::new();
                false
            }
        };
        if changed {
            save(path, &mut file);
        }
        ids
    }

    /// 记住一次成功领取的活动 id（去重、置顶、截断）。
    pub fn remember_at(path: &Path, region: Region, campaign_id: &str, now: i64) {
        let campaign_id = campaign_id.trim();
        if !super::is_safe_campaign_id(campaign_id) {
            return;
        }
        let mut file = load(path);
        let entries = file.regions.entry(key(region)).or_default();
        entries.retain(|entry| entry.id != campaign_id);
        entries.insert(0, Entry { id: campaign_id.to_string(), at: now });
        prune(entries, now);
        save(path, &mut file);
    }

    /// 剔除一个过期候选（409 CAMPAIGN_NOT_ACTIVE 后调用）。
    pub fn forget_at(path: &Path, region: Region, campaign_id: &str) {
        let mut file = load(path);
        let Some(entries) = file.regions.get_mut(&key(region)) else { return };
        let before = entries.len();
        entries.retain(|entry| entry.id != campaign_id);
        if entries.len() != before {
            save(path, &mut file);
        }
    }

    fn path() -> std::path::PathBuf {
        crate::server::config::config_dir().join("qoder-campaign-ids.json")
    }

    pub fn candidates(region: Region) -> Vec<String> {
        candidates_at(&path(), region, crate::server::logging::now_ms())
    }

    pub fn remember(region: Region, campaign_id: &str) {
        remember_at(&path(), region, campaign_id, crate::server::logging::now_ms());
    }

    pub fn forget(region: Region, campaign_id: &str) {
        forget_at(&path(), region, campaign_id);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp_path(name: &str) -> std::path::PathBuf {
            let mut path = std::env::temp_dir();
            path.push(format!("qoder-campaign-ids-{}-{}.json", std::process::id(), name));
            let _ = std::fs::remove_file(&path);
            path
        }

        #[test]
        fn remember_candidates_forget_roundtrip() {
            let path = temp_path("roundtrip");
            remember_at(&path, Region::Global, "id-2", 1_000);
            remember_at(&path, Region::Global, "id-1", 2_000);
            remember_at(&path, Region::Global, "id-1", 3_000); // 去重 + 置顶 + 刷新时间
            assert_eq!(candidates_at(&path, Region::Global, 4_000), vec!["id-1", "id-2"]);
            // 地区隔离
            assert!(candidates_at(&path, Region::Cn, 4_000).is_empty());
            forget_at(&path, Region::Global, "id-1");
            assert_eq!(candidates_at(&path, Region::Global, 4_000), vec!["id-2"]);
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn expired_entries_dropped_and_capacity_capped() {
            let path = temp_path("expire");
            let ttl = CAMPAIGN_ID_TTL_MS;
            remember_at(&path, Region::Global, "old", 1_000);
            remember_at(&path, Region::Global, "fresh", 1_000 + ttl - 1);
            assert_eq!(candidates_at(&path, Region::Global, 1_000 + ttl), vec!["fresh"]);
            for index in 0..(CAMPAIGN_ID_KEEP + 3) {
                remember_at(&path, Region::Global, &format!("n{index}"), 2_000 + ttl + index as i64);
            }
            let kept = candidates_at(&path, Region::Global, 9_000 + ttl);
            assert_eq!(kept.len(), CAMPAIGN_ID_KEEP);
            assert_eq!(kept[0], format!("n{}", CAMPAIGN_ID_KEEP + 2));
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn unsafe_ids_never_persisted() {
            let path = temp_path("unsafe");
            remember_at(&path, Region::Global, "../evil?", 1_000);
            remember_at(&path, Region::Global, "  ", 1_000);
            remember_at(&path, Region::Global, "", 1_000);
            assert!(candidates_at(&path, Region::Global, 2_000).is_empty());
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// campaignId 会进 URL 路径，只放行 [A-Za-z0-9_-]（TWA 同款白名单）。
/// 学习表与盲发候选共用同一道闸。
fn is_safe_campaign_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// 往候选表追加一个活动 id（安全白名单 + 去重）
fn push_candidate(out: &mut Vec<String>, id: &str) {
    let id = id.trim();
    if is_safe_campaign_id(id) && !out.iter().any(|existing| existing == id) {
        out.push(id.to_string());
    }
}

/// 收集盲发直领候选：**列表里非 CLAIMED 的 CLAIM_BENEFIT 行**（两种请求形态
/// 都收 —— 上游按形态分桶，主形态看不到的行副形态可能看得到）+ **学习表候选**。
/// 保持顺序（列表行优先、学习表其次）、路径安全白名单去重、截断到
/// [`BLIND_CLAIM_MAX`]。可见的 `CLAIMABLE` 行不走这里（正常路径已领取）。
fn blind_candidates(
    list: &Value,
    alt_list: Option<&Value>,
    learned: &[String],
    now: i64,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for source in [Some(list), alt_list].into_iter().flatten() {
        for row in source.get("campaigns").and_then(Value::as_array).into_iter().flatten() {
            if row.get("actionType").and_then(Value::as_str) != Some(ACTION_CLAIM_BENEFIT) {
                continue;
            }
            if !campaign_is_active(row, now) {
                continue;
            }
            // 本账号已领过的行不是候选（那走 `claimed` → alreadyCompleted）
            if row.get("claimStatus").and_then(Value::as_str) == Some(STATUS_CLAIMED) {
                continue;
            }
            if let Some(id) = row.get("campaignId").and_then(Value::as_str) {
                push_candidate(&mut out, id);
            }
        }
    }
    for id in learned {
        push_candidate(&mut out, id);
    }
    out.truncate(BLIND_CLAIM_MAX);
    out
}

/// Qoder 的每日签到（中国版直领；国际版带设备风控身份两段拉取）。
///
/// 返回 `{success, msg, ...}` —— 与 WorkBuddy / 小浣熊 / AutoClaw 三家同形状
/// （见 `billing::checkin::claim_result`），于是汇总、日志与界面三处不需要
/// 为第四家再加分支。
///
/// `success` 的口径是「**本次真的领到了**」：
///   - 领到 → `true`，`msg` 带金额与有效期，另给 `rewardPoints`（界面显示「本次 +N」）
///     与 `unit`（单位是 credits，不是积分）；
///   - 今天已领 / 没有活动 → `false` + `msg`，其中「已领取」另带
///     `alreadyCompleted: true`，`billing::checkin` 据此落 `checkinAt`（行上的按钮
///     变成「已签到」）；「没有活动」不带它 —— 不能把「今天没得领」记成已签到。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    region: Region,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let mut credentials = refresh::ensure_fresh(store, region, account_id, false).await?;
    let (record, _) = refresh::snapshot(store, region, account_id)?;

    let proxy = auth::account_proxy(&record)?;

    let mut response = fetch_campaigns(&credentials, proxy.as_ref()).await?;
    if response.status == 401 && credentials.can_refresh() {
        credentials = refresh::ensure_fresh(store, region, account_id, true).await?;
        response = fetch_campaigns(&credentials, proxy.as_ref()).await?;
    }
    let list = auth::payload(response, "签到活动查询")?;

    // ── 国际版：先拿顶层 uid，叠风控头重拉一次（中国版不需要）────
    // 401 重拉后的重试同样要叠头 —— 那时令牌换了，但流程不变。
    // 首段（最小头）列表以 `alt_list` 传给收尾：上游按请求形态分桶时，
    // 它可能给出风控头形态看不到的活动行（盲发候选的一部分）。
    let mut headers = endpoints::sash_headers(&credentials.access_token);
    let mut risk_note: Option<String> = None;
    if region == Region::Global {
        let uid = list.get("uid").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let (augmented, note) = risk::augment_headers(headers, region, &uid).await;
        headers = augmented;
        risk_note = note;
        let second = fetch_campaigns_with(&credentials, proxy.as_ref(), &headers).await?;
        if second.status == 401 && credentials.can_refresh() {
            credentials = refresh::ensure_fresh(store, region, account_id, true).await?;
            let third = fetch_campaigns(&credentials, proxy.as_ref()).await?;
            let third = intl_refetch(&credentials, proxy.as_ref(), third).await?;
            return claim_from(store, region, account_id, credentials, proxy, third, Some(&list), risk_note).await;
        }
        let second = auth::payload(second, "签到活动查询（风控重拉）")?;
        return claim_from(store, region, account_id, credentials, proxy, second, Some(&list), risk_note).await;
    }

    claim_from(store, region, account_id, credentials, proxy, list, None, risk_note).await
}

/// 国际版的活动列表重取（叠风控头）。首段的 `uid` 缺失（上游没给）时按空
/// uid 走 —— 风控身份生成不出来，头束退化为「只有客户端身份」，与缺组件
/// 同一表现（上游不下发积分活动，落到 `no_claim` 的说明文案）。
async fn intl_refetch(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
    first: ApiResponse,
) -> Result<Value, GatewayError> {
    let first = auth::payload(first, "签到活动查询")?;
    let uid = first.get("uid").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let (headers, _) = risk::augment_headers(endpoints::sash_headers(&credentials.access_token), credentials.region, &uid).await;
    let second = fetch_campaigns_with(credentials, proxy, &headers).await?;
    auth::payload(second, "签到活动查询（风控重拉）")
}

/// 台账用的**设备级**标识：同一台机器上的所有账号共用这一个值。
///
/// 不能用 `machine::device_id(region, uid)` —— 那是按账号派生的**请求头** id，
/// 拿它当台账键会让「账号 A 今天领过」的记录对账号 B 永远不可见，
/// 设备名额归因（以及「今天别再重试」的判定）就彻底失效了。
fn device_key() -> Option<String> {
    machine::machine_id().ok()
}

/// 活动列表解析 + 领取（两个地区共用的收尾）。
async fn claim_from(
    store: &AccountStore,
    region: Region,
    account_id: &str,
    mut credentials: Credentials,
    proxy: Option<ResolvedProxy>,
    list: Value,
    alt_list: Option<&Value>,
    risk_note: Option<String>,
) -> Result<Value, GatewayError> {
    let display = account_id;
    let rows: Vec<Value> = list
        .get("campaigns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let now = chrono::Utc::now().timestamp();
    let mut claimable: Option<&Value> = None;
    let mut claimed = false;
    for row in &rows {
        if row.get("actionType").and_then(Value::as_str) != Some(ACTION_CLAIM_BENEFIT) {
            continue;
        }
        if !campaign_is_active(row, now) {
            continue;
        }
        match row.get("claimStatus").and_then(Value::as_str) {
            Some(STATUS_CLAIMABLE) if claimable.is_none() => claimable = Some(row),
            Some(STATUS_CLAIMED) => {
                claimed = true;
                // 本账号已领过的行 = 今日活动 id 的最佳学习源（cpa-multi-plugins /
                // TWA 实证：已领 grant 无条件展示，campaign id 全局有效、与账号
                // 无关）—— 记入学习表，让列表被分桶隐藏的其它账号也能直领。
                if let Some(id) = row.get("campaignId").and_then(Value::as_str) {
                    campaign_ids::remember(region, id);
                }
            }
            _ => {}
        }
    }

    logging::log(
        "[Checkin]",
        &format!(
            "Qoder 签到检查（账号={}，地区={}，活动数={}，可领取={}，已领取={}）",
            display,
            region.label(),
            rows.len(),
            claimable.is_some(),
            claimed
        ),
    );

    // 国际版风控头用的账号 uid（活动列表顶层字段；中国版不需要风控头）
    let risk_uid = if region == Region::Global {
        Some(list.get("uid").and_then(Value::as_str).unwrap_or("").trim().to_string())
    } else {
        None
    };

    let Some(target) = claimable else {
        // ── 盲发直领兜底（依据见模块头）─────────────────────────
        // 「列表里没有 CLAIMABLE 行」≠「领不到」：上游按请求形态分桶隐藏活动行，
        // 且 claim 端点不受列表过滤约束（幂等）。候选 = 两种形态的列表里非
        // CLAIMED 的 CLAIM_BENEFIT 行 + 本地学习表（成功领取过的 id）。
        let learned = campaign_ids::candidates(region);
        let candidates = blind_candidates(&list, alt_list, &learned, now);
        let mut blind_note: Option<String> = None;
        if !candidates.is_empty() {
            logging::log(
                "[Checkin]",
                &format!(
                    "Qoder 盲发直领（账号={}，地区={}，候选={}）",
                    display,
                    region.label(),
                    candidates.join(",")
                ),
            );
            let mut failures: Vec<String> = Vec::new();
            for campaign_id in &candidates {
                match claim_one(
                    store,
                    region,
                    account_id,
                    &mut credentials,
                    proxy.as_ref(),
                    campaign_id,
                    risk_uid.as_deref(),
                )
                .await
                {
                    Ok(value) => {
                        let success = value.get("success").and_then(Value::as_bool) == Some(true);
                        let already =
                            value.get("alreadyCompleted").and_then(Value::as_bool) == Some(true);
                        if success || already {
                            return Ok(value);
                        }
                        // 上游按「人」拦了这份每日积分：换候选也不会变，直接收工。
                        if value.get("samePersonLimited").and_then(Value::as_bool) == Some(true) {
                            return Ok(value);
                        }
                        // 未完成/未确认：记失败，试下一个候选
                        let message = value.get("msg").and_then(Value::as_str).unwrap_or("未完成");
                        failures.push(format!("{campaign_id}：{}", truncate_str(message, 80)));
                    }
                    Err(error) => {
                        let stale = error.status_code == 409
                            && error.message.contains(CAMPAIGN_NOT_ACTIVE_TAG);
                        let blocked = error.status_code == 409;
                        failures.push(format!("{campaign_id}：{}", truncate_str(&error.message, 80)));
                        if stale {
                            // 活动批次已轮换：从学习表剔除，试下一个候选
                            campaign_ids::forget(region, campaign_id);
                            continue;
                        }
                        if blocked {
                            // 设备/风控明确拒绝（同一身份再试别的候选没有意义）
                            break;
                        }
                        // 5xx / 网络：试下一个候选
                    }
                }
            }
            blind_note = Some(format!(
                "已按学到的活动 id 盲发直领 {} 个候选，均未成功（{}）",
                candidates.len(),
                truncate_str(&failures.join("；"), 200)
            ));
        }
        // 「盲发也没领到」的归因：本机今天是否已经被**另一个账号**领走。
        // 两个地区都存在「每天每台机器只发一份」的名额（2026-10-28 实测：国内版
        // 三个账号同分钟只有一个 `CLAIMED`，其余返回 `BLOCKED`
        // +`SAME_PERSON_ALREADY_CLAIMED`），所以归因对两个地区都做。
        let holder = match device_key() {
            Some(device) => device_claims::today(region, &device)
                .filter(|claim| !device_claims::claimed_by(claim, account_id))
                .map(|claim| claim.account.chars().take(6).collect::<String>()),
            None => None,
        };
        return Ok(no_claim(region, claimed, risk_note, holder, blind_note));
    };
    let campaign_id = target
        .get("campaignId")
        .and_then(Value::as_str)
        .unwrap_or("");
    if campaign_id.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "Qoder 签到活动缺少活动标识，无法领取",
        ));
    }
    claim_one(
        store,
        region,
        account_id,
        &mut credentials,
        proxy.as_ref(),
        campaign_id,
        risk_uid.as_deref(),
    )
    .await
}

/// 截断一段文本（错误摘要 / 候选归因用，避免日志与文案爆炸）
fn truncate_str(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// 领取单个活动并确认（正常可见路径与盲发兜底共用）。
///
/// 返回签到结果 JSON（success / alreadyCompleted / …，形状见 [`claim_daily_checkin`]）；
/// `Err` 表示传输层 / 上游拒绝（HTTP 非 2xx），错误信息带响应体摘要 —— 盲发候选
/// 的 409 `CAMPAIGN_NOT_ACTIVE`（活动批次轮换）与 409 其它（设备/风控拒绝）靠它
/// 区分。成功时把活动 id 记入本地学习表（`campaign_ids`），国际版另落设备台账。
async fn claim_one(
    store: &AccountStore,
    region: Region,
    account_id: &str,
    credentials: &mut Credentials,
    proxy: Option<&ResolvedProxy>,
    campaign_id: &str,
    risk_uid: Option<&str>,
) -> Result<Value, GatewayError> {
    let display = account_id;
    let url = format!(
        "{}{}/{}/claim",
        region.open_api(),
        endpoints::CAMPAIGNS_PATH,
        campaign_id
    );
    let mut headers = endpoints::sash_headers(&credentials.access_token);
    if region == Region::Global {
        let (augmented, _) = risk::augment_headers(headers, region, risk_uid.unwrap_or("")).await;
        headers = augmented;
    }
    let mut claim = claim_campaign(&url, credentials, proxy, &headers).await?;
    // 上游 409 有两种完全不同的含义：`ALREADY_CLAIMED`（今天已经领过）与
    // `CAMPAIGN_NOT_ACTIVE`（活动批次已轮换，id 过期）。以前把「409」整体当成
    // 「已领取」，于是盲发兜底试到过期 id 时，会把「上游拒绝了这次领取」误报成
    // 「今日已领取」—— 用户看到的是「已领取」，实际一分没到账（2026-10-28 实测：
    // 同一账号先拿到 `BLOCKED/SAME_PERSON_ALREADY_CLAIMED`，紧接着过期 id 的
    // 409 把结果覆盖成「今日已领取」）。
    let not_active = campaign_not_active(&claim);
    // 上游并发/重放时给 409 + `{"result":"ALREADY_CLAIMED"}`（参考实现实测），
    // 那不是失败，是「今天已经领过了」。
    if !claim.ok && !not_active && already_claimed_response(&claim) {
        logging::log(
            "[Checkin]",
            &format!(
                "Qoder 签到结果（账号={}，地区={}，状态=already_claimed，说明=今日已领取）",
                display,
                region.label()
            ),
        );
        return Ok(json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已领取",
        }));
    }
    if claim.status == 401 && credentials.can_refresh() {
        *credentials = refresh::ensure_fresh(store, region, account_id, true).await?;
        // 重签一个新头束（令牌换了，旧头的 Bearer 已失效）
        let mut fresh_headers = endpoints::sash_headers(&credentials.access_token);
        if region == Region::Global {
            let (augmented, _) =
                risk::augment_headers(fresh_headers, region, risk_uid.unwrap_or("")).await;
            fresh_headers = augmented;
        }
        claim = claim_campaign(&url, credentials, proxy, &fresh_headers).await?;
    }
    if !claim.ok {
        // 带响应体摘要（TWA describe_blind_failure 同款）：让 409 的两种业务
        // 形态在错误信息里可读、可分类。
        let head = response_head(&claim);
        return Err(GatewayError::with_status(
            i32::from(claim.status),
            format!("Qoder 签到领取失败（HTTP {}）{head}", claim.status),
        ));
    }
    let body = claim
        .payload
        .filter(Value::is_object)
        .ok_or_else(|| GatewayError::with_status(502, "Qoder 签到领取未返回有效 JSON 对象"))?;
    let claim_body = nested_data(&body);

    if claim_body.get("status").and_then(Value::as_str) != Some(STATUS_CLAIMED) {
        let status = claim_body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("未知");
        let failure = claim_body
            .get("failureCode")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        // `BLOCKED` + `SAME_PERSON_ALREADY_CLAIMED`：上游按「人」发每日积分
        // （同一出口 IP / 同一台机器算同一个人），本机今天已经有一个账号领走了
        // 那唯一一份，其余账号当天一律被拦。这不是「本账号已签到」，也不是可以
        // 重试的失败 —— 换候选、换时间都不会改变判定，所以直接给出归因结果。
        if failure.eq_ignore_ascii_case(FAILURE_SAME_PERSON_TAG) {
            logging::log(
                "[Checkin]",
                &format!(
                    "Qoder 签到结果（账号={}，地区={}，状态=same_person_blocked，失败码={}）",
                    display,
                    region.label(),
                    failure
                ),
            );
            return Ok(json!({
                "success": false,
                "samePersonLimited": true,
                "failureCode": failure,
                "msg": "上游按「人」每天只发一份每日积分（SAME_PERSON_ALREADY_CLAIMED）：今天这份已被本机另一个账号领走，本账号今天领不到；同机多账号需每天轮换谁先领，明天会自动换下一个账号",
            }));
        }
        logging::log(
            "[Checkin]",
            &format!(
                "Qoder 签到结果（账号={}，地区={}，状态=claim_not_completed，上游状态={}，失败码={}）",
                display,
                region.label(),
                status,
                if failure.is_empty() { "无" } else { failure }
            ),
        );
        return Ok(json!({ "success": false, "msg": format!("签到未完成（上游状态 {status}）") }));
    }
    if claim_body
        .get("replayed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        logging::log(
            "[Checkin]",
            &format!(
                "Qoder 签到结果（账号={}，地区={}，状态=already_claimed，说明=今日已领取）",
                display,
                region.label()
            ),
        );
        return Ok(json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已领取",
        }));
    }

    // POST 返回 CLAIMED 只代表接口接受了领取；再查一次活动状态，避免把
    // 2xx 或延迟响应误报成已经到账。失败时不写本地完成标记，后续轮询可重试。
    // 国际版确认也必须带同一套风控头；普通请求只返回 VIEW_DETAILS，
    // 会把已成功领取误判成「领取后状态未确认」。
    let confirmed = if region == Region::Global {
        let (confirmed_headers, _) =
            risk::augment_headers(endpoints::sash_headers(&credentials.access_token), region, risk_uid.unwrap_or("")).await;
        fetch_campaigns_with(credentials, proxy, &confirmed_headers).await?
    } else {
        fetch_campaigns(credentials, proxy).await?
    };
    let confirmed = auth::payload(confirmed, "签到领取确认")?;
    if !campaign_status(nested_data(&confirmed), campaign_id, STATUS_CLAIMED) {
        logging::log(
            "[Checkin]",
            &format!(
                "Qoder 签到结果（账号={}，地区={}，状态=claim_unconfirmed，说明=领取后状态未确认）",
                display,
                region.label()
            ),
        );
        return Ok(json!({
            "success": false,
            "msg": "签到未完成（领取后状态未确认）",
        }));
    }

    let reward = claim_body
        .get("benefit")
        .and_then(|benefit| benefit.get("amount"))
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_REWARD);
    let valid_days = claim_body
        .get("benefit")
        .and_then(|benefit| benefit.get("validity"))
        .and_then(|validity| validity.get("days"))
        .and_then(Value::as_f64);
    // 文案不重复「签到成功」：调用方 `claim_result` 已经用它作日志前缀
    // （「账号 X: 签到成功（…）」），这里只说领到了什么。
    let msg = match valid_days {
        Some(days) if days > 0.0 => format!("获得 {reward} credits，{days} 天有效"),
        _ => format!("获得 {reward} credits"),
    };
    // 学习表：两种形态都看不到活动行的账号（或下次运行）凭这个 id 直领
    campaign_ids::remember(region, campaign_id);
    // 台账记两个地区：上游按「人」发每日积分（同一台机器/同一出口 IP 算一个人），
    // 国内版和国际版都一样（2026-10-28 实测国内版也是「先到先得、其余 BLOCKED」）。
    // 记下今天这台机器的名额归谁，本机其它账号才能被准确归因成「名额已被 #x 领走」，
    // 而不是被当成「窗口没开」反复重试。
    if let Some(device) = device_key() {
        device_claims::record(region, &device, account_id, campaign_id);
    }
    logging::log(
        "[Checkin]",
        &format!(
            "Qoder 签到结果（账号={}，地区={}，状态=claimed_confirmed，奖励={} credits，valid_days={}）",
            display,
            region.label(),
            reward,
            valid_days.map(|days| days.to_string()).unwrap_or_else(|| "unknown".to_string())
        ),
    );
    Ok(json!({
        "success": true,
        "msg": msg,
        "rewardPoints": reward,
        "unit": "credits",
    }))
}

/// 409/4xx/5xx 响应体摘要（错误信息用；JSON 取其紧凑文本，非 JSON 给占位）
fn response_head(response: &ApiResponse) -> String {
    match response.payload.as_ref() {
        Some(payload) => truncate_str(&serde_json::to_string(payload).unwrap_or_default(), 160),
        None => "上游未返回 JSON".to_string(),
    }
}

/// 活动列表（默认头束：`forceRefresh` 拿实时 `claimStatus`）。
async fn fetch_campaigns(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let headers = endpoints::sash_headers(&credentials.access_token);
    fetch_campaigns_with(credentials, proxy, &headers).await
}

/// 活动列表（指定头束 —— 国际版第二段要叠风控头）。
async fn fetch_campaigns_with(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
    headers: &[(String, String)],
) -> Result<ApiResponse, GatewayError> {
    let url = format!(
        "{}{}{}",
        credentials.region.open_api(),
        endpoints::CAMPAIGNS_PATH,
        endpoints::CAMPAIGNS_QUERY
    );
    auth::request("GET", &url, None, headers, proxy).await

}

/// 领取一个活动。请求体是空对象（抓包确认：无参数），`Origin` 指向该地区门户
/// （参考实现只在 POST 上带它）。
async fn claim_campaign(
    url: &str,
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
    headers: &[(String, String)],
) -> Result<ApiResponse, GatewayError> {
    let mut headers = headers.to_vec();
    headers.push(("content-type".to_string(), "application/json".to_string()));
    headers.push((
        "origin".to_string(),
        credentials.region.web_origin().to_string(),
    ));
    auth::request("POST", url, Some(&json!({})), &headers, proxy).await
}

/// 响应体里是否带某个业务码（`result` / `errorCode` / `failureCode` / `status`）。
///
/// 上游在两个位置表达同一种业务结果：`200` 时放在 `status`（`CLAIMED` /
/// `BLOCKED`）+ `failureCode`，`4xx` 时放在 `errorCode`。分类必须读业务码，
/// 不能只看 HTTP 状态码 —— 409 同时用于「已领」和「活动过期」。
fn has_business_code(response: &ApiResponse, code: &str) -> bool {
    let Some(payload) = response.payload.as_ref() else {
        return false;
    };
    let body = nested_data(payload);
    ["result", "errorCode", "failureCode", "status"]
        .iter()
        .filter_map(|key| body.get(*key).and_then(Value::as_str))
        .any(|value| value.trim().eq_ignore_ascii_case(code))
}

/// 409 = 「活动批次已过期」（id 需重学，换个候选还有机会）。
fn campaign_not_active(response: &ApiResponse) -> bool {
    has_business_code(response, CAMPAIGN_NOT_ACTIVE_TAG)
}

/// 「今天已经领过了」的两种上游形态：`result: ALREADY_CLAIMED`（4xx）与
/// `status: CLAIMED` 的重放（由调用方单独判定）。
fn already_claimed_response(response: &ApiResponse) -> bool {
    has_business_code(response, RESULT_ALREADY_CLAIMED)
}

/// 上游部分版本把领取结果放在 `data` 下，部分版本直接返回顶层对象。
fn nested_data(value: &Value) -> &Value {
    value
        .get("data")
        .filter(|item| item.is_object())
        .unwrap_or(value)
}

fn campaign_status(list: &Value, campaign_id: &str, expected: &str) -> bool {
    list.get("campaigns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|row| {
            row.get("campaignId").and_then(Value::as_str) == Some(campaign_id)
                && row.get("actionType").and_then(Value::as_str) == Some(ACTION_CLAIM_BENEFIT)
                && row.get("claimStatus").and_then(Value::as_str) == Some(expected)
        })
}

fn campaign_is_active(row: &Value, now: i64) -> bool {
    let start_ok = campaign_timestamp(row.get("startAt"))
        .map(|start| start <= now)
        .unwrap_or(true);
    let end_ok = campaign_timestamp(row.get("endAt"))
        .map(|end| now < end)
        .unwrap_or(true);
    start_ok && end_ok
}

fn campaign_timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    let raw = value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())?;
    if raw > 100_000_000_000 {
        Some(raw / 1000)
    } else {
        Some(raw)
    }
}

/// 「这次没领到」的结果：区分「今天已领」「没有可领的活动」「缺风控身份」
/// 「本设备名额已被另一个账号领走」与「盲发直领也被上游拒绝」。
///
/// `holder` 是本机今日领取者的账号哈希前缀（两个地区都有：实测国际版与国内版
/// 的每日积分都按「人」发放 —— 同一台机器/同一出口 IP 只有一个账号能领到，
/// 其余账号当天拿到 `BLOCKED`+`SAME_PERSON_ALREADY_CLAIMED`）。
///
/// `blind_note` 是盲发直领兜底的失败摘要（候选 id 直领也全被拒绝时带上，
/// 让用户知道「不只是列表看不到，是直领也被拒」）。
fn no_claim(
    region: Region,
    claimed: bool,
    risk_note: Option<String>,
    holder: Option<String>,
    blind_note: Option<String>,
) -> Value {
    let with_note = |msg: String| match blind_note.as_deref() {
        Some(note) if !note.is_empty() => format!("{msg}；{note}"),
        _ => msg,
    };

    if claimed {
        return json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已领取",
        });
    }
    // 本机今天的名额已经被另一个账号用掉：两个地区都是这个结论，文案只按地区
    // 说明各自的窗口/来源差异。
    if let Some(holder) = holder {
        return json!({
            "success": false,
            "deviceLimited": true,
            "msg": with_note(format!(
                "本机今日的{}每日积分名额已被另一个账号（#{holder}）领走：Qoder 每天按「人」只发一份，同机多账号只能轮换领取（签到中心会按最久未领自动换账号）；本账号今天不会再自动重试",
                region.label()
            )),
        });
    }
    if region == Region::Global {
        // 国际版剩下的两种原因：缺 UMID 组件（风控头没带上，上游不下发积分
        // 活动），或者窗口还没开。
        if let Some(note) = risk_note {
            return json!({
                "success": false,
                "msg": with_note(format!("未领取：{note}。国际版签到需要本机安装 Qoder 客户端或 qodercli（Linux 可在签到中心一键安装组件）")),
            });
        }
        return json!({
            "success": false,
            "msg": with_note("上游暂未给该账号下发每日积分活动；国际版每天按人限领一次，本机其他账号可能已占用今日名额。若确认未领取，请在官方活动窗口（北京时间 10:00 后）重试".to_string()),
        });
    }
    // 中国版没有活动是账号侧状态（实测 Free 套餐账号如此，Pro/试用账号才有）；
    // 「本机名额已被别的账号领走」已在上面按 holder 归因，不落到这里。
    json!({
        "success": false,
        "msg": with_note("当前没有可领取的签到活动".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_window_rejects_future_and_expired_rows() {
        let now = 1_700_000_000;
        assert!(campaign_is_active(
            &json!({"startAt": now - 1, "endAt": now + 1}),
            now
        ));
        assert!(!campaign_is_active(&json!({"startAt": now + 1}), now));
        assert!(!campaign_is_active(&json!({"endAt": now}), now));
        assert!(campaign_is_active(&json!({}), now));
    }

    #[test]
    fn nested_claim_payload_and_status_are_supported() {
        let body = json!({"data": {"status": "CLAIMED", "result": "ALREADY_CLAIMED"}});
        assert_eq!(
            nested_data(&body).get("status").and_then(Value::as_str),
            Some("CLAIMED")
        );
        assert!(campaign_status(
            &json!({"campaigns": [{
                "campaignId": "c1",
                "actionType": "CLAIM_BENEFIT",
                "claimStatus": "CLAIMED"
            }]}),
            "c1",
            STATUS_CLAIMED
        ));
    }

    #[test]
    fn no_claim_explains_shared_daily_limit() {
        let result = no_claim(Region::Global, false, None, None, None);
        let message = result.get("msg").and_then(Value::as_str).unwrap_or("");
        assert!(message.contains("每天按人限领一次"));
        assert!(message.contains("其他账号可能已占用"));
    }

    /// 名字里的 `intl` 是历史名（`docs/local-contracts.json` 按完整用例名绑定，
    /// 不能随语义扩展改名）：2026-10-28 实测确认国内版同样是「每天按人一份」，
    /// 所以断言覆盖两个地区。
    #[test]
    fn intl_no_claim_names_the_device_holder() {
        for region in [Region::Global, Region::Cn] {
            let result = no_claim(region, false, None, Some("a1b2c3".to_string()), None);
            assert_eq!(
                result.get("deviceLimited").and_then(Value::as_bool),
                Some(true),
                "{} 的名额归因要带 deviceLimited",
                region.label()
            );
            let message = result.get("msg").and_then(Value::as_str).unwrap_or("");
            assert!(message.contains("#a1b2c3"));
            assert!(message.contains("不会再自动重试"));
            // 「名额已被领走」不能写成「已领取/已签到」：那会让账号被记成今天
            // 已经领过（`checkin_completed_today` 按文案兜底），从而整天不再重试。
            assert!(!message.contains("已领取"), "{} 不能冒称已领取", region.label());
            assert!(!message.contains("已签到"), "{} 不能冒称已签到", region.label());
        }
        // 没有 holder 时国内版仍说「没有可领取的活动」（Free 账号没有活动行）
        let cn = no_claim(Region::Cn, false, None, None, None);
        assert!(cn.get("deviceLimited").is_none());
        assert!(cn
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("没有可领取"));
    }

    #[test]
    fn no_claim_appends_blind_note_to_every_failure_branch() {
        let note = Some("已按学到的活动 id 盲发直领 2 个候选，均未成功".to_string());
        for region in [Region::Global, Region::Cn] {
            let result = no_claim(region, false, None, None, note.clone());
            let message = result.get("msg").and_then(Value::as_str).unwrap_or("");
            assert!(message.contains("盲发直领"), "地区 {} 的失败文案要带盲发摘要", region.label());
        }
        // 已领取分支不受影响（幂等回放就绪时盲发摘要没有信息量）
        let claimed = no_claim(Region::Global, true, None, None, note);
        assert_eq!(claimed.get("msg").and_then(Value::as_str), Some("今日已领取"));
    }

    #[test]
    fn only_already_claimed_is_treated_as_claimed() {
        let response = |status: u16, body: Value| ApiResponse {
            ok: (200..300).contains(&status),
            status,
            payload: Some(body),
        };
        // 今天已领（4xx 形态）
        let already = response(409, json!({"result": "ALREADY_CLAIMED"}));
        assert!(already_claimed_response(&already));
        assert!(!campaign_not_active(&already));
        // 活动批次过期：**不是**「已领取」，必须留给盲发兜底去换候选
        let stale = response(
            409,
            json!({"errorCode": "CAMPAIGN_NOT_ACTIVE", "errorMessage": "Internal server error"}),
        );
        assert!(campaign_not_active(&stale));
        assert!(!already_claimed_response(&stale));
        // 嵌套在 data 下也认
        let nested = response(409, json!({"data": {"errorCode": "CAMPAIGN_NOT_ACTIVE"}}));
        assert!(campaign_not_active(&nested));
        // 无 body 的 409：两种都不是（按失败处理，不能冒称已领取）
        let bare = response(409, Value::Null);
        assert!(!already_claimed_response(&bare));
        assert!(!campaign_not_active(&bare));
    }

    #[test]
    fn blind_candidates_collect_list_rows_and_learned_ids() {
        let now = 1_700_000_000;
        let list = json!({"campaigns": [
            {"campaignId": "c-visible", "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE", "startAt": now - 10, "endAt": now + 10},
            {"campaignId": "c-hidden", "actionType": "CLAIM_BENEFIT", "startAt": now - 10, "endAt": now + 10},
            {"campaignId": "c-done", "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMED", "startAt": now - 10, "endAt": now + 10},
            {"campaignId": "c-stale", "actionType": "CLAIM_BENEFIT", "claimStatus": "CLAIMABLE", "endAt": now - 1},
            {"campaignId": "c-promo", "actionType": "VIEW_DETAILS", "claimStatus": "CLAIMABLE"}
        ]});
        let learned = vec!["c-learned".to_string(), "c-visible".to_string()];
        let out = blind_candidates(&list, None, &learned, now);
        // 主列表的 CLAIMABLE 行在实际链路里由正常路径领取（走不到盲发），但
        // 收进来无害（claim 幂等）——副形态（alt_list）的 CLAIMABLE 行正是
        // 盲发要救的对象。CLAIMED / 过期 / 非签到类 / 重复项被排除。
        assert_eq!(out, vec!["c-visible", "c-hidden", "c-learned"]);
    }

    #[test]
    fn blind_candidates_merge_alt_list_and_reject_unsafe_ids() {
        let now = 1_700_000_000;
        let list = json!({"campaigns": []});
        let alt = json!({"campaigns": [
            {"campaignId": "../evil", "actionType": "CLAIM_BENEFIT"},
            {"campaignId": "alt-row", "actionType": "CLAIM_BENEFIT", "claimStatus": "PENDING"}
        ]});
        let learned = vec!["ok-id_1".to_string()];
        let out = blind_candidates(&list, Some(&alt), &learned, now);
        assert_eq!(out, vec!["alt-row", "ok-id_1"]);
    }

    #[test]
    fn blind_candidates_are_capped() {
        let now = 1_700_000_000;
        let rows: Vec<Value> = (0..10)
            .map(|index| json!({"campaignId": format!("id{index}"), "actionType": "CLAIM_BENEFIT"}))
            .collect();
        let list = json!({ "campaigns": rows });
        let out = blind_candidates(&list, None, &[], now);
        assert_eq!(out.len(), BLIND_CLAIM_MAX);
    }
}
