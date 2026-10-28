//! PKCE 随机串、本机标识与**按账号绑定的设备身份**。随机源失败时终止登录，不降级为可预测值。
//!
//! ── 为什么还要「按账号」的设备身份 ────────────────────────────
//! Qoder 的每日积分活动（`sash` 一族）把 `Cosy-MachineId` 当作设备标识。
//! 同一台机器上的多个账号如果共用一个 id，上游看到的就是**一台设备**，
//! 国际版每天只会给其中一个账号下发可领取活动（见 `risk.rs` 模块头与
//! CreditDaddy README 的「每台设备每日限领一个国际版账号」）。
//!
//! 这里给每个账号分配一份**独立且持久**的机器标识：首次使用时生成一次，
//! 之后永远复用（落盘在 `{config_dir}/qoder-device-ids.json`）。
//! 不做「每次执行重新生成」——那会让同一账号的设备指纹反复变化，
//! 在上游看来像是频繁换机，属于典型的异常特征。
//!
//! 与 [`machine_id`] 的分工：后者是**设备级**标识（COSY 签名 / 设备授权 /
//! 刷新令牌打包用，见 `credentials.rs`），口径不变；本模块的
//! [`device_id`] 只用于活动/签到请求头，两者互不覆盖。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::server::errors::GatewayError;

use super::endpoints::Region;

static MACHINE_ID: Mutex<Option<String>> = Mutex::new(None);
static DEVICE_IDS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub fn random_secret() -> Result<String, GatewayError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| GatewayError::with_status(500, "无法生成安全的 Qoder 登录随机串"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn random_uuid() -> Result<String, GatewayError> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| GatewayError::with_status(500, "无法生成 Qoder 机器标识"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

fn read_id(path: &std::path::Path) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?.trim().to_string();
    (!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then_some(value)
}

pub fn machine_id() -> Result<String, GatewayError> {
    let mut cached = MACHINE_ID.lock()
        .map_err(|_| GatewayError::with_status(500, "Qoder 机器标识锁不可用"))?;
    if let Some(value) = cached.as_ref() {
        return Ok(value.clone());
    }
    let directory = crate::server::config::config_dir();
    let path = directory.join("qoder-machine-id");
    if let Some(value) = read_id(&path) {
        *cached = Some(value.clone());
        return Ok(value);
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    let existing = home.map(PathBuf::from).and_then(|home| {
        [
            home.join(".qoder-proxy/machine_id"),
            home.join(".qoder/.auth/machine_id"),
            home.join(".qoder/machine_id"),
        ].iter().find_map(|candidate| read_id(candidate))
    });
    let value = match existing {
        Some(value) => value,
        None => random_uuid()?,
    };
    std::fs::create_dir_all(&directory)
        .and_then(|_| std::fs::write(&path, &value))
        .map_err(|error| GatewayError::with_status(500, format!("保存 Qoder 机器标识失败: {error}")))?;
    *cached = Some(value.clone());
    Ok(value)
}

// ─── 按账号绑定的设备身份 ────────────────────────────────────

/// 落盘形态（`{config_dir}/qoder-device-ids.json`）
#[derive(Serialize, Deserialize)]
struct DeviceIdFile {
    /// 结构版本（将来加字段时用来判断要不要迁移）
    #[serde(default = "current_version")]
    version: u32,
    /// `"<region>:<账号标识哈希>" → 记录`
    #[serde(default)]
    accounts: BTreeMap<String, DeviceIdEntry>,
}

#[derive(Serialize, Deserialize)]
struct DeviceIdEntry {
    /// 分配给该账号的机器标识（UUID v4，只生成一次）
    id: String,
    /// 分配时间（毫秒）
    #[serde(default)]
    created_at: i64,
    /// 最近一次使用时间（毫秒；只在分配时写一次，避免每次请求都落盘）
    #[serde(default)]
    last_used_at: i64,
}

fn current_version() -> u32 {
    1
}

impl Default for DeviceIdFile {
    fn default() -> Self {
        Self { version: current_version(), accounts: BTreeMap::new() }
    }
}

/// 账号标识的哈希（不落明文 uid；与 `device_claims` 的账号哈希同口径）
fn account_hash(uid: &str) -> String {
    let digest = sha2::Sha256::digest(uid.as_bytes());
    digest.iter().take(12).map(|byte| format!("{byte:02x}")).collect()
}

fn device_key(region: Region, uid: &str) -> String {
    format!("{}:{}", region.id(), account_hash(uid))
}

fn device_ids_path() -> PathBuf {
    crate::server::config::config_dir().join("qoder-device-ids.json")
}

fn load_device_ids(path: &Path) -> DeviceIdFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<DeviceIdFile>(&text).ok())
        .unwrap_or_default()
}

fn save_device_ids(path: &Path, file: &DeviceIdFile) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_vec_pretty(file) else { return };
    // 临时名必须唯一：同一进程内并行测试/并发请求会同时写，只带 pid 会互相覆盖
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("qoder-device-ids.json");
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

/// 纯文件版本：同一个 `(地区, 账号)` 永远拿到同一个 id。
///
/// 读-改-写之间有进程级锁：两个账号同时首次签到时不加锁会互相覆盖对方刚分配的
/// 记录（后写者把先写者的条目吞掉），表现为其中一台「设备」下次又换 id。
fn device_id_at(path: &Path, region: Region, uid: &str) -> Result<String, GatewayError> {
    static ALLOCATE_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ALLOCATE_LOCK.lock().map_err(|_| GatewayError::with_status(500, "Qoder 设备身份锁不可用"))?;
    let key = device_key(region, uid);
    let mut file = load_device_ids(path);
    if let Some(entry) = file.accounts.get(&key) {
        if !entry.id.is_empty() {
            return Ok(entry.id.clone());
        }
    }
    let id = random_uuid()?;
    let now = crate::server::logging::now_ms();
    file.accounts.insert(
        key,
        DeviceIdEntry { id: id.clone(), created_at: now, last_used_at: now },
    );
    save_device_ids(path, &file);
    Ok(id)
}

/// 某账号的活动请求用机器标识（`Cosy-MachineId`）。
///
/// - **一次分配、永久复用**：落盘后不再变化，跨重启稳定；
/// - 不同账号不同 id（同地区内每个账号一台「设备」）；
/// - `uid` 为空时退化到设备级 [`machine_id`]（拿不到账号身份时不做区分）；
/// - 环境变量 `AGENT2API_QODER_DEVICE_ID_SCOPE=device` 可整体退回设备级 id
///   （实测上游按硬件身份判定设备名额，`Cosy-MachineId` 不参与该判定；
///   若某个上游版本开始校验它与机器码的一致性，用这个开关一键回退，无需改代码）。
pub fn device_id(region: Region, uid: &str) -> Result<String, GatewayError> {
    if std::env::var("AGENT2API_QODER_DEVICE_ID_SCOPE")
        .map(|value| value.trim().eq_ignore_ascii_case("device"))
        .unwrap_or(false)
    {
        return machine_id();
    }
    let uid = uid.trim();
    if uid.is_empty() {
        return machine_id();
    }
    let key = device_key(region, uid);
    if let Ok(guard) = DEVICE_IDS.lock() {
        if let Some(map) = guard.as_ref() {
            if let Some(value) = map.get(&key) {
                return Ok(value.clone());
            }
        }
    }
    let value = device_id_at(&device_ids_path(), region, uid)?;
    if let Ok(mut guard) = DEVICE_IDS.lock() {
        guard.get_or_insert_with(HashMap::new).insert(key, value.clone());
    }
    Ok(value)
}

// ─── 按账号合成身份（实验开关，默认关闭）────────────────────

/// 合成身份开关：`AGENT2API_QODER_SYNTHETIC_IDENTITY=1` 时，活动请求改用按账号
/// **稳定派生**的 `machineToken / machineCode / machineType`，不再调用 UMID 组件。
///
/// ── 为什么是开关而不是默认 ────────────────────────────────────
/// 2026-10-19 实测：国际版每日活动按设备发放，第一个账号领取后同设备其它账号的
/// 活动行整条消失；直接对已领活动 id 发 claim 返回 409 `campaign is not available
/// for claim`。社区实现（smart-open/TraeWorkAssistant `tasks/qoder_device.rs`）声称
/// 服务端对 `machine_token` 无强绑定、可按账号现场生成——其依据（bzym2/QoderGateway
/// `src/qoder2api/auth.py new_machine`：machine_id 账号级持久、machine_token/type
/// 每会话现场随机，服务端实测放行）来自国际站 COSY/网关域，**不是** sash 活动域；
/// 而 TWA 在 sash 域的探针发现派生/伪造三件套会被上游归入不信任分桶（列表无
/// CLAIMABLE、claim 503/BLOCKED）。两者要靠「名额重置后」的对照实验才能判定，
/// 所以默认关闭：先用 `scripts/qoder-device-experiment.mjs` 跑 B（真实身份）vs
/// D（合成身份），确认合成身份能另开一份名额再打开。即便合成身份不被活动域
/// 接受，`checkin.rs` 的盲发直领兜底（学习 campaign id 后每账号直领）也不依赖
/// 这个开关 —— 它用本机真值三件套 + 按账号的 `device_id` 完成「每账号一台设备」。
///
/// ── 2026-10-28 补记（交叉验证后的边界）────────────────────────
/// [`synthetic_identity`] 的输出形态已按官方组件逐字节对齐（标记位齐全），
/// 但**形态对齐不等于能用**：`mmqz/cpa-multi-plugins` 的取证结论是「国际版
/// sash 域对**派生/轮换**身份是身份级过滤，与形态无关」——换回官方形态的
/// 派生值仍会被过滤；`LiSeafood/AiCheckin` 的实测也印证了同一条业务规则
/// （国际版多账号每天合计 100 Credits、按「人」去重）。所以国际版不要指望
/// 这个开关绕过名额，它的正当用途只有「本机没有 UMID 组件时给一份形态合法
/// 的兜底身份」。
pub fn synthetic_identity_enabled() -> bool {
    std::env::var("AGENT2API_QODER_SYNTHETIC_IDENTITY")
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// 官方 UMID 组件的 `machineToken` 固定头：`3f 58 00` 三字节（base64url
/// 渲染恒为字面量前缀 `P1gA`）。
///
/// 这个头不是装饰。`mmqz/cpa-multi-plugins` 对官方 `runtime-info` 的取证
/// （`plugins/qoder/machine_identity.go`，v0.8.44，9 次官方桥实跑）记录：上游
/// 反风控层会识别「形态不像官方」的令牌，把**设备定向活动行静默丢弃**——每日
/// 100 Credits 正是设备定向行之一。旧派生方案（43 位无头令牌）被称为「派生
/// 身份被过滤」的根因就在这里：不是派生值不被接受，是形态被判定为非官方。
const MACHINE_TOKEN_HEADER: [u8; 3] = [0x3f, 0x58, 0x00];

/// 官方 `machineType` 的固定中段标记（18 hex = 8hex + `91` + 8hex）。
const MACHINE_TYPE_MARKER: &str = "91";

/// 官方 `machineCode` 的固定中段标记（18 hex = 8hex + `00` + 8hex）。
const MACHINE_CODE_MARKER: &str = "00";

/// 按账号稳定派生的三件套，**逐字节复刻官方组件的输出形态**。
///
/// | 字段 | 长度 | 形态 |
/// |---|---|---|
/// | `machineToken` | 88（base64url 无填充） | 66 字节 = `3f 58 00` 固定头 + 63 字节派生体 |
/// | `machineType` | 18（小写 hex） | 8hex + `91` + 8hex |
/// | `machineCode` | 18（小写 hex） | 8hex + `00` + 8hex |
///
/// 只复刻形态、不复刻取值：三个字段仍按 `(region, uid)` 稳定派生，所以
/// **同一账号长期固定在同一台「设备」上、不同账号天然落在不同设备**。
///
/// ⚠️ 形态正确≠上游一定认：`intl` 侧对**非真实**身份的过滤是身份级的
/// （见 [`synthetic_identity_enabled`] 的说明）。这里只保证不再因为「形态
/// 畸形」被当成非官方身份。
pub fn synthetic_identity(region: Region, uid: &str) -> (String, String, String) {
    let seed = format!("{}:{}", region.id(), uid.trim());
    let token = synthetic_machine_token(&seed);
    let code = synthetic_machine_code(&seed);
    let kind = synthetic_machine_type(&seed);
    (token, code, kind)
}

/// `machineToken`：66 字节 = 固定头 + 63 字节派生体 → 88 字符 base64url。
fn synthetic_machine_token(seed: &str) -> String {
    let mut raw = [0u8; 66];
    raw[..MACHINE_TOKEN_HEADER.len()].copy_from_slice(&MACHINE_TOKEN_HEADER);
    raw[MACHINE_TOKEN_HEADER.len()..].copy_from_slice(&derived_bytes(seed, "token", 63));
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// `machineType`：8hex + `91` + 8hex。
fn synthetic_machine_type(seed: &str) -> String {
    let bytes = derived_bytes(seed, "type", 8);
    format!("{}{MACHINE_TYPE_MARKER}{}", hex_of(&bytes[..4]), hex_of(&bytes[4..]))
}

/// `machineCode`：8hex + `00` + 8hex。
fn synthetic_machine_code(seed: &str) -> String {
    let bytes = derived_bytes(seed, "code", 8);
    format!("{}{MACHINE_CODE_MARKER}{}", hex_of(&bytes[..4]), hex_of(&bytes[4..]))
}

/// 从种子流派生指定字节数（sha256 链，确定性 → 同账号每次都一样）
fn derived_bytes(seed: &str, label: &str, count: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(count + 32);
    let mut round: u32 = 0;
    while out.len() < count {
        let mut hasher = sha2::Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.update(b":");
        hasher.update(label.as_bytes());
        hasher.update(b":");
        hasher.update(round.to_string().as_bytes());
        out.extend_from_slice(&hasher.finalize());
        round = round.saturating_add(1);
    }
    out.truncate(count);
    out
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_identity_matches_component_formats() {
        let (token, code, kind) = synthetic_identity(Region::Global, "account-a");
        assert_eq!(token.len(), 88);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(code.len(), 18);
        assert!(code.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(kind.len(), 18);
        assert!(kind.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    /// 官方形态的**标记位**不能丢：`3f 58 00` 头（→ `P1gA` 前缀）、type 的 `91`、
    /// code 的 `00`。缺任何一个都会被上游判成非官方身份，设备定向活动行被静默
    /// 丢弃（每日 100 Credits 就是其中之一）。
    #[test]
    fn synthetic_identity_keeps_official_markers() {
        for region in [Region::Cn, Region::Global] {
            for uid in ["account-a", "account-b"] {
                let (token, code, kind) = synthetic_identity(region, uid);
                let label = region.label();
                assert!(token.starts_with("P1gA"), "{label}/{uid} token 缺 3f5800 头：{token}");
                assert_eq!(&kind[8..10], MACHINE_TYPE_MARKER, "{label}/{uid} type 缺 91 标记：{kind}");
                assert_eq!(&code[8..10], MACHINE_CODE_MARKER, "{label}/{uid} code 缺 00 标记：{code}");
            }
        }
        // 固定头逐字节成立（不是碰巧的字符串前缀）
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(synthetic_identity(Region::Cn, "account-a").0)
            .expect("token 必须是合法 base64url");
        assert_eq!(raw.len(), 66);
        assert_eq!(&raw[..3], &MACHINE_TOKEN_HEADER);
    }

    #[test]
    fn synthetic_identity_is_stable_and_account_scoped() {
        let a1 = synthetic_identity(Region::Global, "account-a");
        let a2 = synthetic_identity(Region::Global, "account-a");
        let b = synthetic_identity(Region::Global, "account-b");
        let cn = synthetic_identity(Region::Cn, "account-a");
        assert_eq!(a1, a2, "同账号必须稳定");
        assert_ne!(a1.0, b.0, "不同账号必须不同");
        assert_ne!(a1.0, cn.0, "不同地区必须不同");
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("qoder-device-ids-{}-{}.json", std::process::id(), name));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn device_id_is_stable_for_same_account() {
        let path = temp_path("stable");
        let first = device_id_at(&path, Region::Global, "account-a").expect("allocate");
        let second = device_id_at(&path, Region::Global, "account-a").expect("reload");
        assert_eq!(first, second);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn device_id_differs_per_account_and_region() {
        let path = temp_path("distinct");
        let a = device_id_at(&path, Region::Global, "account-a").expect("a");
        let b = device_id_at(&path, Region::Global, "account-b").expect("b");
        let cn = device_id_at(&path, Region::Cn, "account-a").expect("cn");
        assert_ne!(a, b);
        assert_ne!(a, cn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn device_id_survives_reload_from_disk() {
        let path = temp_path("reload");
        let first = device_id_at(&path, Region::Global, "account-a").expect("first");
        // 模拟进程重启：清空内存缓存后重新读文件
        if let Ok(mut guard) = DEVICE_IDS.lock() {
            *guard = None;
        }
        let reloaded = device_id_at(&path, Region::Global, "account-a").expect("reload");
        assert_eq!(first, reloaded);
        let text = std::fs::read_to_string(&path).expect("file");
        assert!(text.contains(&first));
        // 明文 uid 不落盘
        assert!(!text.contains("account-a"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn device_key_never_contains_raw_uid() {
        let key = device_key(Region::Cn, "user@example.com");
        assert!(key.starts_with("cn:"));
        assert!(!key.contains("user@example.com"));
    }
}
