//! 「更新设置」里的 GitHub 令牌：加密落库、读取与状态。
//!
//! ── 为什么落库前要加密 ─────────────────────────────────────
//! 令牌能提高 GitHub API 的限额（匿名 60 次/小时 → 5000 次/小时），是**有代价的
//! 凭证**：泄露后别人能以你的身份读仓库、触发 CI。配置库（`agent2api.db`）会被
//! 整库导出 / 备份 / 拷机迁移，明文躺在里面等于「拷走一个文件就带走令牌」。
//! 因此这里存的是 AES-256-GCM 密文：
//!   · 密钥：32 字节随机，存**库外**的 `{config_dir}/update-token.key`（首次保存
//!     令牌时生成；Unix 上 0600）。库被单独拿走时没有密钥文件，解不出令牌。
//!   · 密文形态：`enc1:<base64(nonce(12) || 密文+tag)>`，作为配置键 `githubToken`
//!     的值落库（见 config 模块的 KEY_GITHUB_TOKEN）。`enc1` 是版本前缀，
//!     将来换算法（比如换平台原生密钥库）时能识别出旧格式。
//!
//! ── 明文只进内存，不出三处口子 ─────────────────────────────
//!   1. `effective_token`：`github_headers` 拼 Authorization 头时用；
//!   2. `stored_token`：只给「这次保存有没有真的换掉令牌」的判定用（不解密就
//!      无法与已存值逐字比较，重复粘贴会被误当成换凭证）；
//!   3. 保存与清除路径：加密后立刻丢明文，不落任何日志；
//!   4. API **只报「有没有、来自哪」**（`status_json`），不回显本体 ——
//!      「已填写」三个字是界面能拿到的全部（回显 = 白加密）。
//!
//! ── 优先级：界面保存的 > 环境变量 ──────────────────────────
//! 环境变量 `GITHUB_TOKEN` / 旧名 `WORKBUDDY_GITHUB_TOKEN`（原先是唯一来源，
//! Docker 部署仍靠它）作为兜底。与 config 模块的既有约定一致（「配置里的值 >
//! 环境变量」）：在界面上保存是用户的**最后一次明确动作**，应当生效。
//!
//! ── 解密失败怎么办 ────────────────────────────────────────
//! 密钥文件被删 / 手改过 / 拷库没拷密钥时：**不当作「没配」**——配置里明明有
//! 一段密文，静默按匿名跑会让用户以为令牌还在生效。`effective_token` 回落
//! 环境变量并留一条 verbose；`status_json` 把原因带出去，界面上提示重新保存
//! 一次（新密钥 + 新密文，自愈）。

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::server::config;
use crate::server::logging;

/// 密钥文件名（落在 `~/.agent2api`，与库同目录、库之外）
const KEY_FILE: &str = "update-token.key";

/// 密文信封的版本前缀（见模块头「密文形态」）
const ENVELOPE_PREFIX: &str = "enc1:";

/// GCM nonce 长度（96 bit，标准值）
const NONCE_LEN: usize = 12;

/// 令牌长度上限：GitHub 令牌最长 255 左右，再宽就当脏数据处理
const MAX_TOKEN_LEN: usize = 255;

// ─── 密钥（加载 / 首次生成，进程内缓存）─────────────────────

/// 进程级密钥缓存。
///
/// 为什么用 `Mutex<Option<…>>` 而不是 `OnceLock`：密钥要**不存在就现场生成**，
/// 生成失败（目录不可写等）不能把失败缓存成永久状态 —— 下次保存还能重试；
/// 同时首次并发的两个保存要串行（各自生成不同的密钥、互相覆盖文件，
/// 先存的令牌就解不开了）。
static KEY: std::sync::OnceLock<std::sync::Mutex<Option<[u8; 32]>>> = std::sync::OnceLock::new();

/// 取加密密钥（没有就生成一份新的）。所有失败都以中文错误上抛。
fn secret_key() -> Result<[u8; 32], String> {
    let cell = KEY.get_or_init(|| std::sync::Mutex::new(None));
    let Ok(mut guard) = cell.lock() else {
        return Err("密钥锁中毒".to_string());
    };
    if let Some(key) = *guard {
        return Ok(key);
    }
    let key = load_or_create_key()?;
    *guard = Some(key);
    Ok(key)
}

/// 读密钥文件；不存在则生成并写出（Unix 上收紧到 0600）。
fn load_or_create_key() -> Result<[u8; 32], String> {
    let path = config::config_dir().join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => match <[u8; 32]>::try_from(&bytes[..]) {
            Ok(key) => Ok(key),
            Err(_) => Err(format!(
                "密钥文件 {} 长度异常（期望 32 字节，实际 {} 字节）",
                path.display(),
                bytes.len()
            )),
        },
        // 不存在才生成新密钥；其余错误（权限等）如实上抛 —— 覆盖式重建会把
        // 已保存的密文变成永远解不开的死数据
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_key_file(&path),
        Err(error) => Err(format!("读取密钥文件 {} 失败: {error}", path.display())),
    }
}

/// 生成 32 字节随机密钥并写盘
fn create_key_file(path: &std::path::Path) -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|error| format!("系统随机源不可用: {error}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("创建配置目录失败: {error}"))?;
    }
    std::fs::write(path, key).map_err(|error| format!("写入密钥文件 {} 失败: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(key)
}

// ─── 加密 / 解密 ───────────────────────────────────────────

/// 加密明文 → `enc1:<base64(nonce||密文+tag)>`。
///
/// nonce 每次现生成（GCM 的 nonce 重用是灾难性的：同一密钥下两条明文用同一
/// nonce 会互相泄底），随机性来自系统熵源，不用计数器——进程重启后计数没有
/// 持久化，随机 nonce 才是这里站得住的选择。
fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<String, String> {
    use aes_gcm::aead::{consts::U12, Aead as _};
    use aes_gcm::{Aes256Gcm, KeyInit as _, Nonce};

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| "密钥长度不受 AES-256 支持".to_string())?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes).map_err(|error| format!("系统随机源不可用: {error}"))?;
    let nonce = Nonce::<U12>::try_from(nonce_bytes.as_slice())
        .map_err(|_| "生成 nonce 失败".to_string())?;
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| "AES-GCM 加密失败".to_string())?;
    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(format!("{ENVELOPE_PREFIX}{}", BASE64.encode(&blob)))
}

/// 解开一段 `enc1:` 信封 → UTF-8 明文。失败原因都是「能照着做」的提示。
fn decrypt(envelope: &str) -> Result<String, String> {
    use aes_gcm::aead::{consts::U12, Aead as _};
    use aes_gcm::{Aes256Gcm, KeyInit as _, Nonce};

    let Some(encoded) = envelope.trim().strip_prefix(ENVELOPE_PREFIX) else {
        return Err("密文格式不受支持（不是 enc1 信封）".to_string());
    };
    let blob = BASE64
        .decode(encoded)
        .map_err(|_| "密文不是合法 base64".to_string())?;
    // 长度下限：nonce + GCM tag（16）。aes-gcm 的 tag 附在密文尾部，
    // 这里整段交给 decrypt（Postfix 语义，与 autoclaw/crypto.rs 同一套）
    if blob.len() <= NONCE_LEN + 16 {
        return Err("密文长度异常".to_string());
    }
    let key = secret_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| "密钥长度不受 AES-256 支持".to_string())?;
    let nonce = Nonce::<U12>::try_from(&blob[..NONCE_LEN])
        .map_err(|_| "密文 nonce 长度异常".to_string())?;
    let plaintext = cipher
        .decrypt(&nonce, &blob[NONCE_LEN..])
        .map_err(|_| format!("AES-GCM 解密失败（密钥文件 {KEY_FILE} 是否被删除或更换？重存一次令牌即可自愈）"))?;
    String::from_utf8(plaintext).map_err(|_| "解密结果不是合法 UTF-8 文本".to_string())
}

// ─── 读取与写入（对外）────────────────────────────────────

/// 环境变量里的令牌（Docker / 命令行部署的配置口，两个名字都认、空串当没配）
fn env_token() -> Option<String> {
    ["GITHUB_TOKEN", "WORKBUDDY_GITHUB_TOKEN"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

/// 界面保存的那一份令牌（解密后）。未保存 / 解不开 → None。
///
/// 与 `effective_token` 的分工：这条**只看界面保存的那份**（不含环境变量兜底），
/// 供「这次保存有没有真的换掉令牌」的判定使用（api::update 据此决定要不要清
/// 检查冷却 —— 重复粘贴同一个令牌不该算一次更换）。
pub fn stored_token() -> Option<String> {
    let envelope = config::github_token_envelope()?;
    decrypt(&envelope).ok()
}

/// 实际生效的令牌：界面保存的（解密）> 环境变量。
///
/// `github_headers` 每次检查更新都调它：解密失败的回落与日志都在这里
/// 统一处理，调用方拿到 `Option<String>` 就够了。
pub fn effective_token() -> Option<String> {
    if let Some(envelope) = config::github_token_envelope() {
        match decrypt(&envelope) {
            Ok(token) => return Some(token),
            Err(reason) => logging::verbose(
                "[Update]",
                &format!("已保存的 GitHub 令牌解密失败（{reason}），本次跳过使用"),
            ),
        }
    }
    env_token()
}

/// `set_token` 的结果。
///
/// `saved`：是否落盘成功（false 时内存已生效，与 config 各 setter 同一约定）。
/// `changed`：令牌是否**真的发生了替换/清除** —— 调用方（api::update）只有看到
/// true 才清「检查更新」的失败冷却，免得「重复粘贴同一个令牌」被当成换凭证、
/// 变成绕开冷却的口子。
#[derive(Clone, Copy, Debug)]
pub struct TokenUpdate {
    pub saved: bool,
    pub changed: bool,
}

/// 保存 / 清除令牌（`None` = 清除）。
///
/// 校验只做三件事：非空、不超长、字符集限定在「字母数字下划线连字符」——
/// GitHub 令牌都在这个集合里，而令牌要进 HTTP 头，放进任何空白或控制字符
/// 都是请求头注入，必须在入口拦死。
pub fn set_token(token: Option<&str>) -> Result<TokenUpdate, String> {
    let previous = stored_token();
    let Some(raw) = token else {
        // 清除：库里原本存在过密文（哪怕解不开）就算一次变化
        let changed = previous.is_some() || config::github_token_envelope().is_some();
        return Ok(TokenUpdate {
            saved: config::set_github_token_envelope(None),
            changed,
        });
    };
    let token = raw.trim();
    if token.is_empty() {
        return Err("令牌不能为空（要清除请传 null）".to_string());
    }
    if token.len() > MAX_TOKEN_LEN {
        return Err(format!("令牌过长（最多 {MAX_TOKEN_LEN} 字符）"));
    }
    if !token.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
        return Err("令牌只能包含字母、数字、下划线与连字符".to_string());
    }
    // 与已存令牌逐字比较（解不开时存储侧那一份算「非现值」，会正确判为变化）
    let changed = previous.as_deref() != Some(token);
    let key = secret_key()?;
    let envelope = encrypt(&key, token.as_bytes())?;
    Ok(TokenUpdate {
        saved: config::set_github_token_envelope(Some(envelope)),
        changed,
    })
}

/// 令牌状态 → API 的 JSON 形态（**不含本体**，见模块头「三处口子」）。
///
/// `origin`：`stored`（界面保存的）/ `env`（环境变量）/ null（没配）。
/// 已存令牌解不开时 `error` 带原因 —— 界面据此提示「重新保存一次」，
/// 而不是把「配置坏了」显示成「没配置」。
pub fn status_json() -> serde_json::Value {
    if let Some(envelope) = config::github_token_envelope() {
        return match decrypt(&envelope) {
            Ok(_) => serde_json::json!({ "filled": true, "origin": "stored", "error": null }),
            Err(reason) => {
                let env = env_token().is_some();
                let origin = if env { Some("env") } else { None };
                serde_json::json!({
                    "filled": env,
                    "origin": origin,
                    "error": reason,
                })
            }
        };
    }
    if env_token().is_some() {
        serde_json::json!({ "filled": true, "origin": "env", "error": null })
    } else {
        serde_json::json!({ "filled": false, "origin": null, "error": null })
    }
}
