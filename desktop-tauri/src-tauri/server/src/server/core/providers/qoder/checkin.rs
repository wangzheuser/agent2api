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
//! 原因就是缺头，不是没活动。先拉一次拿 uid → 叠风控头重拉 → 再领。
//! 国内版读取得到本机官方身份时同样携带，读取失败保留最小头路径。
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

    // 国内版原有最小头已返回有效每日活动时，保留这条已能工作的链路。
    if !needs_identity_refetch(region, &list, chrono::Utc::now().timestamp()) {
        let headers = endpoints::sash_headers(&credentials.access_token);
        return claim_from(store, region, account_id, credentials, proxy, list, None, headers, None).await;
    }
    let uid = list
        .get("uid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|uid| !uid.is_empty())
        .unwrap_or(&credentials.user_id);
    let (mut headers, risk_note) = campaign_identity_headers(&credentials, uid).await;
    let use_identity = region == Region::Global || risk_note.is_none();
    // 一次签到只采集一份身份；列表、领取、确认及 401 重试不能中途换设备。
    if use_identity {
        let mut second = fetch_campaigns_with(&credentials, proxy.as_ref(), &headers).await?;
        if second.status == 401 && credentials.can_refresh() {
            credentials = refresh::ensure_fresh(store, region, account_id, true).await?;
            replace_campaign_token(&mut headers, &credentials.access_token);
            second = fetch_campaigns_with(&credentials, proxy.as_ref(), &headers).await?;
        }
        let second = auth::payload(second, "签到活动查询（风控重拉）")?;
        return claim_from(store, region, account_id, credentials, proxy, second, Some(&list), headers, risk_note).await;
    }

    // 国内版缺少官方组件或读取失败时不发送半套身份，保持原有最小头行为。
    claim_from(store, region, account_id, credentials, proxy, list, None, headers, risk_note).await
}

fn needs_identity_refetch(region: Region, list: &Value, now: i64) -> bool {
    region == Region::Global || !list.get("campaigns").and_then(Value::as_array).is_some_and(|rows| {
        rows.iter().any(|row| {
            row.get("actionType").and_then(Value::as_str) == Some(ACTION_CLAIM_BENEFIT)
                && matches!(row.get("claimStatus").and_then(Value::as_str), Some(STATUS_CLAIMABLE | STATUS_CLAIMED))
                && campaign_is_active(row, now)
        })
    })
}

async fn campaign_identity_headers(
    credentials: &Credentials,
    uid: &str,
) -> (Vec<(String, String)>, Option<String>) {
    let base = endpoints::sash_headers(&credentials.access_token);
    if credentials.region == Region::Global {
        return risk::augment_headers(base, credentials.region, uid).await;
    }
    let (headers, note) = risk::augment_native_headers(base, credentials.region, uid).await;
    if note.is_some() {
        return (endpoints::sash_headers(&credentials.access_token), note);
    }
    (headers, note)
}

fn replace_campaign_token(headers: &mut Vec<(String, String)>, token: &str) {
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
    headers.push(("authorization".to_string(), format!("Bearer {token}")));
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
    mut headers: Vec<(String, String)>,
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
                    &mut headers,
                )
                .await
                {
                    Ok(value) => {
                        // Accepted / confirmed claims are terminal even when credit
                        // evidence is pending. Preserve their state; never POST another candidate.
                        if evidence::stops_claim_attempts(&value) {
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
        &mut headers,
    )
    .await
}

/// 截断一段文本（错误摘要 / 候选归因用，避免日志与文案爆炸）
fn truncate_str(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[path = "claim_evidence.rs"]
mod evidence;

/// Optional direct quota observation: never refresh credentials, query plans, or
/// turn a missing bucket / failed request into an invented zero baseline.
async fn fetch_credit_quota(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Option<evidence::QuotaSnapshot> {
    let response = auth::request(
        "GET",
        &format!("{}/api/v2/quota/usage", credentials.region.open_api()),
        None,
        &endpoints::open_api_headers(Some(&credentials.access_token)),
        proxy,
    ).await.ok()?;
    if !response.ok { return None; }
    evidence::quota_snapshot(response.payload.as_ref()?)
}

fn quota_evidence_value(value: Option<evidence::QuotaSnapshot>) -> Value {
    match value {
        Some(value) => json!({"total":value.total,"remaining":value.remaining,"unit":"credits"}),
        None => Value::Null,
    }
}

/// One ordinary claim attempt: acceptance, campaign confirmation, and quota
/// observations are separate. Unknown reward evidence never becomes a default
/// amount, and an accepted request is terminal even while confirmation is pending.
async fn claim_one(
    store: &AccountStore,
    region: Region,
    account_id: &str,
    credentials: &mut Credentials,
    proxy: Option<&ResolvedProxy>,
    campaign_id: &str,
    headers: &mut Vec<(String, String)>,
) -> Result<Value, GatewayError> {
    let before = fetch_credit_quota(credentials, proxy).await;
    let url = format!("{}{}/{}/claim", region.open_api(), endpoints::CAMPAIGNS_PATH, campaign_id);
    let mut claim = claim_campaign(&url, credentials, proxy, headers).await?;
    // Classify only the final response, including the one normal auth retry.
    if claim.status == 401 && credentials.can_refresh() {
        *credentials = refresh::ensure_fresh(store, region, account_id, true).await?;
        replace_campaign_token(headers, &credentials.access_token);
        claim = claim_campaign(&url, credentials, proxy, headers).await?;
    }
    if has_business_code(&claim, FAILURE_SAME_PERSON_TAG) {
        return Ok(json!({
            "success":false,"samePersonLimited":true,"failureCode":FAILURE_SAME_PERSON_TAG,
            "msg":"上游返回同人领取限制，本账号本次未领取；未将其标为已完成"
        }));
    }
    if !campaign_not_active(&claim)
        && (already_claimed_response(&claim)
            || claim.payload.as_ref().is_some_and(evidence::already_claimed)) {
        return Ok(json!({
            "success":false,"alreadyCompleted":true,"msg":"今日已领取",
            "rewardPoints":Value::Null,"creditIncreaseObserved":false
        }));
    }
    if !claim.ok {
        let head = response_head(&claim);
        return Err(GatewayError::with_status(
            i32::from(claim.status),
            format!("Qoder 签到领取失败（HTTP {}）{head}", claim.status),
        ));
    }
    let body = claim.payload.filter(Value::is_object)
        .ok_or_else(|| GatewayError::with_status(502,"Qoder 签到领取未返回有效 JSON 对象"))?;
    let claim_body = nested_data(&body);
    if claim_body.get("status").and_then(Value::as_str) != Some(STATUS_CLAIMED) {
        return Ok(json!({"success":false,"msg":"上游未确认本次领取"}));
    }
    let reward = evidence::credit_reward(claim_body);
    // An accepted POST is never re-sent merely because confirmation is delayed.
    // A failed / stale confirmation remains structured pending evidence.
    let confirmed = match fetch_campaigns_with(credentials, proxy, headers).await {
        Ok(response) => auth::payload(response,"签到领取确认").ok()
            .is_some_and(|value| campaign_status(nested_data(&value),campaign_id,STATUS_CLAIMED)),
        Err(_) => false,
    };
    if !confirmed {
        return Ok(json!({
            "success":false,"claimAccepted":true,"claimConfirmed":false,"claimUnconfirmed":true,
            "status":"claim_confirmation_pending","reportedReward":reward,"rewardPoints":Value::Null,
            "creditIncreaseObserved":false,"grantReceiptVerified":false,
            "msg":"领取接口已接受，活动状态待核实；暂不计新增积分"
        }));
    }
    let mut after = fetch_credit_quota(credentials, proxy).await;
    if !evidence::credit_increase_observed(claim_body,true,before,after) {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        after = fetch_credit_quota(credentials, proxy).await;
    }
    let increased = evidence::credit_increase_observed(claim_body,true,before,after);
    let msg = if increased {
        format!("活动领取已确认，观察到资源包额度增加 {} credits",reward.unwrap_or(0.0))
    } else {
        "活动领取已确认，新增积分尚未核实；不重复发起领取".to_string()
    };
    // Preserve existing bookkeeping, without changing identity or eligibility.
    campaign_ids::remember(region,campaign_id);
    if let Some(device) = device_key() {
        device_claims::record(region,&device,account_id,campaign_id);
    }
    logging::log("[Checkin]",&format!(
        "Qoder 签到确认（账号={}，地区={}，活动已领取=true，额度增长已观察={}）",
        account_id,region.label(),increased
    ));
    Ok(json!({
        "success":increased,"status":"claimed","claimAccepted":true,"claimConfirmed":true,
        "creditIncreaseObserved":increased,"grantReceiptVerified":false,
        "msg":msg,"rewardPoints":if increased { reward } else { None },
        "reportedReward":reward,"unit":reward.map(|_|"credits"),
        "creditEvidence":{
            "source":"quota_delta","attribution":"correlated_not_independent_receipt",
            "before":quota_evidence_value(before),"after":quota_evidence_value(after),
            "observedTotalDelta":evidence::observed_delta(before,after)
        }
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
    fn native_refetch_preserves_cn_campaigns_and_requires_intl_identity() {
        let now = 1_700_000_000;
        for status in [STATUS_CLAIMABLE, STATUS_CLAIMED] {
            let list = json!({"campaigns": [{
                "actionType": ACTION_CLAIM_BENEFIT,
                "claimStatus": status,
                "startAt": now - 1,
                "endAt": now + 1
            }]});
            assert!(!needs_identity_refetch(Region::Cn, &list, now));
            assert!(needs_identity_refetch(Region::Global, &list, now));
            assert!(needs_identity_refetch(Region::Cn, &list, now + 2));
        }
        assert!(needs_identity_refetch(Region::Cn, &json!({"campaigns": []}), now));
        assert!(needs_identity_refetch(Region::Cn, &json!({"campaigns": [{
            "actionType": "VIEW_DETAILS", "claimStatus": STATUS_CLAIMABLE
        }]}), now));
    }

    #[tokio::test]
    async fn cn_native_identity_failure_keeps_minimal_headers() {
        let credentials = Credentials::from_payload(&json!({
            "mode": "cn", "accessToken": "test-device-token"
        })).expect("fixture credentials");
        // 空 uid 保证不启动真实组件；即使本机安装了 Qoder 也不会采集硬件身份。
        let (headers, note) = campaign_identity_headers(&credentials, "").await;
        assert!(note.is_some());
        assert_eq!(headers, endpoints::sash_headers(&credentials.access_token));
        assert!(!headers.iter().any(|(name, _)| name.to_ascii_lowercase().starts_with("cosy-machine")));
    }

    #[test]
    fn campaign_token_refresh_preserves_identity_snapshot() {
        let mut headers = endpoints::sash_headers("old-token");
        for field in ["Cosy-MachineId", "Cosy-MachineToken", "Cosy-MachineCode", "Cosy-MachineType"] {
            headers.push((field.to_string(), format!("fixture-{field}")));
        }
        let identity: Vec<_> = headers.iter().filter(|(name, _)| !name.eq_ignore_ascii_case("authorization")).cloned().collect();
        // 清理大小写不同的旧鉴权头，同时保留原始身份与非鉴权头的顺序。
        headers.push(("Authorization".to_string(), "Bearer duplicate".to_string()));
        replace_campaign_token(&mut headers, "fresh-token");
        replace_campaign_token(&mut headers, "newest-token");
        assert_eq!(headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("authorization")).count(), 1);
        assert_eq!(headers.last(), Some(&("authorization".to_string(), "Bearer newest-token".to_string())));
        assert_eq!(headers.iter().filter(|(name, _)| !name.eq_ignore_ascii_case("authorization")).cloned().collect::<Vec<_>>(), identity);
    }

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
