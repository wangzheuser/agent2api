//! Qoder 每日活动的**设备名额台账**（按地区 + 设备 + 北京日期）。
//!
//! ── 为什么需要它（2026-10-19 实测）────────────────────────────
//! 国际版的每日 100 Credits 是按**设备**发放的：同一台设备上，当天第一个领取的
//! 账号拿到 `CLAIMED`，其它账号的活动行会**整条消失**（不是变成已领）。
//! 国内版则是按账号发放 —— 同一台设备上两个国内版账号当天都拿到了 `CLAIMED`
//! （实测），所以本台账只对国际版有意义，国内版不写也不读。
//!
//! 台账解决的问题是**归因**而不是限制：没有它，第二个国际版账号的「没有活动」
//! 会被当成「窗口没开」，自动签到一整天反复重试（既没意义又在打上游风控）。
//!
//! ── 数据形态 ────────────────────────────────────────────────
//! `{config_dir}/qoder-device-claims.json`
//! ```json
//! { "version": 1,
//!   "claims": { "global:<设备哈希>:2026-10-19": { "account": "<账号哈希>", "at": 1792..., "campaign": "01a1..." } } }
//! ```
//! 账号只落哈希（与 `machine::device_id` / `risk::identity_cache_key` 同口径），
//! 不落明文 uid；按天清理，只保留最近若干天。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；读失败一律当「没有记录」，
//! 台账不可用绝不能把签到链路带崩。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::endpoints::Region;

/// 台账保留天数（够跨时区/跨天排查，又不至于无限增长）
const RETAIN_DAYS: usize = 14;

#[derive(Serialize, Deserialize)]
struct ClaimsFile {
    #[serde(default = "current_version")]
    version: u32,
    #[serde(default)]
    claims: BTreeMap<String, Claim>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claim {
    /// 领取账号的哈希（不落明文 uid）
    pub account: String,
    /// 领取时间（毫秒）
    #[serde(default)]
    pub at: i64,
    /// 活动标识（排查用）
    #[serde(default)]
    pub campaign: String,
}

fn current_version() -> u32 {
    1
}

impl Default for ClaimsFile {
    fn default() -> Self {
        Self { version: current_version(), claims: BTreeMap::new() }
    }
}

fn account_hash(account_id: &str) -> String {
    let digest = sha2::Sha256::digest(account_id.as_bytes());
    digest.iter().take(12).map(|byte| format!("{byte:02x}")).collect()
}

fn device_hash(device_id: &str) -> String {
    let digest = sha2::Sha256::digest(device_id.as_bytes());
    digest.iter().take(12).map(|byte| format!("{byte:02x}")).collect()
}

fn claims_path() -> PathBuf {
    crate::server::config::config_dir().join("qoder-device-claims.json")
}

fn load(path: &Path) -> ClaimsFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<ClaimsFile>(&text).ok())
        .unwrap_or_default()
}

fn save(path: &Path, file: &ClaimsFile) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_vec_pretty(file) else { return };
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = path.file_name().and_then(|name| name.to_str()).unwrap_or("qoder-device-claims.json");
    let temp = parent.join(format!("{base}.{}.{seq}.tmp", std::process::id()));
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

fn entry_key(region: Region, device_id: &str, date: &str) -> String {
    format!("{}:{}:{}", region.id(), device_hash(device_id), date)
}

/// 记一条「今天这台设备的名额被哪个账号领走了」。
///
/// 只在**领取成功且回查确认**后调用；同一设备同一天重复调用以最后一次为准
/// （跨地区互不影响：key 里带地区）。
pub fn record(region: Region, device_id: &str, account_id: &str, campaign: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let Ok(_guard) = LOCK.lock() else { return };
    let path = claims_path();
    let mut file = load(&path);
    let today = crate::server::core::beijing::today_key();
    file.claims.insert(
        entry_key(region, device_id, &today),
        Claim {
            account: account_hash(account_id),
            at: crate::server::logging::now_ms(),
            campaign: campaign.to_string(),
        },
    );
    prune(&mut file, &today);
    save(&path, &file);
}

/// 今天这台设备的名额归属（没有记录 → None）
pub fn today(region: Region, device_id: &str) -> Option<Claim> {
    let path = claims_path();
    let file = load(&path);
    let today = crate::server::core::beijing::today_key();
    file.claims.get(&entry_key(region, device_id, &today)).cloned()
}

/// 这个账号哈希是不是今天这台设备的领取者
pub fn claimed_by(claim: &Claim, account_id: &str) -> bool {
    claim.account == account_hash(account_id)
}

fn prune(file: &mut ClaimsFile, today: &str) {
    // 按日期键排序，保留最近 RETAIN_DAYS 个不同日期（跨地区共用同一份日期集合）
    let mut dates: Vec<String> = file
        .claims
        .keys()
        .filter_map(|key| key.rsplit(':').next().map(str::to_string))
        .collect();
    dates.sort();
    dates.dedup();
    if dates.len() <= RETAIN_DAYS {
        return;
    }
    let keep: std::collections::BTreeSet<String> = dates.into_iter().rev().take(RETAIN_DAYS).collect();
    file.claims.retain(|key, _| {
        key.rsplit(':').next().map(|date| keep.contains(date)).unwrap_or(false)
    });
    // 今天的记录一定保留（极端情况下时钟跳变也不丢）
    let _ = today;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("qoder-claims-{}-{}.json", std::process::id(), name));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn entry_key_isolated_by_region_device_and_date() {
        let a = entry_key(Region::Global, "device-1", "2026-10-19");
        let b = entry_key(Region::Cn, "device-1", "2026-10-19");
        let c = entry_key(Region::Global, "device-2", "2026-10-19");
        let d = entry_key(Region::Global, "device-1", "2026-10-20");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        // 不落明文设备 id
        assert!(!a.contains("device-1"));
    }

    #[test]
    fn claim_matches_only_the_same_account() {
        let claim = Claim { account: account_hash("account-a"), at: 1, campaign: "c1".into() };
        assert!(claimed_by(&claim, "account-a"));
        assert!(!claimed_by(&claim, "account-b"));
    }

    #[test]
    fn roundtrip_via_file() {
        let path = temp_path("roundtrip");
        let mut file = ClaimsFile::default();
        let key = entry_key(Region::Global, "device-1", "2026-10-19");
        file.claims.insert(
            key.clone(),
            Claim { account: account_hash("a"), at: 42, campaign: "c".into() },
        );
        save(&path, &file);
        let loaded = load(&path);
        assert_eq!(loaded.claims.get(&key).map(|c| c.at), Some(42));
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!text.contains("device-1"));
        assert!(!text.contains("\"a\""));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_keeps_recent_days() {
        let mut file = ClaimsFile::default();
        for day in 1..=20 {
            let date = format!("2026-09-{day:02}");
            file.claims.insert(
                entry_key(Region::Global, "d", &date),
                Claim { account: "x".into(), at: 0, campaign: String::new() },
            );
        }
        prune(&mut file, "2026-09-20");
        assert_eq!(file.claims.len(), RETAIN_DAYS);
        assert!(file.claims.keys().any(|key| key.ends_with("2026-09-20")));
        assert!(!file.claims.keys().any(|key| key.ends_with("2026-09-01")));
    }
}
