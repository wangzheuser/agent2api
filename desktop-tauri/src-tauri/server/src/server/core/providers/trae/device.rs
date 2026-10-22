//! 登录用的设备密钥对（EC P-256）。
//!
//! ── 为什么要专门生成一对 ──────────────────────────────────
//! 官方客户端每次登录都现生成一套 P-256，把 **SPKI 公钥 PEM** 塞进
//! `ExchangeToken` 的 `DeviceInfo.DevicePublicKey`，服务端把它与该次登录的
//! `DeviceID` / `MachineID` 绑在一起。参考实现注明：**空值会被判成
//! HTTP 401 / 业务码 20405**（`2xxxx` 那族是凭据/设备绑定拒绝，
//! 2026-09-04 实测），所以这不是"可选的元数据"，少了就登不进来。
//!
//! 私钥同样要落盘（`auth.devicePrivateKey`）：参考实现里它**只写不读**，
//! 但重建凭据时必须保住 —— 把它抹掉等于把这台设备的绑定关系弄断，
//! 而那条链上没有任何一处会告诉你为什么突然 401 了。
//!
//! PEM 的行宽按 Go `encoding/pem` 的 64 字符折行，两边产物可以直接对读。

use base64::{engine::general_purpose::STANDARD, Engine as _};
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey};
use p256::pkcs8::DecodePrivateKey;
use p256::ecdsa::signature::Signer;

/// 一对设备密钥（公钥进请求，私钥进凭据）。
#[derive(Clone, Debug)]
pub struct DeviceKeyPair {
    /// `-----BEGIN PUBLIC KEY-----`（SPKI / DER）
    pub public_pem: String,
    /// `-----BEGIN PRIVATE KEY-----`（PKCS#8 / DER）
    pub private_pem: String,
}

/// 生成一套新的设备密钥。
///
/// 标量取自系统随机源；落在 `[1, n-1]` 之外时**重取**（概率约 2^-128，
/// 但"取一次就假定成功"在密码学路径上是不允许的写法）。
pub fn generate_device_key_pair() -> Option<DeviceKeyPair> {
    let mut attempts = 0;
    while attempts < 16 {
        attempts += 1;
        let mut bytes = [0u8; 32];
        if getrandom::getrandom(&mut bytes).is_err() {
            return None;
        }
        let scalar = p256::FieldBytes::from_slice(&bytes);
        let Ok(key) = p256::ecdsa::SigningKey::from_bytes(scalar) else {
            continue; // 标量落在 [1, n-1] 之外，重取
        };
        // 自己拼 PEM：pkcs8 0.10 的 `to_*_pem` 要 `pkcs8/pem` 特性（p256 的 `pem`
        // 只转到 `elliptic-curve/pem`，没打开它），而 Go 侧 `encoding/pem` 的
        // 行宽是写死的 64 字符 + 结尾换行 —— 手拼反而把这份格式钉在代码里，
        // 两边产物字节级可互读。
        let public = pem("PUBLIC KEY", key.verifying_key().to_public_key_der().ok()?.as_bytes());
        let private = pem("PRIVATE KEY", key.to_pkcs8_der().ok()?.as_bytes());
        return Some(DeviceKeyPair { public_pem: public, private_pem: private });
    }
    None
}

/// DER → Go `encoding/pem` 同格式的 PEM（64 字符折行，首尾各带换行）。
fn pem(label: &str, der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut out = String::with_capacity(encoded.len() + 64);
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        // base64 表全是 ASCII，分片不会切出非法 UTF-8 序列。
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

/// 由落盘的私钥 PEM 反推公钥 PEM（SPKI）。
///
/// 续期要带 `DeviceInfo.DevicePublicKey`，而老凭据里可能只存了私钥
/// （或公钥被上一轮"重建"抹掉）。公钥本来就能从私钥推出，没必要让
/// 这种账号重新登录一次 —— 推不出来才算这份凭据坏了。
pub fn public_pem_from_private(private_pem: &str) -> Option<String> {
    let key = p256::ecdsa::SigningKey::from_pkcs8_pem(private_pem).ok()?;
    Some(pem(
        "PUBLIC KEY",
        key.verifying_key().to_public_key_der().ok()?.as_bytes(),
    ))
}

/// 用设备私钥签一段消息，返回 `base64(DER)`。
///
/// 官方的 `DeviceProof.Signature` 就是这个形态（ECDSA-SHA256 + ASN.1 DER，
/// 不是 JWS 的 `r||s` 定长串）。`Signer::sign` 内部按 SHA-256 摘要并用
/// RFC 6979 的确定性 k —— 服务端拿公钥验签，不比对字节，两种 k 都合法。
pub fn sign_der_base64(private_pem: &str, message: &[u8]) -> Option<String> {
    let key = p256::ecdsa::SigningKey::from_pkcs8_pem(private_pem).ok()?;
    let signature: p256::ecdsa::Signature = key.sign(message);
    Some(STANDARD.encode(signature.to_der().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn the_public_key_is_a_spki_pem_the_server_can_read() {
        let pair = generate_device_key_pair().expect("生成应当成功");
        assert!(pair.public_pem.starts_with("-----BEGIN PUBLIC KEY-----\n"));
        assert!(pair.public_pem.trim_end().ends_with("-----END PUBLIC KEY-----"));
        assert!(pair.private_pem.starts_with("-----BEGIN PRIVATE KEY-----\n"), "PKCS#8 而不是 SEC1");
        // P-256 的 SPKI DER 固定 91 字节 → base64 124 字符 → 按 64 折成两行
        // （64 + 60）。公钥 PEM 本身就会折行，所以"只测单行"验不到折行分支。
        let lines: Vec<&str> = pair.public_pem.lines().filter(|line| !line.starts_with("-----")).collect();
        let widths: Vec<usize> = lines.iter().map(|line| line.len()).collect();
        assert_eq!(vec![64usize, 60], widths, "SPKI 应当正好折成 64+60 两行");
        let body: String = lines.concat();
        let der = base64::engine::general_purpose::STANDARD.decode(&body).expect("base64 可解");
        assert_eq!(91, der.len(), "P-256 SPKI DER 长度变了说明编码方式漂了：{}", der.len());
        // prime256v1 = 1.2.840.10045.3.1 → DER 里是 2a 86 48 ce 3d 03 01 07。
        // （先前抄成了"公钥算法 OID + 0x03"，那是把两个相邻的 ASN.1 段拼成一段了。）
        assert!(der.windows(8).any(|window| window == b"\x2a\x86\x48\xce\x3d\x03\x01\x07"), "曲线 OID 应当在里面（prime256v1）");
    }

    #[test]
    fn two_logins_get_two_different_keys() {
        // "每登录一套设备指纹"是官方客户端的行为，复用等于把两次登录并成一台设备。
        let first = generate_device_key_pair().expect("生成应当成功");
        let second = generate_device_key_pair().expect("生成应当成功");
        assert_ne!(first.public_pem, second.public_pem);
        assert_ne!(first.private_pem, second.private_pem);
    }

    #[test]
    fn pem_lines_are_wrapped_at_sixty_four_characters() {
        let pair = generate_device_key_pair().expect("生成应当成功");
        for line in pair.private_pem.lines().filter(|line| !line.starts_with("-----")) {
            assert!(line.len() <= 64, "Go 的 pem 按 64 折行，超了就不是同一份字节：{}", line.len());
        }
        assert!(pair.private_pem.lines().count() > 3, "私钥 DER 足够长，必然出现折行");
    }

    #[test]
    fn the_device_proof_signature_verifies_against_the_paired_public_key() {
        use p256::ecdsa::signature::Verifier;
        use p256::pkcs8::DecodePublicKey;

        let pair = generate_device_key_pair().expect("生成应当成功");
        let message = b"POST\n/trae/api/v3/oauth/ExchangeToken\nono9krqynydwx5\nrt\n1790000000\nnonce";
        let encoded = sign_der_base64(&pair.private_pem, message).expect("签名应当成功");

        // 官方要的是 DER 形态：以 0x30 开头的 ASN.1 SEQUENCE，不是 64 字节的 r||s。
        let der = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .expect("base64 可解");
        assert_eq!(0x30, der[0], "DeviceProof 用的是 DER 编码");
        assert!(der.len() > 64 && der.len() <= 72, "P-256 DER 签名长度异常：{}", der.len());

        let verifying = p256::ecdsa::VerifyingKey::from_public_key_pem(&pair.public_pem)
            .expect("公钥可解");
        let signature = p256::ecdsa::Signature::from_der(&der).expect("DER 可解");
        verifying.verify(message, &signature).expect("签名应当能验过");
        // 换一段消息必须验不过 —— 否则"验签通过"只是因为没验。
        assert!(verifying.verify(b"other", &signature).is_err());
    }

    #[test]
    fn the_public_key_can_be_derived_from_a_private_key_alone() {
        // 老凭据可能只留下私钥：续期要带公钥，能推出来就不必让用户重登一次。
        let pair = generate_device_key_pair().expect("生成应当成功");
        assert_eq!(
            Some(pair.public_pem.clone()),
            public_pem_from_private(&pair.private_pem)
        );
        assert!(public_pem_from_private("not a pem").is_none());
    }
}
