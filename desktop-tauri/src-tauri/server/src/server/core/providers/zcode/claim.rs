//! ZCode 的**限时套餐领取**（manual claim / start plan）。
//!
//! ── 这个功能替代了本家的「签到」────────────────────────────
//! 其余各家都有每日签到（`core::auto_checkin` 的提供商清单），ZCode 没有签到
//! 活动 —— 它的运营玩法是**限时发放的体验套餐**（周末套餐 / Global Build /
//! ZCode Trust Build），用户在客户端里点一下就领，领到的额度在一段时间内可用。
//!
//! ── 「每天领一次」是**套餐 id 实现的**，不是上游的每日签到 ────
//! 2026-09-28 那期（ZCode Trust Build，9/28–11/7）公告写的是「每天登录领 1 亿」，
//! 而上游没有单独的「每日领取」接口：`preview` 每天返回一个**带日期段的新套餐**
//! （`zcode-v3-start-plan-trust-0928` → `…-0929`），于是「今天领过没」等价于
//! 「今天这个 plan_id 领过没」（同一期再领回 `1003 already claimed`）。
//! 本家的界面据此按自然日落一次状态（`claimAt`），见
//! `core::account_store::StoredAccount::claim_at`。
//!
//! ── 上游协议（与 ZCode 客户端的活动投放和领取动作一致）──────
//! 只从当前投放按钮的 `claim_zcode_plan` 动作读取套餐 ID，不执行活动资源或 URL。
//!
//! ```text
//!   探测  GET  {zcode}/api/v1/marketing/touch?seq=
//!   领取  POST {zcode}/api/v1/zcode-plan/billing/claim   body {"plan_id": "..."}
//! ```
//!
//! ── 头集合是「极简 + 一处硬要求」─────────────────────────────
//! 探测与领取都带账号 JWT 和稳定的 `X-Device-Mid`。探测另带版本与语言，
//! 领取另带 Content-Type 与验证码参数；空投放只表示当前没下发领取动作。
//!
//! **`X-Device-Mid` 现在是硬要求**（2026-09-28 实测）：缺它、或值不是 UUID 形态，
//! 网关一律回 `400 {"code":3001,"msg":"parameter error"}` —— 连**匿名探测**也一样
//! （只带版本头/平台头都不管用，只加验证码头也无效）。设备标识还必须**跨请求
//! 稳定**（风控据此关联同一设备的请求），因此它随凭证落盘、缺失时由
//! `AccountStore::zcode_device_mid_or_create` 生成一次后长期复用，
//! **不要**在调用点现编一个。
//!
//! ── 验证码为什么是**参数**而不是本模块自己求解 ───────────────
//! claim 通常要带 `X-Aliyun-Captcha-Verify-Param`（阿里云无痕验证）。
//! 参考实现在进程内用 happy-dom 跑阿里云官方混淆 SDK 求解 —— 那是 Node/Bun
//! 生态的产物（需要 DOM 桩、canvas/WebGL/Worker 垫片、同步 XHR 的 worker 变通），
//! 本网关是纯 Rust 且零 Node 依赖，**不重复实现那套求解器**。
//!
//! 本项目已有一条现成的等价链路：AutoClaw 的 OAuth 登录同样要过阿里云无痕
//! 验证，做法是**前端（Tauri webview）用官方 SDK 求解、把 verifyParam 交给
//! 后端**（见 `ui/autoclaw-oauth.js` 与 `api::session::login_oauth_captcha_config`）。
//! 领取走同一条路：前端解出参数 → 调后端的领取接口 → 本模块把它原样转发给上游。
//! 于是本模块的签名里 `captcha` 是**入参**，本模块只负责「带上它去领取」。
//! 自动领取复用现有验证码生产者提供入参（core::zcode_claim），本模块保持
//! 只负责上游协议。
//!
//! ── 失败分类（biz code → 语义）──────────────────────────────
//! 与客户端 `$vt` 映射器逐条对齐（参考实现 `src/claim/types.ts`）：
//! `1001` 不存在 / `1002` 不可领（活动结束或未开始）/ `1003` 已领过 /
//! `1004` 不符合资格（账号或客户端版本不达标）/ `1005` 当日领取额度用尽 /
//! `3001` 参数错误 / `3007` 验证码失败 / `401` 未登录。
//! 这套分类是**调度语义的输入**：见 `outcome_hold`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use regex::Regex;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

use crate::server::core::auth_http::send_raw;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::update::client::{fetch_public, github_headers};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::region::Region;

/// 单次请求超时（参考实现 `DEFAULT_TIMEOUT_MS` 逐字相同）。
///
/// 必须显式设：`egress` 的默认 read_timeout 是 600 秒（给 SSE 长连接留的），
/// 不设的话一个挂住的领取请求能让定时任务转圈十分钟。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 客户端版本号。
///
/// 上游拿它判「这个客户端够不够新」（`1004 ineligible` 就包含版本不达标），
/// 活动也可能要求最低版本。参考实现把它做成配置项（`identity.appVersion`，
/// 环境变量 `ZCODE_APP_VERSION` 可覆盖）。没有覆盖时，进入 ZCode 低频入口后按
/// 「官方下载安装页 → GitHub Release → GitHub main/package.json」取第一份有效值；
/// 网络不可用时回落到最后一次成功值和 `3.16.3`。
///
/// ★ 别拿参考项目自己的版本号（它 `package.json` 里那个 `4.6.9`）—— 那个数字
/// 冒充不了客户端，`app_version` 与 `X-ZCode-App-Version` 都会对不上，症状是
/// 领取被判 `1004 ineligible`（而这看起来像「账号没资格」，极易误判）。
/// 上游抬门槛时用户可以用环境变量覆盖，不必等发版。
const DEFAULT_APP_VERSION: &str = "3.16.3";
const APP_VERSION_REFRESH_MS: i64 = 30 * 60_000;
const OFFICIAL_DOWNLOAD_PAGE: &str = "https://zcode.z.ai/cn";
const GITHUB_RELEASE: &str = "https://api.github.com/repos/zai-org/ZCode/releases/latest";
const GITHUB_MAIN_PACKAGE: &str =
    "https://raw.githubusercontent.com/zai-org/ZCode/main/package.json";

#[derive(Clone, Debug)]
struct AppVersionCache {
    version: String,
    checked_at_ms: i64,
}

static APP_VERSION_CACHE: OnceLock<Mutex<Option<AppVersionCache>>> = OnceLock::new();

fn app_version_cache() -> &'static Mutex<Option<AppVersionCache>> {
    APP_VERSION_CACHE.get_or_init(|| Mutex::new(None))
}

fn normalize_dynamic_version(value: &str) -> Option<String> {
    let [major, minor, patch] = crate::server::core::update::parse_version(value)?;
    Some(format!("{major}.{minor}.{patch}"))
}

fn configured_app_version() -> Option<String> {
    std::env::var("ZCODE_APP_VERSION")
        .ok()
        .and_then(|value| normalize_dynamic_version(value.trim()))
}

fn parse_official_download_page(html: &str) -> Option<String> {
    let html = html.replace("\\\"", "\"");
    let pattern = Regex::new(r#""version"\s*:\s*"([^"]+)""#).ok()?;
    let mut highest: Option<String> = None;
    for capture in pattern.captures_iter(&html) {
        let Some(candidate) = capture
            .get(1)
            .and_then(|value| normalize_dynamic_version(value.as_str()))
        else {
            continue;
        };
        if highest
            .as_deref()
            .and_then(|current| crate::server::core::update::compare_versions(&candidate, current))
            .map(|order| order > 0)
            .unwrap_or(true)
        {
            highest = Some(candidate);
        }
    }
    highest
}

fn parse_github_release(payload: &Value) -> Option<String> {
    payload
        .get("tag_name")
        .and_then(Value::as_str)
        .and_then(normalize_dynamic_version)
}

fn parse_github_package(payload: &Value) -> Option<String> {
    payload
        .get("version")
        .and_then(Value::as_str)
        .and_then(normalize_dynamic_version)
}

async fn fetch_public_text(url: &str, headers: &[(String, String)]) -> Result<String, String> {
    let response = fetch_public(url, headers, Some(REQUEST_TIMEOUT_MS)).await?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    response
        .text()
        .await
        .map_err(|error| format!("读取响应失败: {error}"))
}

/// 按固定来源优先级刷新客户端版本，不跨来源取最大值。
pub async fn refresh_app_version() -> String {
    if let Some(version) = configured_app_version() {
        return version;
    }

    let now = logging::now_ms();
    let cached = match app_version_cache().lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    if let Some(entry) = cached.as_ref() {
        if now.saturating_sub(entry.checked_at_ms) < APP_VERSION_REFRESH_MS {
            return entry.version.clone();
        }
    }

    let official_headers = vec![(
        "User-Agent".to_string(),
        "agent2api-local-proxy".to_string(),
    )];
    let github_headers = github_headers();
    let mut resolved: Option<(&str, String)> = None;

    if let Ok(body) = fetch_public_text(OFFICIAL_DOWNLOAD_PAGE, &official_headers).await {
        if let Some(version) = parse_official_download_page(&body) {
            resolved = Some(("官方下载安装页", version));
        }
    }
    if resolved.is_none() {
        if let Ok(body) = fetch_public_text(GITHUB_RELEASE, &github_headers).await {
            if let Ok(payload) = serde_json::from_str::<Value>(&body) {
                if let Some(version) = parse_github_release(&payload) {
                    resolved = Some(("GitHub releases/latest", version));
                }
            }
        }
    }
    if resolved.is_none() {
        if let Ok(body) = fetch_public_text(GITHUB_MAIN_PACKAGE, &github_headers).await {
            if let Ok(payload) = serde_json::from_str::<Value>(&body) {
                if let Some(version) = parse_github_package(&payload) {
                    resolved = Some(("GitHub main/package.json", version));
                }
            }
        }
    }

    let (source, version) = resolved.unwrap_or_else(|| {
        (
            "缓存/兜底",
            cached
                .map(|entry| entry.version)
                .unwrap_or_else(|| DEFAULT_APP_VERSION.to_string()),
        )
    });
    match app_version_cache().lock() {
        Ok(mut guard) => {
            *guard = Some(AppVersionCache {
                version: version.clone(),
                checked_at_ms: now,
            });
        }
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            *guard = Some(AppVersionCache {
                version: version.clone(),
                checked_at_ms: now,
            });
        }
    }
    logging::verbose(
        "[ZCode]",
        &format!("客户端版本解析为 {version}（来源：{source}）"),
    );
    version
}

/// 探测与领取的路径（参考实现 `client.ts` 的常量）
const CLAIM_PATH: &str = "/api/v1/zcode-plan/billing/claim";
const MARKETING_PATH: &str = "/api/v1/marketing/touch";

/// 客户端配置（风控参数藏在它的 `data.configs.captcha` 里）
const CLIENT_CONFIGS_PATH: &str = "/api/v1/client/configs";

/// 阿里云无痕验证的配置（前端拿它去初始化 SDK）。
///
/// 三个字段都是 SDK 的必填项，任缺一个就没法初始化：
///   - `prefix` / `region` → `window.AliyunCaptchaConfig`（决定打哪个阿里云站点）
///   - `scene_id` → `initAliyunCaptcha({ SceneId })`
///
/// 参考实现从同一个接口取（`fetchCaptchaConfig`，带 60 秒缓存），本家让
/// 调用方决定要不要缓存 —— 这是低频动作（一次领取取一次），不值得在这里
/// 维护一份带 TTL 的进程级缓存。
#[derive(Clone, Debug)]
pub struct CaptchaConfig {
    /// 上游说这一家此刻要不要验证码。`false` 时前端**不该**弹滑块 ——
    /// 硬弹会让用户在本不需要验证的时候被拦一道
    pub enabled: bool,
    /// 上游说推理模型请求是否跳过验证码令牌。
    pub skip_model_request: bool,
    /// SDK 站点前缀（`window.AliyunCaptchaConfig.prefix`）
    pub prefix: String,
    /// 场景 id（`initAliyunCaptcha` 的 `SceneId`）
    pub scene_id: String,
    /// SDK 站点地区（`window.AliyunCaptchaConfig.region`）
    pub region: String,
}

/// 解析 `/client/configs` 响应中的 captcha 段，供网络入口和纯单测共用。
pub fn parse_captcha_config(payload: &Value) -> Option<CaptchaConfig> {
    let captcha = payload
        .get("data")
        .and_then(|value| value.get("configs"))
        .and_then(|value| value.get("captcha"))?;
    let text = |key: &str| {
        captcha
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let prefix = text("prefix");
    let scene_id = text("sceneId");
    if prefix.is_empty() || scene_id.is_empty() {
        return None;
    }
    Some(CaptchaConfig {
        enabled: captcha.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        skip_model_request: captcha
            .get("skip_model_request")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        prefix,
        scene_id,
        region: {
            let value = text("region");
            if value.is_empty() { "ga".to_string() } else { value }
        },
    })
}

/// 取风控配置（`GET {zcode}/api/v1/client/configs?app_version=&platform=`）。
///
/// 返回 `Ok(None)` = 上游没有下发 captcha 段（或字段不全）：调用方据此
/// **不弹验证码**，而不是弹一个初始化不出来的滑块（那种失败对用户来说
/// 完全无法理解：滑块都没出现就被报「验证码组件不可用」）。
pub async fn captcha_config(
    region: Region,
    proxy: Option<&ResolvedProxy>,
) -> Result<Option<CaptchaConfig>, GatewayError> {
    refresh_app_version().await;
    let url = format!(
        "{}{CLIENT_CONFIGS_PATH}?app_version={}&platform={}",
        region.zcode_origin(),
        urlencode(&app_version()),
        urlencode(platform())
    );
    let response = send_raw("GET", &url, None, &[], proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "获取风控配置超时")
            } else {
                GatewayError::with_status(502, format!("获取风控配置失败: {error}"))
            }
        })?;
    let payload = response.payload.unwrap_or(Value::Null);
    Ok(parse_captcha_config(&payload))
}

/// 领取出错时**优先复核风控配置**（模块头第 3007 档的处置，见 `claim` 的文档）。
///
/// 单独成一个函数是因为它有明确的前置条件与用途：只有「验证码失败」这一档
/// 才值得多花一次往返去问「现在还需要验证码吗」。其他失败（额度用尽、
/// 已领过）复核配置没有意义。
pub fn captcha_hint(config: &CaptchaConfig) -> String {
    if config.enabled {
        "请重新完成验证码后再试".to_string()
    } else {
        // 上游此刻说「不需要验证码」，而领取却因验证码被拒 —— 说明我们带的
        // 那个参数反而成了问题（可能是上一轮的陈旧解）。如实说明，让用户
        // 点一次重试（不带的路径见 `claim` 的调用方）。
        "上游当前未要求验证码，请直接重试一次领取".to_string()
    }
}

/// 客户端版本号（`ZCODE_APP_VERSION` 可覆盖）
pub fn app_version() -> String {
    if let Some(version) = configured_app_version() {
        return version;
    }
    match app_version_cache().lock() {
        Ok(guard) => guard
            .as_ref()
            .map(|entry| entry.version.clone())
            .unwrap_or_else(|| DEFAULT_APP_VERSION.to_string()),
        Err(poisoned) => poisoned
            .into_inner()
            .as_ref()
            .map(|entry| entry.version.clone())
            .unwrap_or_else(|| DEFAULT_APP_VERSION.to_string()),
    }
}

/// 平台标识（参考实现的 `${process.platform}-${process.arch}`）。
///
/// 上游把它当「客户端形态」的一部分。本网关只跑在这三端上，映射写死即可；
/// 未知平台回落到 `win32-x64`（上游对未知值不报错，只是可能不满足活动的
/// 客户端白名单，那时会如实回 `1004`）。
pub fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32-x64"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "darwin-arm64"
        } else {
            "darwin-x64"
        }
    } else if cfg!(target_os = "linux") {
        if cfg!(target_arch = "aarch64") {
            "linux-arm64"
        } else {
            "linux-x64"
        }
    } else {
        "win32-x64"
    }
}

/// 操作系统的**类别名**（`windows` / `macos` / `linux`）。
///
/// 客户端身份头 `X-Os-Category` 与激活事件体的 `device_os_category` 共用
/// 这一处（两处各写一份 cfg 链，迟早有一处漏改）。与 [`platform`] 的区别：
/// 那个是「平台-架构」（`win32-x64`），这个是纯类别名 —— 上游按它分流、
/// 不看架构。认不出的平台落 `linux`（参考实现 `normalizeOsCategory` 的
/// 默认分支同义）。
pub fn os_category() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// 一条活动投放中的可领取套餐；实际权益与有效期由领取结果和余额接口提供。
#[derive(Clone, Debug)]
pub struct ClaimablePlan {
    /// 上游套餐 id（领取时原样回传）
    pub plan_id: String,
    /// 展示名
    pub name: String,
    /// 描述
    pub description: String,
    /// 优先级（同一批多个套餐时，取最大的那个）
    pub priority: i64,
}

/// 领取的失败语义（参考实现 `ClaimFailureKind` 的移植）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClaimFailure {
    /// 套餐不存在（1001）
    NotFound,
    /// 不可领：活动已结束或还没开始（1002）
    Unavailable,
    /// 这个账号已经领过（1003）
    AlreadyClaimed,
    /// 不符合资格：账号或客户端版本不达标（1004）
    Ineligible,
    /// 当日领取额度用尽（1005）
    QuotaExhausted,
    /// 参数错误（3001）—— 最常见的成因是缺 `X-Device-Mid`
    InvalidRequest,
    /// 验证码校验失败（3007）
    Captcha,
    /// 上游风控拦截（3012）
    RiskBlocked,
    /// 未登录 / JWT 失效（401）
    LoginRequired,
    /// 其它 HTTP 错误
    HttpError,
    /// 认不出的业务码
    Unknown,
}

impl ClaimFailure {
    /// 给用户看的一句话（进日志与界面）
    pub fn label(self) -> &'static str {
        match self {
            Self::NotFound => "套餐不存在",
            Self::Unavailable => "活动已结束或尚未开始",
            Self::AlreadyClaimed => "该账号已领取过",
            Self::Ineligible => "账号或客户端版本不符合活动资格",
            Self::QuotaExhausted => "当日领取额度已用尽",
            Self::InvalidRequest => "参数错误（常见成因：缺少设备标识）",
            Self::Captcha => "验证码校验未通过",
            Self::RiskBlocked => "上游风控拦截（异常活动）",
            Self::LoginRequired => "登录态已失效，请重新登录",
            Self::HttpError => "上游 HTTP 错误",
            Self::Unknown => "未知失败",
        }
    }

    /// biz code → 语义（与客户端 `$vt` 映射器逐条对齐）
    fn from_code(code: i64) -> Self {
        match code {
            1001 => Self::NotFound,
            1002 => Self::Unavailable,
            1003 => Self::AlreadyClaimed,
            1004 => Self::Ineligible,
            1005 => Self::QuotaExhausted,
            3001 => Self::InvalidRequest,
            3007 => Self::Captcha,
            3012 => Self::RiskBlocked,
            401 => Self::LoginRequired,
            _ => Self::Unknown,
        }
    }
}

/// 领取的结果
#[derive(Clone, Debug)]
pub enum ClaimOutcome {
    /// 领到了
    Claimed {
        /// 套餐 id
        plan_id: String,
        /// 生效时间（unix 秒；缺省表示立即生效）
        starts_at: Option<i64>,
        /// 失效时间（unix 秒）
        ends_at: Option<i64>,
    },
    /// 没领到（含「已领过」这类不算错误的结果）
    Failed {
        /// 套餐 id
        plan_id: String,
        /// 失败语义
        failure: ClaimFailure,
        /// 上游业务码（进日志）
        code: i64,
        /// 上游原文
        message: String,
        /// 上游给的下一个可尝试窗口（unix 秒；`1005` 额度用尽时通常有）
        failure_ends_at: Option<i64>,
    },
}

/// 一次领取尝试之后**下次什么时候再试**（调度语义，参考实现 `scheduler.ts`）。
#[derive(Clone, Copy, Debug)]
pub enum NextAttempt {
    /// 本账号在本期活动里无事可做：等到这个 unix 秒（套餐失效时间）之后再说。
    /// 用于 `Claimed` 与 `AlreadyClaimed` —— 两者都表示「这一期已经领到了」。
    HoldUntil(i64),
    /// 换一个窗口再试：等到这个 unix 秒（上游给的下一窗口）。
    /// 用于 `QuotaExhausted`。
    WaitWindow(i64),
    /// 退避：按常规冷却时长再试。用于验证码失败 / 网络错误 / 认不出的失败。
    Cooldown,
}

/// 从领取结果推出下一次尝试时机。
///
/// 为什么单独成函数：这段判断是**调度正确性**的核心（把「已领过」当成失败
/// 去重试会一直打上游、把「额度用尽」当成已领到会整期不再尝试），
/// 放在一处并配上测试视角的注释，比散在调度循环里更难写错。
pub fn outcome_hold(outcome: &ClaimOutcome) -> NextAttempt {
    match outcome {
        // 领到了：本期的目标已达成，等它过期（缺 ends_at 时给 0，由调用方
        // 回落成常规轮询 —— 没有结束时间的套餐不该被无限期 hold 住）
        // 解引用是必需的：这里匹配的是 `&ClaimOutcome`，字段拿到的是
        // `&Option<i64>`（`Option<i64>` 是 Copy，`*` 出来即可）
        ClaimOutcome::Claimed { ends_at, .. } => NextAttempt::HoldUntil((*ends_at).unwrap_or(0)),
        ClaimOutcome::Failed { failure, failure_ends_at, .. } => match failure {
            // 已领过 = 本期的目标其实也达成了（可能是用户手动领的 / 另一台设备领的）
            ClaimFailure::AlreadyClaimed => {
                NextAttempt::HoldUntil((*failure_ends_at).unwrap_or(0))
            }
            // 额度用尽：上游通常给出下一个窗口
            ClaimFailure::QuotaExhausted => match failure_ends_at {
                Some(at) => NextAttempt::WaitWindow(*at),
                None => NextAttempt::Cooldown,
            },
            // 活动没开始 / 不符合资格 / 参数错误 / 验证码 / 风控 / 网络：都按冷却重试。
            // 其中 `Unavailable`（活动未开始）**不能**长退避 —— 它恰恰是开抢前
            // 最常见的一档，退避太久会错过上新。
            _ => NextAttempt::Cooldown,
        },
    }
}

/// 按账号读取客户端活动投放；调用方负责提供持久化的稳定设备标识。
pub async fn preview(
    region: Region,
    jwt: &str,
    device_mid: Option<&str>,
    proxy: Option<&ResolvedProxy>,
) -> Result<Vec<ClaimablePlan>, GatewayError> {
    if jwt.trim().is_empty() {
        return Err(GatewayError::with_status(401, "登录态为空，无法探测套餐活动"));
    }
    refresh_app_version().await;
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {}", jwt.trim())),
        ("X-ZCode-App-Version".to_string(), app_version()),
        ("X-Client-Language".to_string(), "zh-CN".to_string()),
    ];
    if let Some(mid) = device_mid.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push(("X-Device-Mid".to_string(), mid.to_string()));
    }

    let url = format!(
        "{}{MARKETING_PATH}?seq={}",
        region.zcode_origin(),
        logging::now_ms()
    );
    let response = send_raw("GET", &url, None, &headers, proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            GatewayError::with_status(
                if error.is_timeout() { 504 } else { 502 },
                "套餐活动投放探测请求失败",
            )
        })?;
    if !(200..300).contains(&response.status) {
        return Err(GatewayError::with_status(
            if response.status == 401 { 401 } else { 502 },
            format!("套餐活动投放探测失败（HTTP {}）", response.status),
        ));
    }
    let payload = response
        .payload
        .ok_or_else(|| GatewayError::with_status(502, "套餐活动投放探测：上游响应不是 JSON"))?;
    parse_marketing_preview(&payload)
}

fn parse_marketing_preview(payload: &Value) -> Result<Vec<ClaimablePlan>, GatewayError> {
    let invalid =
        || GatewayError::with_status(502, "套餐活动投放探测：上游数据不完整，无法确认可领状态");
    let data = payload.get("data").ok_or_else(invalid)?;
    if business_code(payload) != Some(0)
        || data.get("server_time").and_then(Value::as_f64).is_none()
        || !matches!(
            data.get("language").and_then(Value::as_str),
            Some("zh-CN" | "en-US")
        )
    {
        return Err(invalid());
    }
    let deliveries = data
        .get("deliveries")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if deliveries.len() > 32 {
        return Err(invalid());
    }
    fn identifier(value: Option<&Value>) -> Option<&str> {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.chars().count() <= 128)
    }
    fn plain_text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
        let text = value.get(key)?;
        (text.get("format")?.as_str()? == "plaintext")
            .then(|| text.get("content")?.as_str())
            .flatten()
            .filter(|text| text.chars().count() <= 20_000)
    }
    let mut plans: Vec<ClaimablePlan> = Vec::new();
    for delivery in deliveries {
        let position = delivery
            .get("resource_position")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        if !matches!(position, "banner" | "popup")
            || identifier(delivery.get("campaign_id")).is_none()
        {
            return Err(invalid());
        }
        let priority = delivery
            .get("priority")
            .and_then(Value::as_i64)
            .filter(|priority| (0..=100).contains(priority))
            .ok_or_else(invalid)?;
        let content = delivery.get(position).ok_or_else(invalid)?;
        let buttons = content
            .get("buttons")
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        if buttons.len() > if position == "banner" { 2 } else { 4 } {
            return Err(invalid());
        }
        // 只认客户端展示按钮的领取动作，不执行 URL、资源脚本或领取后的 success_popup。
        for button in buttons {
            let action = button.get("action").ok_or_else(invalid)?;
            let action_type = action
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            if action_type != "claim_zcode_plan" {
                continue;
            }
            let plan_id = identifier(action.get("args").and_then(|args| args.get("plan_id")))
                .ok_or_else(invalid)?;
            if let Some(plan) = plans.iter_mut().find(|plan| plan.plan_id == plan_id) {
                plan.priority = plan.priority.max(priority);
                continue;
            }
            let name = plain_text(content, "title")
                .filter(|text| !text.trim().is_empty())
                .unwrap_or(plan_id);
            let description = plain_text(content, "description").unwrap_or("");
            plans.push(ClaimablePlan {
                plan_id: plan_id.to_string(),
                name: name.to_string(),
                description: description.to_string(),
                priority,
            });
        }
    }
    Ok(plans)
}

/// 领取一个套餐。
///
/// `captcha_verify_param` 由**前端**解出（见模块头：本网关不在 Rust 侧复刻
/// 阿里云 SDK 求解器）。`captcha_region` 是同一套 SDK 给出的地区标识，可选。
///
/// 返回 `Err` 只用于「请求本身没打成」（网络 / 超时 / 响应不是 JSON）；
/// 上游明确拒绝（含验证码失败、已领过）一律走 `Ok(ClaimOutcome::Failed)` ——
/// 那些是**业务结果**，调用方要靠 `failure` 决定下次何时再试。
pub async fn claim(
    region: Region,
    jwt: &str,
    plan_id: &str,
    captcha_verify_param: &str,
    captcha_region: Option<&str>,
    device_mid: Option<&str>,
    proxy: Option<&ResolvedProxy>,
) -> Result<ClaimOutcome, GatewayError> {
    refresh_app_version().await;
    if jwt.trim().is_empty() {
        return Ok(ClaimOutcome::Failed {
            plan_id: plan_id.to_string(),
            failure: ClaimFailure::LoginRequired,
            code: 401,
            message: "领取需要登录态（本账号没有 ZCode JWT）".to_string(),
            failure_ends_at: None,
        });
    }
    let url = format!("{}{CLAIM_PATH}", region.zcode_origin());
    // 头顺序与客户端一致：Authorization → Content-Type → 验证码 → 版本 → 平台
    // → 设备标识。顺序本身不是协议要求，但保持与参考实现同序便于逐条比对。
    //
    // ── 验证码头**只在有值时发**（一处容易想当然的地方）──────────
    // 上游的风控配置可能是 `enabled: false`（那一刻不要验证码），此时前端
    // 根本不会弹滑块、也就没有 verifyParam。若把空串也写成头
    // （`X-Aliyun-Captcha-Verify-Param: `），上游会把它当成「给了一个无效的
    // 验证串」而回 3007 —— 于是「不需要验证码」的那一刻反而领不了，
    // 而报错文案是「验证码校验未通过」，指向完全错误的方向。
    // 因此有值才发；没有就让上游按它自己的规则判（要就回 3007，不要就放行）。
    let mut headers: Vec<(String, String)> = vec![
        ("Authorization".to_string(), format!("Bearer {}", jwt.trim())),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if let Some(param) = Some(captcha_verify_param.trim()).filter(|value| !value.is_empty()) {
        headers.push(("X-Aliyun-Captcha-Verify-Param".to_string(), param.to_string()));
    }
    if let Some(value) = captcha_region.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push(("X-Aliyun-Captcha-Verify-Region".to_string(), value.to_string()));
    }
    headers.push(("X-ZCode-App-Version".to_string(), app_version()));
    headers.push(("X-Platform".to_string(), platform().to_string()));
    if let Some(mid) = device_mid.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push(("X-Device-Mid".to_string(), mid.to_string()));
    }

    let body = json!({ "plan_id": plan_id });
    let response = send_raw(
        "POST",
        &url,
        Some(&body),
        &headers,
        proxy,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, "套餐领取超时")
        } else {
            GatewayError::with_status(502, format!("套餐领取失败: {error}"))
        }
    })?;

    let payload = response.payload.unwrap_or(Value::Null);
    let biz_code = business_code(&payload);
    let data = payload.get("data");
    let plan = data.and_then(|value| value.get("plan"));

    // 成功：HTTP 2xx + biz code 0 + data.plan 存在（三者缺一不可 —— 参考实现
    // 同样要求 data.plan 在场，因为只有它带 starts_at/ends_at）
    if (200..300).contains(&response.status) && biz_code == Some(0) {
        if let Some(plan) = plan {
            return Ok(ClaimOutcome::Claimed {
                plan_id: plan_id.to_string(),
                starts_at: plan.get("starts_at").and_then(Value::as_i64),
                ends_at: plan.get("ends_at").and_then(Value::as_i64),
            });
        }
    }

    // 失败：HTTP 层错误（无业务码）与业务码错误分开归类 —— 前者只有状态码
    // 可依（401 判未登录，其余算 HTTP 错误），后者走 biz code 映射
    let failure = match biz_code {
        Some(code) => ClaimFailure::from_code(code),
        None if response.status == 401 => ClaimFailure::LoginRequired,
        None => ClaimFailure::HttpError,
    };
    // 上游在失败响应里也可能带 plan（含 ends_at），那就是「下一个可尝试窗口」
    let failure_ends_at = plan
        .and_then(|value| value.get("ends_at"))
        .and_then(Value::as_i64);
    Ok(ClaimOutcome::Failed {
        plan_id: plan_id.to_string(),
        failure,
        code: biz_code.unwrap_or(i64::from(response.status)),
        message: error_message(&payload, response.status),
        failure_ends_at,
    })
}

/// 从上游响应中归一化业务码。
///
/// 正常响应使用顶层数字 `code`，但风控网关的部分响应会返回字符串，
/// 某些错误包装还会把 code 放进 `data`。统一读取能避免这些响应被误报为未知失败。
fn business_code(payload: &Value) -> Option<i64> {
    fn parse(value: &Value) -> Option<i64> {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
    }

    payload.get("code").and_then(parse).or_else(|| {
        payload
            .get("data")
            .and_then(|data| data.get("code"))
            .and_then(parse)
    })
}

/// 从上游响应里取一句人话（参考实现 `unwrapError` 的同效处理）
fn error_message(payload: &Value, status: u16) -> String {
    for key in ["msg", "message"] {
        if let Some(text) = payload.get(key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    format!("HTTP {status}")
}

/// 从一批可领套餐里挑出目标：指定了 `plan_id` 就取它，否则取优先级最高的。
///
/// 参考实现的选取口径：`planId` 配置为空时按 `priority` 降序取第一个。
/// 优先级相同的情形保持**上游给的顺序**（`max_by_key` 取最后一个的语义陷阱
/// 要避开：这里用显式比较，相等时不替换，于是先出现的那个胜出）。
pub fn pick_target<'a>(plans: &'a [ClaimablePlan], plan_id: &str) -> Option<&'a ClaimablePlan> {
    let wanted = plan_id.trim();
    if !wanted.is_empty() {
        return plans.iter().find(|plan| plan.plan_id == wanted);
    }
    let mut best: Option<&ClaimablePlan> = None;
    for plan in plans {
        match best {
            None => best = Some(plan),
            Some(current) if plan.priority > current.priority => best = Some(plan),
            Some(_) => {}
        }
    }
    best
}

/// 最小的 URL 查询参数编码（`url` crate 不在依赖里，这里只处理查询值）。
///
/// 版本号与平台标识都是 ASCII 字母数字与 `-` / `.`，理论上不需要转义；
/// 但 `ZCODE_APP_VERSION` 是用户可覆盖的，一个带 `&` 的值就能拼出畸形 URL。
/// 因此这里按 RFC 3986 的 unreserved 集合放行，其余一律百分号编码。
///
/// `pub(super)`：`oauth` 那边的中转页参数要把**整个地址**当成一个查询值编码
/// （见 `oauth::apply_interstitial`），同一套规则不必再写第二遍。
pub(super) fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn marketing_preview_requires_account_jwt() {
        for region in Region::ALL {
            let error = preview(region, " ", None, None).await.unwrap_err();
            assert_eq!(error.status_code, 401);
        }
    }

    #[test]
    fn marketing_preview_distinguishes_real_empty_from_incomplete_responses() {
        let payload = json!({
            "code": 0, "msg": "",
            "data": {"server_time": 1793340575, "language": "zh-CN", "deliveries": []}
        });
        assert!(parse_marketing_preview(&payload).unwrap().is_empty());
        for invalid in [
            json!({"code": 0}),
            json!({"code": 0, "data": null}),
            json!({"code": 0, "data": {}}),
            json!({"data": {"server_time": 1, "language": "zh-CN", "deliveries": []}}),
            json!({"code": 3012, "data": {"server_time": 1, "language": "zh-CN", "deliveries": []}}),
            json!({"code": 0, "data": {"server_time": 1, "language": "unknown", "deliveries": []}}),
            json!({"code": 0, "data": {"server_time": 1, "language": "zh-CN", "deliveries": null}}),
        ] {
            assert!(parse_marketing_preview(&invalid).is_err(), "{invalid}");
        }
        let mut malformed = payload.clone();
        malformed["data"]["deliveries"] = json!([{"resource_position": "popup"}]);
        assert!(parse_marketing_preview(&malformed).is_err());
    }

    #[test]
    fn marketing_preview_uses_only_live_claim_buttons_and_preserves_plan_ids() {
        let mut payload = json!({
            "code": 0,
            "data": {
                "server_time": 1793340575, "language": "en-US",
                "deliveries": [
                    {
                        "campaign_id": "campaign-one", "resource_position": "banner", "priority": 4,
                        "banner": {
                            "buttons": [
                                {"action": {"type": "open_url", "args": {"url": "https://example.com"}}},
                                {"action": {"type": "claim_zcode_plan", "args": {"plan_id": "real-plan"}}}
                            ],
                            "success_popup": {
                                "buttons": [{"action": {"type": "claim_zcode_plan", "args": {"plan_id": "not-live"}}}]
                            }
                        }
                    },
                    {
                        "campaign_id": "campaign-two", "resource_position": "popup", "priority": 9,
                        "popup": {
                            "title": {"format": "html", "content": "<b>Do not render</b>"},
                            "description": {"format": "markdown", "content": "[Do not render](https://example.com)"},
                            "buttons": [{"action": {"type": "claim_zcode_plan", "args": {"plan_id": "real-plan"}}}]
                        }
                    }
                ]
            }
        });
        let plans = parse_marketing_preview(&payload).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].plan_id, "real-plan");
        assert_eq!(plans[0].name, "real-plan");
        assert!(plans[0].description.is_empty());
        assert_eq!(plans[0].priority, 9);

        payload["data"]["deliveries"][0]["banner"]["buttons"][1]["action"]["args"]["plan_id"] =
            json!(" ");
        assert!(parse_marketing_preview(&payload).is_err());
    }

    #[test]
    fn maps_risk_block_and_preserves_unknown_codes() {
        assert_eq!(ClaimFailure::from_code(3012), ClaimFailure::RiskBlocked);
        assert_eq!(ClaimFailure::from_code(3999), ClaimFailure::Unknown);
    }

    #[test]
    fn normalizes_numeric_business_codes() {
        assert_eq!(business_code(&json!({"code": 3012})), Some(3012));
        assert_eq!(business_code(&json!({"code": "3012"})), Some(3012));
        assert_eq!(
            business_code(&json!({"data": {"code": "1004"}})),
            Some(1004)
        );
        assert_eq!(business_code(&json!({"code": "not-a-code"})), None);
    }

    #[test]
    fn normalizes_client_build_versions() {
        assert_eq!(
            normalize_dynamic_version("3.16.3"),
            Some("3.16.3".to_string())
        );
        assert_eq!(
            normalize_dynamic_version("3.16.3.8992"),
            Some("3.16.3".to_string())
        );
        assert_eq!(normalize_dynamic_version("not-a-version"), None);
    }

    #[test]
    fn official_page_parser_uses_highest_embedded_release() {
        let html = r#"{"version":"3.14.0"} \"version\":\"3.16.3\" {"version":"bad"}"#;
        assert_eq!(
            parse_official_download_page(html),
            Some("3.16.3".to_string())
        );
    }

    #[test]
    fn github_parsers_read_the_expected_fields() {
        assert_eq!(
            parse_github_release(&json!({"tag_name": "v3.14.3"})),
            Some("3.14.3".to_string())
        );
        assert_eq!(
            parse_github_package(&json!({"version": "3.16.2"})),
            Some("3.16.2".to_string())
        );
        assert_eq!(parse_github_release(&json!({"name": "3.14.3"})), None);
    }

}


#[cfg(test)]
mod captcha_config_vectors {
    //! `configs.captcha` 的解析，钉在**抓下来的真实响应**上。
    //!
    //! 这一格现在是整套免码判定的唯一依据（`skip_model_request`），而它的形状
    //! 只有上游能告诉我们 —— 手写一份"想象中的响应"等于把猜测当证据。
    //! 载荷出处：2026-10-01 16:44 UTC 从 `https://zcode.z.ai/api/v1/client/configs`
    //! 抓的 captcha 段，四个组合（app_version 3.14.0/3.14.4 × platform
    //! darwin-arm64/linux-x64）返回逐字相同，所以这里只需一份。

    use serde_json::json;

    use super::parse_captcha_config;

    #[test]
    fn the_live_captcha_segment_parses_as_captured() {
        let config = parse_captcha_config(&json!({
            "code": 0,
            "msg": "",
            "data": {"configs": {"captcha": {
                "enabled": true, "prefix": "no8xfe", "region": "cn",
                "sceneId": "11xygtvd", "skip_model_request": true
            }}}
        }))
        .expect("这份响应是上游真实给过的");
        assert!(
            config.enabled,
            "领取那条路仍然要滑块 —— 它和推理免码是两件事"
        );
        assert_eq!("no8xfe", config.prefix);
        assert_eq!("11xygtvd", config.scene_id);
        assert_eq!("cn", config.region, "站点地区上游给了就别自己兜底成 ga");
        assert!(
            config.skip_model_request,
            "这一格是免码上线的全部依据，不能被解析成 false"
        );
    }

    #[test]
    fn an_absent_skip_flag_still_requires_a_token() {
        // 缺 `skip_model_request`（上游还没下发这一格）= 照旧要码。
        // 判反了的代价不对称：免码判错每条请求撞一次 3007（烧账号行为分），
        // 要码判错只是白铸一枚令牌。
        let config = parse_captcha_config(&json!({
            "data": {"configs": {"captcha": {
                "enabled": true, "prefix": "no8xfe", "sceneId": "11xygtvd"
            }}}
        }))
        .expect("prefix 与 sceneId 齐了就认这份配置");
        assert!(!config.skip_model_request);
        assert_eq!(
            "ga", config.region,
            "没给 region 时的兜底（阿里云默认站点）"
        );
    }

    #[test]
    fn an_incomplete_segment_is_not_a_config_at_all() {
        // 缺 sceneId 时前端连 SDK 都初始化不出来 —— 按「没有配置」处理，
        // 而不是给出一份字段为空的对象（调用方会拿它去弹滑块，然后当场失败）
        assert!(
            parse_captcha_config(&json!({"data": {"configs": {"captcha": {
                "enabled": true, "prefix": "no8xfe"
            }}}}))
            .is_none()
        );
        assert!(parse_captcha_config(&json!({"data": {"configs": {}} })).is_none());
        assert!(parse_captcha_config(&json!({})).is_none());
    }
}
