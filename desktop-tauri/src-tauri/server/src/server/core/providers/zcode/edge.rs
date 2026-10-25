//! **边缘拦截页**（非业务错误的 HTML 响应）的识别与处置。
//!
//! ── 要解决的问题 ──────────────────────────────────────────
//! 2026-10-25 13:18–13:39 的一轮生产故障：ZCode 套餐池通道连续 36 条请求全部
//! 405，错误体是**一页 HTML**（`<!doctypehtml><html lang="zh-cn">…<title>405</title>`
//! `data-spm="a3c0e"`），不是本项目的业务错误 JSON（`{"code":3012,...}`）。
//! 同一出口、同一账号、同一份请求体在当天 16:50 原样重放 → 200；官方客户端同
//! 时段一直正常。也就是说：**上游业务层是好的**，那 20 分钟里挡住我们的是
//! 上游前面的边缘层（阿里云 ESA 系的拦截页），它按**源站/出口**生效 ——
//! 换账号无效（故障期间两个账号各被拒一次，见 `requests` 表 6830/7029 的
//! `attempt_details`）。
//!
//! ── 代码当时的两处缺陷（把它们放大成一次全通道瘫痪）──────
//!   1. **判错类**：HTML 体里没有 `code` 字段，`classify_error` 落到
//!      `content_block::classify_or_fatal` 的兜底 `Fatal`，编排层据此「换账号
//!      再试」—— 对按出口生效的拦截完全无效，只是拿第二个账号又喂了拦截器
//!      一次（`attempts=2`）；
//!   2. **没有刹车**：客户端每重试一次就打一次上游（实测 70 秒内 20 条请求 →
//!      40 次上游往返），而「持续高频未通过」正是这类拦截**续命**的条件。
//!
//! ── 处置：判成「这一家在**出口**上被挡住」，并复用本地闸门 ────────
//! 识别到拦截页时：
//!   · 分类给 [`UpstreamErrorClass::Terminal`] + [`UpstreamStopReason::ChannelRefused`]
//!     —— 编排层见 `Terminal` 立即返回、**不换号、不原地重发**（这与
//!     WorkBuddy 的 `ChannelRefused` 同一档语义）；
//!   · 顺手 [`readiness::hold`] 一道 provider 级闸门：闸门期间
//!     `upstream::rotate::accounts_in_providers` 把这家摘出候选池，后续请求
//!     **根本不会送到这家**；闸门带 [`readiness::GATE_TTL_MS`] 寿命，到点自动
//!     放行重探一次 —— 拦截解除后无需任何人手工复位，拦截持续时也只有一个
//!     低频探测在打上游。
//!
//! 为什么复用 `readiness` 而不是新造一套熔断：那道闸门的语义（provider 级、
//! 可自愈、选路侧绕行、556 区分「从未发出」）与这里需要的**完全相同**，两套
//! 并存只会让「这家为什么被绕开」有两个答案。
//!
//! ── 判据为什么是「HTML 体」而不是某个状态码 ────────────────
//! 状态码不稳定（同一层还会回 403 / 502 / 503），而**「响应体是一页 HTML」**
//! 是这一层区别于业务 JSON 的稳定特征：能走到业务层的响应一律是 JSON，
//! 网关自己的 `read_upstream_error` 也会把非 JSON 体原样塞进 `message`。
//! 所以判据取「`status >= 400` + 没有业务码 + 体像 HTML」，不认具体码值。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! `panic=abort`：本文件零 unwrap/expect/panic。

use crate::server::logging;

use super::super::adapter::{UpstreamErrorClass, UpstreamStopReason};
use super::super::readiness;

/// 响应体像 HTML 文档（而不是业务 JSON）。
///
/// 判据刻意保守：去空白后以 `<` 开头，且含 `<html` / `<!doctype` / `<head`。
/// 业务错误体的第一个字符是 `{`，永远不会命中；而纯文本错误（`Unauthorized`
/// 这类）也不命中 —— 那些应当继续走普通的 Fatal 透传。
pub fn looks_like_html(raw: &str) -> bool {
    let text = raw.trim_start();
    if !text.starts_with('<') {
        return false;
    }
    let lowered = text[..text.len().min(512)].to_ascii_lowercase();
    lowered.contains("<html") || lowered.contains("<!doctype") || lowered.contains("<head")
}

/// 这次失败是不是「我们的请求压根没进到业务层」的边缘拦截页。
///
/// `code` 是上游业务码：只要解析出了业务码，就说明业务层答过话，与拦截页
/// 无关（哪怕体里同时夹着 HTML —— 那属于上游自己的错误页格式）。
pub fn is_edge_page(status: u16, code: Option<i64>, raw: &str) -> bool {
    status >= 400 && code.is_none() && looks_like_html(raw)
}

/// 客户端可见的文案：说清「谁拦的、与谁无关、现在该做什么」。
///
/// 不写「上游已迁移该端点」这类**未经证实的因果** —— 10-25 那轮故障里
/// 这条断言是错的（旧端点在故障前后都是 200，见模块头），写进用户文案会
/// 把人劝去改配置。
pub fn blocked_message(status: u16) -> String {
    format!(
        "上游边缘网关拦截（HTTP {status} 返回的是拦截页而不是业务响应，本请求未进入业务层）：\
         这类拦截按**出口 / 源站**生效，与账号额度和提示词都无关 —— 换账号、改提示词、改模型都不解决。\
         已记下这次拦截并暂停对该通道的连发（约 {ttl} 秒后自动重探），稍后重试即可；\
         持续出现请检查本机到上游的网络出口（代理 / 加速器 / 机房 IP）",
        ttl = readiness::GATE_TTL_MS / 1000
    )
}

/// 记下这次拦截并升起 provider 级闸门；返回分类结果供适配器直接返回。
///
/// `hold` 返回 `true` = 本次让闸门从放行变成挡住 —— 日志只在这一刻打一行
/// （逐请求打会刷屏，且拦截期间每条请求都会走到这里）。
pub fn blocked_class(
    provider_id: &str,
    status: u16,
    upstream_code: Option<i64>,
) -> UpstreamErrorClass {
    let message = blocked_message(status);
    if readiness::hold(provider_id, &message) {
        logging::console_line(
            "[Routing]",
            &format!(
                "⚠️ {provider_id} 返回边缘拦截页（HTTP {status}），选路暂时绕开这一家；\
                 最长 {} 秒后自动重探一次",
                readiness::GATE_TTL_MS / 1000
            ),
        );
    }
    UpstreamErrorClass::Terminal {
        kind: UpstreamStopReason::ChannelRefused,
        status,
        message,
        upstream_code,
    }
}

/// 构造期闸门：还在拦截冷却里时**不要发**，直接给本地前置条件错误。
///
/// 与 [`readiness::LOCAL_PRECONDITION_STATUS`] 同一档语义（一次上游往返都没
/// 发生），编排层见它会顺延下一个候选家而不是终止整份请求 —— 这是
/// `rotate::accounts_in_providers` 之外的**第一枪兜底**（选路那次过滤之后
/// 才被选中的账号仍会走到构造）。
pub fn gate_error(provider_id: &str) -> Option<crate::server::errors::GatewayError> {
    if !readiness::gated(provider_id) {
        return None;
    }
    Some(crate::server::errors::GatewayError::with_status(
        i32::from(readiness::LOCAL_PRECONDITION_STATUS),
        format!(
            "{} 这一家刚被上游边缘层拦截，正在退让中（最长 {} 秒重探一次）：稍后重试即可",
            provider_id,
            readiness::GATE_TTL_MS / 1000
        ),
    ))
}

/// 这一家被上游答上了话（拿到真实响应体）→ 撤掉拦截闸门。
///
/// **不能省掉这一步**：闸门只有寿命、没有别的解除条件，一次十几秒的抖动会让
/// 这一家白绕开两分钟（流量落到更贵的别家）。上游既然已经答话，就没有继续
/// 挡着的理由。返回值与 [`readiness::release`] 一致（`true` = 确实撤掉了）。
///
/// 它同时也覆盖另一类闸门（活动套餐的令牌池空了）—— 那是**同一个
/// provider 级事实**的另一种成因，而「答上话了」同样证伪了它。
pub fn note_recovered(provider_id: &str) -> bool {
    readiness::release(provider_id)
}

#[cfg(test)]
mod edge_page {
    //! 判据三条 + 闸门接线一条。
    //!
    //! 闸门那一条必须拿 [`readiness::lock_for_tests`]：那张表是进程级的，
    //! 并发的用例互相能把对方的前提改掉。

    use super::*;

    /// 生产上真实收到的那一页（`requests` 表 6830 的 `error` 列，去掉尾部省略）。
    const REAL_PAGE: &str = "<!doctypehtml><html lang=\"zh-cn\"><meta charset=\"utf-8\">\
        <meta http-equiv=\"X-UA-Compatible\"content=\"IE=edge,chrome=1\">\
        <meta name=\"data-spm\"content=\"a3c0e\"><title>405</title><style>a,body,div,h</style>";

    #[test]
    fn the_real_interception_page_is_recognized() {
        assert!(looks_like_html(REAL_PAGE));
        assert!(is_edge_page(405, None, REAL_PAGE));
        // 前导空白与换行不影响（实测体以 `<!doctypehtml>` 直接开头，但没有理由
        // 把带空白的变体判丢）
        assert!(is_edge_page(405, None, &format!("\n  {REAL_PAGE}")));
        // 403 / 502 / 503 是同一层的其它说法，判据不认具体码值
        for status in [403u16, 405, 502, 503] {
            assert!(is_edge_page(status, None, REAL_PAGE), "HTTP {status}");
        }
    }

    #[test]
    fn business_errors_are_not_edge_pages() {
        // 3012：业务层的风控 JSON —— 它有自己的文案与动作，不能当成边缘拦截
        let blocked = r#"{"code":3012,"msg":"request has been blocked due to unusual activity."}"#;
        assert!(!looks_like_html(blocked));
        assert!(!is_edge_page(405, None, blocked));
        // 哪怕体里夹着 HTML，只要解析出了业务码，就说明业务层答过话
        assert!(!is_edge_page(405, Some(3012), REAL_PAGE));
        // 纯文本错误（`Unauthorized` 这类）继续走普通透传
        assert!(!is_edge_page(401, None, "token expired or incorrect"));
        assert!(!is_edge_page(500, None, ""));
        // 2xx 不可能被当成拦截页（那些走 `inspect_success_head`）
        assert!(!is_edge_page(200, None, REAL_PAGE));
    }

    #[test]
    fn the_client_message_does_not_invent_a_cause() {
        let message = blocked_message(405);
        // 这两句都必须是「未证实」的姿态：10-25 那轮里「端点已迁移」是错的，
        // 旧端点在故障前后都是 200（见模块头）。文案里出现它 = 把用户劝去改配置。
        assert!(!message.contains("迁移"));
        assert!(!message.contains("升级本程序"));
        // 必须说清「换账号无效」这条可执行结论
        assert!(message.contains("换账号"));
        assert!(message.contains("出口"));
    }

    #[test]
    fn an_interception_page_holds_the_provider_gate() {
        let _slot = readiness::lock_for_tests();
        readiness::clear_for_tests();
        // 拦截页 → Terminal/ChannelRefused：编排层见 Terminal 立即返回，
        // 不换号也不原地重发（这正是故障期间多烧掉一个账号的原因）
        let class = blocked_class("zcode-cn", 405, None);
        let UpstreamErrorClass::Terminal { kind, status, .. } = &class else {
            panic!("拦截页必须判成 Terminal，实际 {class:?}");
        };
        assert_eq!(*kind, UpstreamStopReason::ChannelRefused);
        assert_eq!(*status, 405);
        // 闸门升起：后续请求在选路侧就被绕开，且构造期兜底也在
        assert!(readiness::gated("zcode-cn"));
        assert!(gate_error("zcode-cn").is_some());
        // 另一家不受牵连
        assert!(!readiness::gated("zcode"));
        assert!(gate_error("zcode").is_none());
        // 复用 readiness 的寿命：到期自动放行，不需要谁去手工复位
        assert!(readiness::release("zcode-cn"));
        assert!(gate_error("zcode-cn").is_none());
    }
}
