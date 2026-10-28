//! Qoder 设备风控身份（`Cosy-MachineToken` / `Cosy-MachineCode` /
//! `Cosy-MachineType`）——国际版签到的关键依赖。
//!
//! ── 为什么需要它（CreditDaddy 的实测结论，issue #140）──────────
//! 国际版服务端**只对带风控头的请求**下发「每天领 100 Credits」的活动
//! （campaign）：不带时 `GET /sash/api/v1/me/campaigns` 里永远只有
//! `VIEW_DETAILS` 促销 —— 这正是本家旧注释「国际版没有签到计划」的真正
//! 原因（缺头，不是没活动）。中国版不需要风控头就能领，两个分支见
//! `checkin.rs`。
//!
//! ── 三个值从哪来 ────────────────────────────────────────────
//! 由 Qoder 的 **UMID 程序**（`runtime-info`）在本机生成：
//!   - Qoder 客户端自带：`<安装目录>/resources/umid/runtime-info(.exe)`；
//!   - 官方 qodercli 解压：`~/.qoder/.bin/umid-<平台>-<架构>-*/runtime-info`
//!     （国内版 `~/.qoder-cn/...`）—— 本文件发现顺序的第二优先级；
//!   - 网关数据目录：Linux/Docker 上前两个都不存在，`install_component`
//!     从官方 qodercli 的 npm 包里提取一份存到 `{config_dir}/qoder-umid/`。
//!
//! 调用方式照抄参考实现：`runtime-info <env>`（Windows / macOS 另带
//! `--account-stdin` 并把 `{"account":"<uid>"}` 写进 stdin），第一行输出
//! JSON `{machineToken, machineCode, machineType}`。`env` 是环境编号：
//! 客户端组件国际版 3 / 中国版 0；qodercli 组件国际版 4（SINGAPORE）/
//! 中国版 0。**同variant组件缺失时回落另一 variant 的**（CreditDaddy 的
//! `riskRunner` 同款：协议一致，只有 env 编号不同）。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 客户端每 60±5 分钟轮换一次身份，这里保守缓存 50 分钟（按 region）。
//! 完整身份同时落盘，避免容器重启后重新采样出不同的 VM 风控分支。
//!
//! ── panic=abort ─────────────────────────────────────────────
//! 零 unwrap/expect/panic；子进程输出不信任（按行解析 + 长度上限）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::server::errors::GatewayError;
use crate::server::logging;

use super::endpoints::Region;
use super::machine;

/// 身份缓存时长（客户端 60±5 分钟轮换，保守取 50 分钟）
const RISK_TTL_MS: i64 = 50 * 60 * 1000;
/// 单次组件调用超时（与客户端一致）
const RISK_TIMEOUT_MS: u64 = 25_000;
/// stdout 大小上限（正常输出就一行 JSON，超限视为组件异常）
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// 客户端版本读不到时的兜底（参考 qoder2api-hub 的桌面端活动请求头）
const DEFAULT_CLIENT_VERSION: &str = "0.4.3";

/// 一次生成的设备风控身份三件套
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct RiskIdentity {
    pub machine_token: String,
    pub machine_code: String,
    pub machine_type: String,
    #[serde(default)]
    pub vm_is_vm: Option<bool>,
    #[serde(default)]
    pub vm_brand: Option<String>,
    #[serde(default)]
    pub vm_type_code: Option<i64>,
    #[serde(default)]
    pub vm_percentage: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct PersistedIdentity {
    at: i64,
    identity: RiskIdentity,
}

/// 一个可调用的 UMID 组件
pub(crate) struct Component {
    pub(crate) exe: PathBuf,
    /// 环境编号（国际版 3/4、中国版 0，见模块头）
    pub(crate) env: u32,
    /// 来源标签（日志与状态接口用）：app / cli / data
    pub(crate) source: &'static str,
}

// ─── 组件发现 ───────────────────────────────────────────────

/// 用户主目录（与 `autoclaw::credentials::home_dir` 同口径）
fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn exists(path: &Path) -> bool {
    path.is_file()
}

/// 安装目录下的 resources 候选（Windows）：0.3+ 启动器把实际版本放在
/// `.qoder-versions\<版本>\resources`（新版本在前），顶层 resources 可能
/// 只是首装残留（CreditDaddy `resourceDirsIn` 同款）。
fn windows_resource_dirs(install: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(install.join(".qoder-versions")) {
        let mut versions: Vec<(String, PathBuf)> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.split('.').all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit())))
            })
            .map(|path| {
                let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("").to_string();
                (name, path)
            })
            .collect();
        versions.sort_by(|a, b| b.0.cmp(&a.0));
        out.extend(versions.into_iter().map(|(_, path)| path.join("resources")));
    }
    out.push(install.join("resources"));
    out
}

/// Windows 的 Qoder 客户端安装目录候选（CreditDaddy `candidateResourceDirs`）
fn windows_install_dirs(install_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        out.push(PathBuf::from(&local).join("Programs").join(install_name));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        out.push(PathBuf::from(&program_files).join(install_name));
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        out.push(PathBuf::from(&program_files_x86).join(install_name));
    }
    out
}

/// 在一个 resources 目录里找 UMID 组件
fn runtime_info_in(resources: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "runtime-info.exe" } else { "runtime-info" };
    let path = resources.join("umid").join(exe);
    exists(&path).then_some(path)
}

/// 跨地区回落开关：允许拿另一地区客户端的 UMID 组件生成本地区身份。
///
/// **默认关闭**，与参考实现（`mmqz/cpa-multi-plugins` 严格按地区取组件）对齐。
/// 需要如实记住的是：本机实测「用 `.qoder-cn` 的组件 + 国际版 env=3」与
/// 「用 `.qoder` 的组件 + env=3」产出的三件套**逐字节相同**，所以“跨区产物
/// 带错地区指纹”在这台机器上并没有被复现 —— 这条收紧是防御性对齐，不是在修
/// 一个已观测到的缺陷。代价是：只装了另一地区客户端的用户会拿不到身份，
/// 那种情况用 `AGENT2API_QODER_UMID_CROSS_REGION=1` 恢复旧行为。
fn cross_region_fallback_enabled() -> bool {
    std::env::var("AGENT2API_QODER_UMID_CROSS_REGION")
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// 客户端（app 来源）的组件：默认只认本 region 的安装目录。
///
/// 旧行为是「先找本 variant，找不到就回落另一 variant」（CreditDaddy
/// `apps.find || apps[0]`）。收紧成严格同地区的依据与代价见
/// `cross_region_fallback_enabled`。
fn app_component(region: Region) -> Option<Component> {
    let env = if region == Region::Global { 3 } else { 0 };
    // (installName, macOS app 名)
    let own: (&str, &str) = if region == Region::Global { ("Qoder", "Qoder") } else { ("Qoder CN", "Qoder CN") };
    let other: (&str, &str) = if region == Region::Global { ("Qoder CN", "Qoder CN") } else { ("Qoder", "Qoder") };
    let mut variants: Vec<(&str, &str)> = vec![own];
    if cross_region_fallback_enabled() {
        variants.push(other);
    }
    for (install_name, mac_name) in variants {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if cfg!(windows) {
            for install in windows_install_dirs(install_name) {
                candidates.extend(windows_resource_dirs(&install));
            }
        } else if cfg!(target_os = "macos") {
            let home = home_dir();
            candidates.push(PathBuf::from("/Applications").join(format!("{mac_name}.app")).join("Contents/Resources"));
            candidates.push(home.join("Applications").join(format!("{mac_name}.app")).join("Contents/Resources"));
        } else {
            let slug = install_name.to_lowercase().replace(' ', "-");
            candidates.push(PathBuf::from("/opt").join(install_name).join("resources"));
            candidates.push(PathBuf::from("/opt").join(&slug).join("resources"));
            candidates.push(PathBuf::from("/usr/lib").join(&slug).join("resources"));
            candidates.push(PathBuf::from("/snap").join(&slug).join("current/resources"));
        }
        if let Some(exe) = candidates.iter().find_map(|resources| runtime_info_in(resources)) {
            return Some(Component { exe, env, source: "app" });
        }
    }
    None
}

/// qodercli 解压出的组件（cli 来源）：`~/.qoder/.bin/umid-*/runtime-info`
/// （国内版 `~/.qoder-cn`）。默认只认本 region 的 home，理由见
/// `cross_region_fallback_enabled`；`AGENT2API_QODER_UMID_CROSS_REGION=1` 时
/// 恢复「缺失回落另一 variant」。
/// env 用 qodercli 的取值：国际版 4（SINGAPORE）/ 中国版 0。
///
/// 注意布局与客户端不同：qodercli 把 runtime-info 直接解在 `umid-<平台>-<哈希>/`
/// 目录下（旁边还有 sgsdk.dll），**不再套一层 `umid/`** —— 客户端那层目录来自
/// `resources/umid/`，两边不能共用同一个查找函数。
fn cli_component(region: Region) -> Option<Component> {
    let env = if region == Region::Global { 4 } else { 0 };
    let (own, other) = if region == Region::Global { (".qoder", ".qoder-cn") } else { (".qoder-cn", ".qoder") };
    let mut homes: Vec<&str> = vec![own];
    if cross_region_fallback_enabled() {
        homes.push(other);
    }
    let exe_name = if cfg!(windows) { "runtime-info.exe" } else { "runtime-info" };
    let home = home_dir();
    for dir in homes {
        let bin = home.join(dir).join(".bin");
        let Ok(entries) = std::fs::read_dir(&bin) else { continue };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("umid-"))
            })
            .collect();
        dirs.sort();
        if let Some(exe) = dirs.into_iter().map(|dir| dir.join(exe_name)).find(|path| exists(path)) {
            return Some(Component { exe, env, source: "cli" });
        }
    }
    None
}

/// 网关数据目录里安装的组件（data 来源，Linux/Docker 的
/// `install_component` 落地路径）。env 与 cli 来源保持一致：这里以前写死 4
/// （SINGAPORE），国内版账号会拿到国际版的 env 编号，和上面两个来源的口径不一致，
/// 所以改成按 region 取。实测该 env 不影响三件套的值（见
/// `cross_region_fallback_enabled`），这里属于口径统一。
fn data_component(region: Region) -> Option<Component> {
    let env = if region == Region::Global { 4 } else { 0 };
    let exe = crate::server::config::config_dir().join("qoder-umid").join("runtime-info");
    exists(&exe).then(|| Component { exe, env, source: "data" })
}

/// 组件发现（按 app → cli → data 的顺序，同 variant 优先）。
pub(crate) fn find_component(region: Region) -> Option<Component> {
    app_component(region).or_else(|| cli_component(region)).or_else(|| data_component(region))
}

/// 本机是否能生成风控身份（界面提示用）
pub fn available(region: Region) -> bool {
    find_component(region).is_some()
}

// ─── 身份的辅助字段（请求头用）──────────────────────────────

/// `Cosy-MachineOS`：活动接口按官方桌面端画像校验，固定使用 x86_64_win32。
pub fn machine_os() -> String {
    "x86_64_win32".to_string()
}

/// `Cosy-MachineHostname`：可打印 ASCII，超长截断并附短哈希
/// （CreditDaddy `machineHostname` 同款清洗）。
pub fn machine_hostname() -> Option<String> {
    let raw = hostname();
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let short_hash = |text: &str| {
        let digest = sha2::Sha256::digest(text.as_bytes());
        digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect::<String>()
    };
    let clip = |text: &str| -> String {
        if text.chars().count() <= 96 {
            return text.to_string();
        }
        let hash = short_hash(text);
        let head: String = text.chars().take(96 - 8 - 1).collect();
        let head = head.trim_end_matches(['-', ' ']).to_string();
        if head.is_empty() { format!("unknown-{hash}") } else { format!("{head}-{hash}") }
    };
    let printable = raw.chars().all(|ch| ('\x21'..='\x7e').contains(&ch) || ch == ' ')
        && raw.starts_with(|ch: char| ('\x21'..='\x7e').contains(&ch))
        && raw.ends_with(|ch: char| ('\x21'..='\x7e').contains(&ch));
    if printable {
        return Some(clip(raw));
    }
    let hash = short_hash(raw);
    let cleaned: String = raw
        .chars()
        .map(|ch| if ('\x21'..='\x7e').contains(&ch) { ch } else { '-' })
        .collect();
    let cleaned = cleaned.split('-').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-");
    Some(clip(&if cleaned.is_empty() { format!("unknown-{hash}") } else { format!("{cleaned}-{hash}") }))
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

/// `Cosy-Version`：客户端版本（从已发现客户端的 build-manifest.json 读；
/// 只有 cli / data 组件时读不到，回落常量 —— 上游只拿它做画像参考）。
fn client_version(region: Region) -> String {
    let versions = if region == Region::Global {
        ["Qoder", "Qoder CN"]
    } else {
        ["Qoder CN", "Qoder"]
    };
    for name in versions {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if cfg!(windows) {
            candidates.extend(windows_install_dirs(name).iter().flat_map(|install| windows_resource_dirs(install)));
        } else if cfg!(target_os = "macos") {
            candidates.push(PathBuf::from("/Applications").join(format!("{name}.app")).join("Contents/Resources"));
        } else {
            candidates.push(PathBuf::from("/opt").join(name).join("resources"));
        }
        for resources in candidates {
            let manifest = resources.join("build-manifest.json");
            let Ok(text) = std::fs::read_to_string(&manifest) else { continue };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            if let Some(version) = value.get("productVersion").and_then(|value| value.as_str()) {
                let version = version.trim();
                if !version.is_empty() {
                    return version.to_string();
                }
            }
        }
    }
    DEFAULT_CLIENT_VERSION.to_string()
}

// ─── 组件调用 ───────────────────────────────────────────────

/// 跑一次 runtime-info，解析首行 JSON。
///
/// Windows / macOS 带 `--account-stdin` 并把 `{"account":"<uid>"}` 写进 stdin
/// （与客户端一致）；Linux 只传 env。stdout 限制 1 MiB，整段调用限时 25 秒。
async fn run_component(component: &Component, uid: &str) -> Result<RiskIdentity, String> {
    use tokio::io::AsyncWriteExt;

    let mut command = tokio::process::Command::new(&component.exe);
    let with_stdin = cfg!(windows) || cfg!(target_os = "macos");
    if with_stdin {
        command.args([component.env.to_string(), "--account-stdin".to_string()]);
    } else {
        command.arg(component.env.to_string());
    }
    command.stdin(if with_stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() });
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::null());
    // Windows 上隐藏控制台窗口（桌面端用户不该看到一个闪过的黑框）
    #[cfg(windows)]
    {
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — tokio Command 同名扩展方法
    }

    let mut child = command.spawn().map_err(|error| format!("UMID 组件启动失败：{error}"))?;
    if with_stdin {
        if let Some(mut stdin) = child.stdin.take() {
            let payload = format!("{}\n", serde_json::json!({ "account": uid }));
            let _ = stdin.write_all(payload.as_bytes()).await;
            let _ = stdin.flush().await;
            // drop 关闭 stdin，组件读到 EOF 才会输出
        }
    }
    let output = tokio::time::timeout(Duration::from_millis(RISK_TIMEOUT_MS), child.wait_with_output())
        .await
        .map_err(|_| format!("UMID 组件调用超时（{RISK_TIMEOUT_MS} 秒）"))?
        .map_err(|error| format!("UMID 组件执行失败：{error}"))?;
    if output.stdout.len() > MAX_OUTPUT_BYTES {
        return Err("UMID 组件输出过大".to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("").trim();
    let parsed: serde_json::Value =
        serde_json::from_str(line).map_err(|_| format!("UMID 组件输出不是 JSON：{}", truncate(line, 120)))?;
    let pick = |key: &str| -> Result<String, String> {
        let value = parsed.get(key).and_then(|value| value.as_str()).unwrap_or("").trim().to_string();
        if value.is_empty() || value.len() > 4096 {
            return Err(format!("UMID 组件输出缺少 {key}"));
        }
        Ok(value)
    };
    let vm_info = parsed.get("vmInfo").and_then(serde_json::Value::as_object);
    let vm_brand = vm_info
        .and_then(|info| info.get("brand"))
        .and_then(serde_json::Value::as_str)
        .map(|value| truncate(value.trim(), 64))
        .filter(|value| !value.is_empty());
    let vm_type_code = vm_info
        .and_then(|info| info.get("vmTypeCode"))
        .and_then(serde_json::Value::as_i64);
    let vm_percentage = vm_info
        .and_then(|info| info.get("percentage"))
        .and_then(serde_json::Value::as_i64);
    let vm_is_vm = vm_info
        .and_then(|info| info.get("isVm"))
        .and_then(serde_json::Value::as_bool);
    Ok(RiskIdentity {
        machine_token: pick("machineToken")?,
        machine_code: pick("machineCode")?,
        machine_type: pick("machineType")?,
        vm_is_vm,
        vm_brand,
        vm_type_code,
        vm_percentage,
    })
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

// ─── 缓存与对外入口 ─────────────────────────────────────────

struct CacheEntry {
    value: RiskIdentity,
    at: i64,
}

static CACHE: Mutex<Option<HashMap<String, CacheEntry>>> = Mutex::new(None);

/// 缓存键 = 地区本身。
///
/// 桥产出的三件套是**机器级**的：同一台机器上不同账号的 `machineToken` /
/// `machineCode` / `machineType` 完全相同（本机实测三个国际版账号的持久化文件
/// 内容逐字节一致）。按账号做键只会在桥缓存失效、某个账号先刷新时让同一台机器
/// 出现两份身份 —— 即“身份漂移”，而上游按机器去重，漂移会让部分账号突然领不到
/// 设备定向活动。所以按地区共用一份。
fn identity_cache_key(region: Region) -> String {
    region.id().to_string()
}

/// 设备级身份落盘路径：每地区一个文件，所有账号共用。
fn persisted_identity_path(region: Region) -> PathBuf {
    crate::server::config::config_dir().join("qoder-umid").join(format!("machine-{}.json", region.id()))
}

/// 旧版按账号落盘的文件（`identity-<region>-<账号哈希>.json`）。
///
/// 只为迁移用：首次升级时把已有的身份认下来，避免因为换缓存口径让同一台机器
/// “换了一台设备”。注意 `identity-<region>.json`（无账号后缀）是更早版本留下的
/// 另一份值，**不在**迁移范围内 —— 它可能早于当前硬件，认它会真的换身份。
fn legacy_identity_paths(dir: &Path, region: Region) -> Vec<PathBuf> {
    let prefix = format!("identity-{}-", region.id());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"))
        })
        .collect()
}

fn read_persisted(path: &Path, now: i64) -> Option<PersistedIdentity> {
    let text = std::fs::read_to_string(path).ok()?;
    let saved = serde_json::from_str::<PersistedIdentity>(&text).ok()?;
    (now.saturating_sub(saved.at) < RISK_TTL_MS).then_some(saved)
}

/// 从旧版按账号文件里挑最新的、仍在有效期内的那份身份。
fn adopt_legacy_identity(dir: &Path, region: Region, now: i64) -> Option<PersistedIdentity> {
    legacy_identity_paths(dir, region)
        .into_iter()
        .filter_map(|path| read_persisted(&path, now))
        .max_by_key(|saved| saved.at)
}

fn load_persisted_identity(region: Region, now: i64) -> Option<RiskIdentity> {
    if let Some(saved) = read_persisted(&persisted_identity_path(region), now) {
        return Some(saved.identity);
    }
    // 迁移：认下最新的一份旧按账号身份，并写成设备级文件。
    let dir = crate::server::config::config_dir().join("qoder-umid");
    let adopted = adopt_legacy_identity(&dir, region, now)?;
    persist_identity(region, &adopted.identity, adopted.at);
    logging::verbose(
        "[Qoder]",
        &format!("设备风控身份迁移到设备级缓存（地区={}，{}）", region.label(), identity_summary(&adopted.identity)),
    );
    Some(adopted.identity)
}

fn persist_identity(region: Region, identity: &RiskIdentity, at: i64) {
    let path = persisted_identity_path(region);
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp = parent.join(format!("machine-{}.{}.tmp", region.id(), std::process::id()));
    let saved = PersistedIdentity { at, identity: identity.clone() };
    let Ok(text) = serde_json::to_vec_pretty(&saved) else { return };
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
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::rename(temp, path);
}

fn majority_identity(samples: &[RiskIdentity]) -> Option<RiskIdentity> {
    let mut groups: Vec<(RiskIdentity, usize)> = Vec::new();
    for sample in samples {
        if let Some((_, count)) = groups.iter_mut().find(|(identity, _)| identity == sample) {
            *count += 1;
        } else {
            groups.push((sample.clone(), 1));
        }
    }
    groups.into_iter().max_by_key(|(_, count)| *count).map(|(identity, _)| identity)
}

fn identity_summary(identity: &RiskIdentity) -> String {
    format!(
        "vm={} brand={} type={} score={}",
        identity.vm_is_vm.map(|value| value.to_string()).unwrap_or_else(|| "unknown".to_string()),
        identity.vm_brand.as_deref().unwrap_or("unknown"),
        identity.vm_type_code.map(|value| value.to_string()).unwrap_or_else(|| "unknown".to_string()),
        identity.vm_percentage.map(|value| value.to_string()).unwrap_or_else(|| "unknown".to_string()),
    )
}

/// 取某账号的风控身份（带 50 分钟缓存）。组件不可用或生成失败返回 None，
/// 失败原因只进 verbose 日志 —— 签到链路会给出「缺组件」的用户文案。
pub async fn identity(region: Region, uid: &str) -> Option<RiskIdentity> {
    if uid.is_empty() {
        return None;
    }
    let key = identity_cache_key(region);
    let now = logging::now_ms();
    if let Ok(guard) = CACHE.lock() {
        if let Some(entry) = guard.as_ref().and_then(|map| map.get(&key)) {
            if now - entry.at < RISK_TTL_MS {
                return Some(entry.value.clone());
            }
        }
    }
    if let Some(value) = load_persisted_identity(region, now) {
        if let Ok(mut guard) = CACHE.lock() {
            guard.get_or_insert_with(HashMap::new).insert(key, CacheEntry { value: value.clone(), at: now });
        }
        logging::verbose(
            "[Qoder]",
            &format!("复用持久化设备风控身份（地区={}，{}）", region.label(), identity_summary(&value)),
        );
        return Some(value);
    }
    let component = find_component(region)?;
    let mut samples = Vec::new();
    let mut last_error = None;
    for _ in 0..3 {
        match run_component(&component, uid).await {
            Ok(value) => samples.push(value),
            Err(error) => last_error = Some(error),
        }
    }
    let Some(value) = majority_identity(&samples) else {
        logging::verbose(
            "[Qoder]",
            &format!("设备风控身份生成失败：{}", last_error.unwrap_or_else(|| "没有有效采样".to_string())),
        );
        return None;
    };
    persist_identity(region, &value, now);
    if let Ok(mut guard) = CACHE.lock() {
        guard.get_or_insert_with(HashMap::new).insert(key, CacheEntry { value: value.clone(), at: now });
    }
    logging::verbose(
        "[Qoder]",
        &format!("设备风控身份已生成（来源 {}，采样={}，{}）", component.source, samples.len(), identity_summary(&value)),
    );
    Some(value)
}

/// 签到头增强：在基础头（`sash_headers`）上叠加客户端身份与设备风控身份。
///
/// 返回 `(增强后的头, 风控不可用的原因)` —— 原因只用于日志与签到结果文案，
/// 头里缺风控三件套时上游不下发积分活动（这就是它的表现，不抛错）。
pub async fn augment_headers(
    mut headers: Vec<(String, String)>,
    region: Region,
    uid: &str,
) -> (Vec<(String, String)>, Option<String>) {
    headers.push(("Cosy-Version".to_string(), client_version(region)));

    // 实验开关：按账号合成整套身份（默认关闭）。打开后不再依赖本机 UMID 组件，
    // 每个账号呈现一份稳定但**非本机真实**的设备身份。开关的判定依据与风险见
    // `machine::synthetic_identity_enabled` 的说明 —— 必须先跑对照实验确认
    // 合成身份能另开一份设备名额，否则只会得到「列表里没有可领活动」。
    if machine::synthetic_identity_enabled() && !uid.trim().is_empty() {
        headers.push(("Cosy-MachineOS".to_string(), machine_os()));
        if let Ok(id) = machine::device_id(region, uid) {
            headers.push(("Cosy-MachineId".to_string(), id));
        }
        if let Some(host) = machine_hostname() {
            headers.push(("Cosy-MachineHostname".to_string(), host));
        }
        let (token, code, kind) = machine::synthetic_identity(region, uid);
        headers.push(("Cosy-MachineToken".to_string(), token));
        headers.push(("Cosy-MachineCode".to_string(), code));
        headers.push(("Cosy-MachineType".to_string(), kind));
        logging::verbose("[Qoder]", &format!("使用按账号合成的设备身份（地区={}，账号={}）", region.label(), &uid[..uid.len().min(8)]));
        return (headers, None);
    }

    match identity(region, uid).await {
        Some(risk) => {
            // 参考 qoder2api-hub：只有官方 UMID 生成的真实身份可用时才发
            // cosy-machine* 头。缺少真实身份时发送半套/派生头会让上游静默
            // 过滤可领取活动，表现为 HTTP 200 但列表里没有 CLAIM_BENEFIT。
            headers.push(("Cosy-MachineOS".to_string(), machine_os()));
            // 设备标识按**账号**绑定（`machine::device_id`：一次分配、落盘持久、
            // 永不轮换）。同一台机器上的多个账号因此各自呈现一台稳定设备，
            // 而不是共用同一个 id 被上游当成同一台设备（国际版每设备每日只下发
            // 一个账号的积分活动）。拿不到账号身份时退回设备级 machine_id。
            if let Ok(id) = machine::device_id(region, uid) {
                headers.push(("Cosy-MachineId".to_string(), id));
            }
            if let Some(host) = machine_hostname() {
                headers.push(("Cosy-MachineHostname".to_string(), host));
            }
            headers.push(("Cosy-MachineToken".to_string(), risk.machine_token));
            headers.push(("Cosy-MachineCode".to_string(), risk.machine_code));
            headers.push(("Cosy-MachineType".to_string(), risk.machine_type));
            (headers, None)
        }
        None => {
            let why = if find_component(region).is_none() {
                "本机没有 Qoder 客户端 / qodercli 的 UMID 组件".to_string()
            } else {
                "UMID 组件生成身份失败".to_string()
            };
            (headers, Some(why))
        }
    }
}

// ─── 状态与安装（Linux/Docker）──────────────────────────────

/// 组件状态摘要（状态接口用）：来源 / 是否可用 / 平台是否支持安装。
pub fn status(region: Region) -> serde_json::Value {
    let component = find_component(region);
    serde_json::json!({
        "available": component.is_some(),
        "source": component.as_ref().map(|component| component.source),
        "installSupported": install_supported(),
        "installing": installing(),
    })
}

static INSTALLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn installing() -> bool {
    INSTALLING.load(std::sync::atomic::Ordering::Relaxed)
}

/// 安装只支持 Linux：npm 包里内嵌的是 Linux ELF（CreditDaddy 的结论），
/// Windows / macOS 用户有客户端与 qodercli 两条现成路径，不需要下载。
pub fn install_supported() -> bool {
    cfg!(target_os = "linux")
}

/// 从官方 qodercli 的 npm 包提取本机架构的 UMID 组件。
///
/// 照抄 CreditDaddy `qoderUmid.js` 的链路：npmmirror 优先加速、npmjs 是
/// integrity 的**权威来源**（镜像被污染时它自己的校验形同虚设）；tar 解析
/// 手写（按名字取一个普通文件）；ELF 按 `e_machine` 挑本机架构（x64=62 /
/// arm64=183）；装好先试跑一次确认能出身份，再原子落地。
pub async fn install_component() -> Result<serde_json::Value, GatewayError> {
    if !install_supported() {
        return Err(GatewayError::with_status(400, "当前平台不需要或不支持下载安装 UMID 组件（Windows / macOS 请安装 Qoder 客户端或 qodercli）"));
    }
    if INSTALLING.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return Err(GatewayError::with_status(409, "UMID 组件正在安装中，请稍候"));
    }
    let result = install_component_inner().await;
    INSTALLING.store(false, std::sync::atomic::Ordering::Relaxed);
    result
}

const NPM_PACKAGE: &str = "@qoder-ai/qodercli";
const NPM_PACKAGE_ESCAPED: &str = "@qoder-ai%2fqodercli";
const NPM_MIRRORS: [&str; 2] = ["https://registry.npmmirror.com", "https://registry.npmjs.org"];
const NPM_AUTHORITY: &str = "https://registry.npmjs.org";
const BUNDLE_ENTRY: &str = "package/bundle/qodercli.js";
const DOWNLOAD_TIMEOUT_MS: u64 = 180_000;
const META_TIMEOUT_MS: u64 = 15_000;

async fn install_component_inner() -> Result<serde_json::Value, GatewayError> {
    let fail = |message: String| GatewayError::with_status(502, message);

    // ① 权威 integrity（npmjs；拿不到时退回镜像自带的，与旧行为一致）
    let authority = npm_meta(NPM_AUTHORITY).await.ok();

    // ② 逐个 registry 下载
    let mut errors: Vec<String> = Vec::new();
    for registry in NPM_MIRRORS {
        let meta = match npm_meta(registry).await {
            Ok(meta) => meta,
            Err(error) => {
                errors.push(format!("{registry}：{error}"));
                continue;
            }
        };
        let version = meta.version.clone().unwrap_or_default();
        let Some((tarball, integrity)) = meta.dist else {
            errors.push(format!("{registry}：元数据缺少 tarball / integrity"));
            continue;
        };
        // 镜像版本与 npmjs 不一致时没有权威校验值可比，跳过镜像直接用官方源
        if registry != NPM_AUTHORITY {
            if let Some(authority) = authority.as_ref() {
                let authority_version = authority.version.as_deref().unwrap_or("");
                if !authority_version.is_empty() && authority_version != version {
                    errors.push(format!("{registry}：版本（{version}）与 npmjs（{authority_version}）不一致"));
                    continue;
                }
                if let Some((_, authority_integrity)) = &authority.dist {
                    if &integrity != authority_integrity {
                        errors.push(format!("{registry}：integrity 与 npmjs 不一致"));
                        continue;
                    }
                }
            }
        }
        logging::log("[Qoder]", &format!("正在下载 {NPM_PACKAGE}@{version}（{registry}）"));
        let body = match http_get(&tarball, DOWNLOAD_TIMEOUT_MS).await {
            Ok(body) => body,
            Err(error) => {
                errors.push(format!("{registry}：下载失败 {error}"));
                continue;
            }
        };
        if let Some(expected) = strip_integrity_prefix(&integrity) {
            let digest = sha2::Sha512::digest(&body);
            let actual = base64::engine::general_purpose::STANDARD.encode(digest);
            if actual != expected {
                errors.push(format!("{registry}：下载内容与 npm integrity 不一致"));
                continue;
            }
        }
        // ③ gunzip + tar：取 bundle/qodercli.js
        let tarball = match flate2::read::GzDecoder::new(&body[..]).bytes_to_vec() {
            Ok(tar) => tar,
            Err(error) => {
                errors.push(format!("{registry}：解压失败 {error}"));
                continue;
            }
        };
        let Some(bundle) = tar_entry(&tarball, BUNDLE_ENTRY) else {
            errors.push(format!("{registry}：包里没有 {BUNDLE_ENTRY}（qodercli 结构可能变化）"));
            continue;
        };
        // ④ 挑本机架构的内嵌 ELF
        let machine = if cfg!(target_arch = "x86_64") { 62u16 } else if cfg!(target_arch = "aarch64") { 183u16 } else { 0 };
        if machine == 0 {
            return Err(fail("当前 CPU 架构不受支持".to_string()));
        }
        let bundle = String::from_utf8_lossy(&bundle).to_string();
        let Some(elf) = extract_elf(&bundle, machine) else {
            errors.push(format!("{registry}：qodercli {version} 里没有本机架构的 UMID 程序"));
            continue;
        };
        // ⑤ 落地 + 试跑 + 原子替换
        let dir = crate::server::config::config_dir().join("qoder-umid");
        if let Err(error) = std::fs::create_dir_all(&dir) {
            return Err(fail(format!("创建组件目录失败: {error}")));
        }
        let final_path = dir.join("runtime-info");
        let tmp_path = dir.join(format!("runtime-info.{}.tmp", std::process::id()));
        if let Err(error) = std::fs::write(&tmp_path, &elf) {
            return Err(fail(format!("写入组件失败: {error}")));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755));
        }
        let component = Component { exe: tmp_path.clone(), env: 4, source: "data" };
        if let Err(error) = run_component(&component, "install-test").await {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(fail(format!("组件试运行失败：{error}")));
        }
        if let Err(error) = std::fs::rename(&tmp_path, &final_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(fail(format!("组件落地失败: {error}")));
        }
        let manifest = serde_json::json!({
            "version": version,
            "registry": registry,
            "installedAt": logging::now_ms(),
        });
        let _ = std::fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(&manifest).unwrap_or_default());
        logging::log("[Qoder]", &format!("✅ 设备风控组件已安装：qodercli {version}"));
        return Ok(manifest);
    }
    Err(fail(format!("设备身份组件安装失败 —— {}", errors.join("；"))))
}

struct NpmMeta {
    version: Option<String>,
    /// (tarball, integrity)
    dist: Option<(String, String)>,
}

async fn npm_meta(registry: &str) -> Result<NpmMeta, String> {
    let url = format!("{registry}/{NPM_PACKAGE_ESCAPED}/latest");
    let body = http_get(&url, META_TIMEOUT_MS).await?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|error| format!("元数据不是 JSON: {error}"))?;
    Ok(NpmMeta {
        version: value.get("version").and_then(|value| value.as_str()).map(str::to_string),
        dist: value.get("dist").and_then(|dist| {
            let tarball = dist.get("tarball").and_then(|value| value.as_str())?.trim().to_string();
            let integrity = dist.get("integrity").and_then(|value| value.as_str())?.trim().to_string();
            (!tarball.is_empty() && !integrity.is_empty()).then_some((tarball, integrity))
        }),
    })
}

async fn http_get(url: &str, timeout_ms: u64) -> Result<Vec<u8>, String> {
    let client = crate::server::core::egress::client_for(None);
    let response = client
        .get(url)
        .timeout(Duration::from_millis(timeout_ms))
        .send()
        .await
        .map_err(|error| crate::server::core::egress::describe_error_detail(&error))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| crate::server::core::egress::describe_error_detail(&error))
}

/// `sha512-<base64>` → base64 部分（前缀不匹配返回 None = 不校验）
fn strip_integrity_prefix(integrity: &str) -> Option<String> {
    integrity.strip_prefix("sha512-").map(str::to_string).filter(|value| !value.is_empty())
}

/// GzDecoder 的小包装（`bytes_to_vec` 不存在，读全量兜个底）
trait BytesToVec {
    fn bytes_to_vec(self) -> Result<Vec<u8>, String>;
}

impl<R: std::io::Read> BytesToVec for flate2::read::GzDecoder<R> {
    fn bytes_to_vec(mut self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut self, &mut out).map_err(|error| error.to_string())?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{identity_cache_key, majority_identity, persisted_identity_path, RiskIdentity};
    use crate::server::logging;

    fn identity(brand: &str, vm_type_code: i64) -> RiskIdentity {
        RiskIdentity {
            machine_token: "token".to_string(),
            machine_code: format!("code-{brand}"),
            machine_type: format!("type-{brand}"),
            vm_is_vm: Some(true),
            vm_brand: Some(brand.to_string()),
            vm_type_code: Some(vm_type_code),
            vm_percentage: Some(80),
        }
    }

    #[test]
    fn majority_identity_keeps_complete_sample() {
        let docker = identity("Docker", 50);
        let kvm = identity("KVM", 13);
        let selected = majority_identity(&[docker.clone(), kvm, docker.clone()]);
        assert_eq!(selected, Some(docker));
    }

    #[test]
    fn risk_identity_deserializes_without_optional_vm_fields() {
        let value: RiskIdentity = serde_json::from_str(
            r#"{"machine_token":"token","machine_code":"code","machine_type":"type"}"#,
        )
        .expect("legacy identity should deserialize");
        assert_eq!(value.vm_is_vm, None);
        assert_eq!(value.vm_brand, None);
    }

    #[test]
    fn risk_identity_cache_keyed_by_region_only() {
        // 桥产出的身份是机器级的：同地区不同账号必须命中同一份缓存，
        // 否则桥缓存失效时会让同一台机器出现两份身份。
        assert_eq!(identity_cache_key(super::Region::Cn), identity_cache_key(super::Region::Cn));
        assert_ne!(identity_cache_key(super::Region::Cn), identity_cache_key(super::Region::Global));
        assert!(persisted_identity_path(super::Region::Cn)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("machine-cn") && name.ends_with(".json")));
    }

    #[test]
    fn legacy_per_account_identity_is_adopted_not_rotated() {
        // 换缓存口径不能让同一台机器"换一台设备"：升级后必须认下旧的按账号身份。
        let dir = std::env::temp_dir().join(format!("qoder-risk-adopt-{}-{}", std::process::id(), logging::now_ms()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let now = logging::now_ms();
        let cn = super::Region::Cn;
        let write = |name: &str, at: i64, code: &str| {
            let saved = super::PersistedIdentity { at, identity: identity(code, 50) };
            std::fs::write(dir.join(name), serde_json::to_vec(&saved).unwrap()).unwrap();
        };
        // 更早版本留下的无后缀文件：不在迁移范围
        write("identity-cn.json", now, "unstamped");
        // 旧按账号文件：较新的一份应被选中
        write("identity-cn-aaaaaaaaaaaa.json", now - 1_000, "older-account");
        write("identity-cn-bbbbbbbbbbbb.json", now, "newest-account");
        // 过期的一份即使更晚也不该被认下（有效期语义与线上一致）
        write("identity-cn-cccccccccccc.json", now - super::RISK_TTL_MS - 1_000, "expired");
        // 其它地区不参与
        write("identity-global-dddddddddddd.json", now, "other-region");

        let adopted = super::adopt_legacy_identity(&dir, cn, now).expect("should adopt a legacy identity");
        assert_eq!(adopted.identity.machine_code, "code-newest-account");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 最小 tar 解析：按名字取一个普通文件的内容（CreditDaddy `tarEntry` 同款，
/// 处理 GNU longname 与 pax 扩展头的 path 覆盖）。
fn tar_entry(tar: &[u8], wanted: &str) -> Option<Vec<u8>> {
    let cstr = |bytes: &[u8]| -> String {
        let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).to_string()
    };
    let mut offset = 0usize;
    let mut long_name: Option<String> = None;
    while offset + 512 <= tar.len() {
        let header = &tar[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = usize::from_str_radix(cstr(&header[124..136]).trim(), 8).unwrap_or(0);
        let entry_type = header[156] as char;
        let prefix = cstr(&header[345..500]);
        let mut name = long_name.clone().unwrap_or_else(|| {
            if prefix.is_empty() { cstr(&header[..100]) } else { format!("{prefix}/{}", cstr(&header[..100])) }
        });
        long_name = None;
        let body_start = offset + 512;
        let body_end = body_start.saturating_add(size);
        if body_end > tar.len() {
            break;
        }
        offset = body_start + size.div_ceil(512) * 512;
        match entry_type {
            'L' => {
                name = String::from_utf8_lossy(&tar[body_start..body_end]).trim_end_matches('\0').to_string();
            }
            'x' => {
                let body = String::from_utf8_lossy(&tar[body_start..body_end]);
                if let Some(value) = body.lines().find_map(|line| line.strip_prefix("path=").map(|value| value.trim().to_string())) {
                    long_name = Some(value);
                }
            }
            '0' | '\0' => {
                if name == wanted {
                    return Some(tar[body_start..body_end].to_vec());
                }
            }
            _ => {}
        }
    }
    None
}

/// 在 qodercli bundle 文本里找内嵌的 base64 ELF，按 `e_machine` 挑本机架构
/// （CreditDaddy `extractElf` 同款：`"f0VMRgIB` 是 `\x7fELF` + 64 位 + 小端
/// 的 base64 开头）。
fn extract_elf(bundle: &str, machine: u16) -> Option<Vec<u8>> {
    const MARK: &str = "\"f0VMRgIB";
    let mut from = 0usize;
    while let Some(relative) = bundle[from..].find(MARK) {
        let start = from + relative; // 指向开头的引号
        let Some(end) = bundle[start + 1..].find('"').map(|value| start + 1 + value) else {
            break;
        };
        let encoded = &bundle[start + 1..end];
        if let Ok(head) = base64::engine::general_purpose::STANDARD.decode(&encoded[..encoded.len().min(4096).min(encoded.len())]) {
            if head.len() >= 20 {
                let value = u16::from_le_bytes([head[18], head[19]]);
                if value == machine {
                    return base64::engine::general_purpose::STANDARD.decode(encoded).ok();
                }
            }
        }
        from = end + 1;
    }
    None
}
