//! KukuAI 会话令牌换发：复刻官方客户端引擎签名，向百度通行证换
//! 「genflowpro 作用域」的 STOKEN。
//!
//! ── 为什么需要这一步（2026-10-08 从客户端 app.asar 逆向 + 真实凭证实测）──
//! KukuAI 业务接口（userreport / model/list / sendmsg…）认的 STOKEN 是
//! **按产品签发**的会话令牌：网页登录后 Cookie 里那份是通行证级的，业务
//! 接口一律判「未登录」（errno=-6），而余额这类通用接口只看 BDUSS ——
//! 所以症状总是「余额能查、模型拉不下」（跨天、跨客户端、跨 TLS 指纹、
//! 跨 Cookie 子集全部复现，已逐一排除其它变量）。
//! 官方客户端登录后的做法（app.asar 认证模块逐行对照）：
//!   1. 登录页 `https://kuku.baidu.com/genflowpro/login` 完成通行证登录；
//!   2. 收 Cookie（`BDUSS` / `PTOKEN` / `PASS_STOKEN` / `STOKEN`）；
//!   3. 用引擎构造**签名**参数体（`genflow_engine_get_login_auth_param`：
//!      在 `bduss+ptoken` 之外补 `appid=1&return_type=1&tpl=genflowpro&
//!      tpl_list=genflowpro|netdisk&sig=…`，`sig` 由引擎内置密钥计算，
//!      缺签 / 错签服务端都回 errno=4·110003）；
//!   4. `POST https://passport.baidu.com/v3/login/api/auth` → 响应顶层
//!      `stoken_list.genflowpro` 即业务会话令牌（响应**不带** Set-Cookie，
//!      令牌只存在于响应体，客户端存进引擎侧登录态）；
//!   5. 之后业务接口只用 `BDUSS + 该 STOKEN + gfprotpl=genflowpro` 三件
//!      （实测三件即可，无需其它 Cookie）。
//! 实测（2026-10-08 真实凭证）：换来的 STOKEN 让 userreport 从 errno=-6
//! 变 errno=0；同响应里的 `netdisk` STOKEN 依旧 -6 —— 确实按产品作用域
//! 区分，不是「新令牌就行」。
//!
//! ── 纯算法实现 ─────────────────────────────────────────────
//! 签名字段是 `appid / bduss / ptoken / return_type / tpl / tpl_list`，
//! 追加内置 `sign_key` 后做 MD5。公式已经用真实 DLL 对两组合成输入
//! 验证过，再用真实抓包请求复算过一次；因此不再需要安装或加载
//! `genflowengine.dll`，Windows / Linux 部署都能换发。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use md5::{Digest as _, Md5};
use serde_json::Value;

use crate::server::core::egress;
use crate::server::errors::GatewayError;

/// passport 换发接口（客户端 `app.asar` 认证模块的 `lo` 常量，逐字对照）
const PASSPORT_AUTH_URL: &str = "https://passport.baidu.com/v3/login/api/auth";

/// 官方引擎内置签名密钥（kuku2api 用真实 DLL 与抓包双重验证）
const ENGINE_SIGN_KEY: &str = "34868b316de55815273b2616954b0286";

/// passport 换发接口对客户端引擎 UA 敏感，浏览器 UA 不可靠。
const ENGINE_UA: &str = "genflow;1.6.5;PC;PC-Windows;10.0.26200;GenFlowPro";

/// 构造 passport 换发用的签名参数体。
fn build_auth_param(bduss: &str, ptoken: &str) -> String {
    let signed = format!(
        "appid=1&bduss={bduss}&ptoken={ptoken}&return_type=1&tpl=genflowpro&\
         tpl_list=genflowpro|netdisk&sign_key={ENGINE_SIGN_KEY}"
    );
    let mut hasher = Md5::new();
    hasher.update(signed.as_bytes());
    let signature = format!("{:x}", hasher.finalize());
    format!(
        "appid=1&bduss={bduss}&ptoken={ptoken}&return_type=1&tpl=genflowpro&\
         tpl_list=genflowpro|netdisk&sig={signature}"
    )
}

/// 用 BDUSS + PTOKEN 换发「genflowpro 作用域」的 STOKEN。
///
/// 成功返回新 STOKEN；换发被拒时返回带可读原因的错误，
/// 由调用方决定回退方式。
pub async fn exchange_genflowpro_stoken(bduss: &str, ptoken: &str) -> Result<String, GatewayError> {
    if bduss.trim().is_empty() || ptoken.trim().is_empty() {
        return Err(GatewayError::with_status(
            400,
            "换发 KukuAI 会话令牌需要 BDUSS 与 PTOKEN（登录 Cookie 里缺 PTOKEN）",
        ));
    }
    let body = build_auth_param(bduss, ptoken);
    let client = egress::client_for(None);
    let response = client
        .post(PASSPORT_AUTH_URL)
        .header(
            "Content-Type",
            "application/x-www-form-urlencoded;charset=UTF-8",
        )
        .header("Referer", format!("{}/genflowpro", super::BASE_URL))
        .header("Origin", super::BASE_URL)
        .header("User-Agent", ENGINE_UA)
        .body(body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(502, format!("换发 KukuAI 会话令牌失败：{error}"))
        })?;
    let status = response.status().as_u16();
    let value: Value = response.json().await.map_err(|error| {
        GatewayError::with_status(502, format!("换发 KukuAI 会话令牌响应异常：{error}"))
    })?;
    let errno = value.get("errno").and_then(Value::as_i64).unwrap_or(-1);
    if status != 200 || errno != 0 {
        let message = value
            .get("errmsg")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(GatewayError::with_status(
            502,
            format!("换发 KukuAI 会话令牌被拒（errno={errno}：{message}）"),
        ));
    }
    let stoken = value
        .pointer("/stoken_list/genflowpro")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if stoken.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "换发 KukuAI 会话令牌成功但响应缺少 genflowpro 令牌",
        ));
    }
    Ok(stoken)
}

#[cfg(test)]
mod tests {
    use super::build_auth_param;

    #[test]
    fn auth_param_matches_dll_signature_vectors() {
        let bduss = "A".repeat(192);
        let ptoken = "P".repeat(64);
        let body = build_auth_param(&bduss, &ptoken);
        assert_eq!(
            body,
            format!(
                "appid=1&bduss={bduss}&ptoken={ptoken}&return_type=1&tpl=genflowpro&\
                 tpl_list=genflowpro|netdisk&sig=9cf3a33e3b6f19324853970abf2d00a2"
            )
        );
        assert!(build_auth_param(&"X".repeat(192), &"Y".repeat(64))
            .ends_with("&sig=a3d861ea0946b617a7235b6b36c65add"));
    }
}
