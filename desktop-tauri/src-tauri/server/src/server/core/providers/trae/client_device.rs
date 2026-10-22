//! 从本机 Trae 客户端读「这台设备在服务端注册过的设备号」。
//!
//! ── 为什么光有 `credential.device_id` 不够 ──────────────────
//! 登录链（`oauth.rs`）生成的 `device_id` 是**本地随机 16 位数字**，随
//! `DeviceInfo.DeviceID` 报给上游；但签到 `claim` 认的是「客户端在设备服务
//! 里登记过的那台设备」。2026-10-22 的真实账号实测：
//!
//!   * 同一 token、同一套头，`checkin_credits/status` 用随机号回 `code:0`
//!     （`checked_in:false, credits:100`），`claim` 回 `9074`；
//!   * 换成客户端 `storage.json` 里 `iCubeAuthInfo://icube-dc:<号>` 的那串
//!     （15 位），`claim` 立刻 `code:0` —— 当日额度真实到账；
//!   * 这期间试过 4 套 UA / 头形状、`{"req_source":1}` 请求体、直连与代理
//!     两种出口 IP，全部仍是 9074。
//!
//! 所以 9074 的判据是**设备身份**，不是限流、不是 IP、不是请求头。参考实现
//! （magpie `clientDevice` / L0NE-6 `dc_device_id`）也是同一条结论。
//!
//! ── 为什么必须校验 uid ──────────────────────────────────────
//! `storage.json` 是**单账号**的：它只有「当前登录的那个号」的设备号。一个网关
//! 里常有多条 Trae 账号，若不加校验就给所有账号套同一个设备号，等于把 N 个号
//! 挤到同一台设备上 —— 上游按设备每日去重，除第一个之外全部 9095/9074。
//! 因此这里必须解出客户端登录态里的 `userId` 并与账号 `uid` 比对，只认**同号**。
//!
//! ── 解密 ────────────────────────────────────────────────────
//! `iCubeAuthInfo://icube.cloudide` 的值是客户端自己的信封：
//! `6 字节头（选盐） + 32 字节随机 + AES-128-CBC(sha512(sha512(random)+salt))`，
//! 明文前 64 字节是它自己的 sha512。头 `74 63 05 10 00 00` → SALT_AES，
//! `12 39 20 20 02 03` → SALT_AES_PRIVATE（见参考实现 `tokio`/`clientDecode`）。
//! 只读、不写回，也不需要客户端在跑。

use std::path::PathBuf;

use aes::cipher::{BlockCipherDecrypt, KeyInit};
use aes::Aes128;
use sha2::{Digest, Sha512};

const AUTH_KEY: &str = "iCubeAuthInfo://icube.cloudide";
const DC_KEY_PREFIX: &str = "iCubeAuthInfo://icube-dc:";
/// 设备号的最小/最大十进制位数（客户端见过 15/16，留宽一点只做防呆）。
const DEVICE_MIN_DIGITS: usize = 12;
const DEVICE_MAX_DIGITS: usize = 20;

/// 客户端 `storage.json` 里一条可用设备身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientDevice {
    pub device_id: String,
    pub uid: String,
}

/// 找本机客户端 `storage.json` 的候选路径（按优先级）。
///
/// 只列目录名不读内容：`Trae CN` 是国内客户端，`Trae` 是国际版（它的登录态里
/// 没有 `icube-dc` 键，读到也会被判成不匹配）。`APPDATA` 缺失（非 Windows /
/// 精简环境）时返回空表 —— 调用方据此直接走原有派生逻辑。
pub fn storage_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.trim().is_empty() {
            roots.push(PathBuf::from(appdata));
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        let path = PathBuf::from(home).join("AppData").join("Roaming");
        if !roots.contains(&path) {
            roots.push(path);
        }
    }
    if roots.is_empty() {
        if let Ok(home) = std::env::var("HOME") {
            let path = PathBuf::from(home).join(".config");
            roots.push(path);
        }
    }
    let mut out = Vec::new();
    for root in roots {
        for name in ["Trae CN", "TRAE SOLO CN", "TRAE SOLO", "Trae"] {
            let path = root
                .join(name)
                .join("User")
                .join("globalStorage")
                .join("storage.json");
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// 读**属于 `uid`** 的那台客户端设备；没有/不是同一个号时返回 `None`。
///
/// `uid` 为空时同样返回 `None`：没有身份可比对就等于无法证明"这是同一个号"，
/// 那种情况下宁可用账号自己的派生号（行为与改动前一致），也不猜别人的设备。
pub fn device_for_uid(uid: &str) -> Option<ClientDevice> {
    let uid = uid.trim();
    if uid.is_empty() {
        return None;
    }
    for path in storage_candidates() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(storage) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(device_id) = storage_device_id(&storage) else {
            continue;
        };
        let Some(client_uid) = storage_uid(&storage) else {
            continue;
        };
        if client_uid.trim() != uid {
            continue;
        }
        return Some(ClientDevice {
            device_id,
            uid: client_uid,
        });
    }
    None
}

/// `iCubeAuthInfo://icube-dc:<digits>` 的号（键名优先，兼容把号写在值里）。
fn storage_device_id(storage: &serde_json::Value) -> Option<String> {
    let object = storage.as_object()?;
    for (key, value) in object {
        if let Some(rest) = key.strip_prefix(DC_KEY_PREFIX) {
            if is_device_shaped(rest) {
                return Some(rest.to_string());
            }
        }
        if key.trim_end_matches(':') == DC_KEY_PREFIX.trim_end_matches(':') {
            if let Some(text) = value.as_str() {
                if is_device_shaped(text) {
                    return Some(text.trim().to_string());
                }
            }
        }
    }
    None
}

/// 解出客户端登录态里的 `userId`（明文 JSON 与加密信封都认）。
fn storage_uid(storage: &serde_json::Value) -> Option<String> {
    let raw = storage.get(AUTH_KEY)?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    let plaintext = if raw.starts_with('{') {
        raw.to_string()
    } else {
        decode_client_value(raw)?
    };
    let payload: serde_json::Value = serde_json::from_str(&plaintext).ok()?;
    payload
        .get("userId")
        .or_else(|| payload.get("uid"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

/// 客户端信封 → 明文 JSON 文本。
///
/// 失败一律返回 `None`：这是"锦上添花"的读侧，读不懂就不该让签到失败。
fn decode_client_value(base64_text: &str) -> Option<String> {
    use base64::Engine as _;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(base64_text.trim())
        .ok()?;
    if blob.len() <= 102 {
        return None;
    }
    let salt: &[u8; 64] = match &blob[..6] {
        [0x74, 0x63, 0x05, 0x10, 0x00, 0x00] => &SALT_AES,
        [0x12, 0x39, 0x20, 0x20, 0x02, 0x03] => &SALT_AES_PRIVATE,
        _ => return None,
    };
    let random = &blob[6..38];
    let inner = Sha512::digest(random);
    let mut hasher = Sha512::new();
    hasher.update(inner);
    hasher.update(salt);
    let key = hasher.finalize();
    let cipher = Aes128::new_from_slice(&key[..16]).ok()?;
    let mut prev = [0u8; 16];
    prev.copy_from_slice(&key[16..32]);
    let body = &blob[38..];
    if body.is_empty() || body.len() % 16 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len());
    for chunk in body.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let ciphertext = block;
        cipher.decrypt_block((&mut block).into());
        for index in 0..16 {
            out.push(block[index] ^ prev[index]);
        }
        prev = ciphertext;
    }
    // 明文 = 64 字节 sha512 + 原始 JSON；先按 PKCS#7 去填充（旧实现也见过补零）。
    if out.len() < 64 + 2 {
        return None;
    }
    let mut payload = &out[64..];
    let pad = *payload.last()? as usize;
    if (1..=16).contains(&pad) && payload.len() >= pad {
        payload = &payload[..payload.len() - pad];
    } else {
        while payload.last() == Some(&0) {
            payload = &payload[..payload.len() - 1];
        }
    }
    String::from_utf8(payload.to_vec()).ok()
}

/// 客户端下发的设备号是 12~20 位纯数字（`iCubeAuthInfo://icube-dc:<id>`）。
fn is_device_shaped(device_id: &str) -> bool {
    let text = device_id.trim();
    (DEVICE_MIN_DIGITS..=DEVICE_MAX_DIGITS).contains(&text.len())
        && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// 客户端 AES 盐（两套，按 6 字节头选）。
const SALT_A: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73,
    109, 139, 209, 37,
];
const SALT_B: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25,
    181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176,
    200, 235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99,
    85, 33, 12, 125,
];
const SALT_C: [u8; 64] = [
    191, 192, 216, 250, 122, 246, 220, 97, 31, 254, 98, 27, 8, 72, 71, 176, 135, 99, 96, 18,
    127, 101, 203, 104, 211, 102, 191, 125, 37, 72, 150, 156, 51, 229, 121, 35, 17, 153, 141,
    177, 110, 131, 150, 128, 172, 255, 254, 6, 18, 140, 55, 62, 236, 249, 135, 64, 135, 12, 117,
    4, 89, 149, 168, 209,
];
const SALT_D: [u8; 64] = [
    246, 204, 26, 232, 232, 70, 129, 109, 223, 146, 169, 242, 23, 241, 105, 145, 50, 196, 165,
    42, 254, 120, 3, 54, 244, 207, 209, 85, 53, 6, 138, 106, 175, 148, 31, 204, 186, 186, 165,
    182, 87, 142, 49, 10, 39, 110, 26, 154, 86, 56, 173, 125, 18, 64, 198, 225, 99, 99, 83, 82,
    191, 134, 76, 170,
];

/// `SALT_AES = SALT_A xor SALT_B`（const 求值，运行时零成本）。
const SALT_AES: [u8; 64] = xor64(SALT_A, SALT_B);
const SALT_AES_PRIVATE: [u8; 64] = xor64(SALT_C, SALT_D);

const fn xor64(left: [u8; 64], right: [u8; 64]) -> [u8; 64] {
    let mut out = [0u8; 64];
    let mut index = 0;
    while index < 64 {
        out[index] = left[index] ^ right[index];
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal(plaintext: &str, header: [u8; 6], salt: &[u8; 64]) -> String {
        use aes::cipher::BlockCipherEncrypt;
        use base64::Engine as _;
        let random = [7u8; 32];
        let inner = Sha512::digest(random);
        let mut hasher = Sha512::new();
        hasher.update(inner);
        hasher.update(salt);
        let key = hasher.finalize();
        let cipher = Aes128::new_from_slice(&key[..16]).expect("key");
        let mut prev = [0u8; 16];
        prev.copy_from_slice(&key[16..32]);
        let digest = Sha512::digest(plaintext.as_bytes());
        let mut payload = digest.to_vec();
        payload.extend_from_slice(plaintext.as_bytes());
        let pad = 16 - (payload.len() % 16);
        payload.extend(std::iter::repeat(pad as u8).take(pad));
        let mut out = Vec::new();
        for chunk in payload.chunks(16) {
            let mut block = [0u8; 16];
            for index in 0..16 {
                block[index] = chunk[index] ^ prev[index];
            }
            cipher.encrypt_block((&mut block).into());
            out.extend_from_slice(&block);
            // CBC 的链是**密文块**：拿 XOR 后的明文当 prev 会在第二轮起全错。
            prev = block;
        }
        let mut blob = header.to_vec();
        blob.extend_from_slice(&random);
        blob.extend_from_slice(&out);
        base64::engine::general_purpose::STANDARD.encode(blob)
    }

    #[test]
    fn the_client_envelope_decodes_back_to_its_json() {
        let text = r#"{"userId":"762424146795179","token":"t","refreshToken":"r"}"#;
        for (header, salt) in [
            ([0x74, 0x63, 0x05, 0x10, 0x00, 0x00], &SALT_AES),
            ([0x12, 0x39, 0x20, 0x20, 0x02, 0x03], &SALT_AES_PRIVATE),
        ] {
            let sealed = seal(text, header, salt);
            let decoded = decode_client_value(&sealed);
            assert_eq!(Some(text.to_string()), decoded.clone(), "{header:?} got={decoded:?}");
        }
    }

    #[test]
    fn a_plain_json_login_state_is_read_as_is() {
        let storage = serde_json::json!({
            AUTH_KEY: r#"{"userId":"762424146795179"}"#,
            "iCubeAuthInfo://icube-dc:1259359753811465": "",
        });
        assert_eq!(Some("1259359753811465".to_string()), storage_device_id(&storage));
        assert_eq!(Some("762424146795179".to_string()), storage_uid(&storage));
    }

    #[test]
    fn the_device_id_may_also_sit_in_the_value() {
        let storage = serde_json::json!({"iCubeAuthInfo://icube-dc": "1259359753811465"});
        assert_eq!(Some("1259359753811465".to_string()), storage_device_id(&storage));
    }

    #[test]
    fn a_non_numeric_or_short_id_is_not_a_client_device() {
        for bad in ["", "abc", "12345678901", "y3qnlo681y11rn", "123456789012345678901"] {
            let storage = serde_json::json!({format!("{DC_KEY_PREFIX}{bad}"): ""});
            assert_eq!(None, storage_device_id(&storage), "{bad}");
        }
    }

    #[test]
    fn an_unknown_codec_header_is_refused_rather_than_guessed() {
        use base64::Engine as _;
        let blob = base64::engine::general_purpose::STANDARD.encode([0u8; 120]);
        assert_eq!(None, decode_client_value(&blob));
    }

    #[test]
    fn a_missing_uid_never_borrows_another_accounts_device() {
        // 没有 uid 就没有"同一个号"的证据，必须返回 None（调用方回退到派生号）。
        assert_eq!(None, device_for_uid(""));
        assert_eq!(None, device_for_uid("   "));
    }
}
