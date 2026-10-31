//! ZCode 已知边缘拦截模板的识别与处置。
//!
//! 2026-10-25 采集的 ESA 页面同时含 `name="data-spm" content="a3c0e"`
//! 元数据和 `<title>405</title>`；匹配后保留 Terminal 与 provider 级闸门。
//! 2026-10-31 的 ALB `502 Bad Gateway` 同样是 HTML，但没有这些模板特征：
//! 不能仅凭 HTML 或网关状态码就认定为拦截，否则会误挡后续请求两分钟。
//!
//! 未知 HTML 继续走适配器的普通错误分类和既有有限重试；已知模板也只能
//! 证明响应匹配，不能断言具体封禁原因。闸门到期后放行下一次请求，不主动探测。
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
    let lowered = text.chars().take(512).collect::<String>().to_ascii_lowercase();
    lowered.contains("<html") || lowered.contains("<!doctype") || lowered.contains("<head")
}

/// 仅识别已采集的边缘拦截模板；带业务码的错误保留原分类。
pub fn is_edge_page(status: u16, code: Option<i64>, raw: &str) -> bool {
    if status < 400 || code.is_some() || !looks_like_html(raw) {
        return false;
    }
    let lowered = raw.to_ascii_lowercase();
    // shortcut: 只认已采集的 ESA 模板；新模板须有脱敏样本后再扩展，避免误挡普通网关故障。
    let known_meta = lowered
        .split('<')
        .filter_map(|part| part.strip_prefix("meta "))
        .filter_map(|part| part.split_once('>'))
        .any(|(attrs, _)| {
            attrs.contains("name=\"data-spm\"") && attrs.contains("content=\"a3c0e\"")
        });
    known_meta && lowered.contains("<title>405</title>")
}

/// 只描述匹配到的响应与本地处置，不推断具体拦截原因。
pub fn blocked_message(status: u16) -> String {
    format!(
        "收到匹配已知模板的边缘拦截页（HTTP {status}），已暂缓该通道；\
         最长 {ttl} 秒后，下一次请求可重新尝试。具体拦截原因尚不能确定，\
         持续出现请检查上游服务及网络出口",
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
                "⚠️ {provider_id} 返回匹配已知模板的边缘拦截页（HTTP {status}），选路暂时绕开这一家；\
                 最长 {} 秒后，下一次请求可重新尝试",
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
            "{} 通道暂缓中；最长 {} 秒后，下一次请求可重新尝试",
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

/// 已采集页面的脱敏回归样本，供识别与适配器测试共用。
#[cfg(test)]
pub(super) const REAL_PAGE: &str = "<!doctypehtml><html lang=\"zh-cn\"><meta charset=\"utf-8\">\
    <meta http-equiv=\"X-UA-Compatible\"content=\"IE=edge,chrome=1\">\
    <meta name=\"data-spm\"content=\"a3c0e\"><title>405</title><style>a,body,div,h</style>";

#[cfg(test)]
pub(super) const ALB_BAD_GATEWAY: &str = "<html>\r\n\
    <head><title>502 Bad Gateway</title></head>\r\n\
    <body bgcolor=\"white\">\r\n\
    <center><h1>502 Bad Gateway</h1></center>\r\n\
    <hr><center>alb</center>\r\n\
    </body>\r\n</html>\r\n";

#[cfg(test)]
mod edge_page {
    //! 涉及闸门的测试持有 [`readiness::lock_for_tests`]，避免互相干扰。

    use super::*;

    #[test]
    fn the_real_interception_page_is_recognized() {
        assert!(looks_like_html(REAL_PAGE));
        assert!(is_edge_page(405, None, REAL_PAGE));
        // 前导空白与换行不影响（实测体以 `<!doctypehtml>` 直接开头，但没有理由
        // 把带空白的变体判丢）
        assert!(is_edge_page(405, None, &format!("\n  {REAL_PAGE}")));
        // 匹配同一模板时，不依赖 HTTP 状态恰好也是 405。
        for status in [403u16, 405, 502, 503] {
            assert!(is_edge_page(status, None, REAL_PAGE), "HTTP {status}");
        }
    }

    #[test]
    fn a_gateway_page_needs_both_known_template_features() {
        assert_eq!(ALB_BAD_GATEWAY.len(), 164);
        assert!(looks_like_html(ALB_BAD_GATEWAY));
        for page in [
            ALB_BAD_GATEWAY.to_string(),
            "<html><title>405</title><body>Method Not Allowed</body></html>".into(),
            REAL_PAGE.replace("a3c0e", "other"),
            REAL_PAGE.replace("<title>405</title>", "<title>502 Bad Gateway</title>"),
            REAL_PAGE.replace("name=\"data-spm\"", "name=\"other\""),
            REAL_PAGE.replace(
                "name=\"data-spm\"content=\"a3c0e\"",
                "name=\"data-spm\"><meta content=\"a3c0e\"",
            ),
        ] {
            for status in [403, 405, 502, 503, 504] {
                assert!(!is_edge_page(status, None, &page), "HTTP {status}: {page}");
            }
        }
        assert!(is_edge_page(503, None, &REAL_PAGE.to_ascii_uppercase()));
    }

    #[test]
    fn an_html_prefix_with_multibyte_text_is_safe() {
        let page = format!("<html>{}中文</html>", "a".repeat(505));
        assert!(looks_like_html(&page));
        assert!(!is_edge_page(502, None, &page));
    }

    #[test]
    fn business_errors_are_not_edge_pages() {
        // 3012：业务层的风控 JSON —— 它有自己的文案与动作，不能当成边缘拦截
        let blocked = r#"{"code":3012,"msg":"request has been blocked due to unusual activity."}"#;
        assert!(!looks_like_html(blocked));
        assert!(!is_edge_page(405, None, blocked));
        // 哪怕体里夹着已知 HTML 模板，业务码仍然保留原分类。
        for code in [1005, 3007, 3012] {
            assert!(!is_edge_page(405, Some(code), REAL_PAGE));
        }
        // 纯文本错误（`Unauthorized` 这类）继续走普通透传
        assert!(!is_edge_page(401, None, "token expired or incorrect"));
        assert!(!is_edge_page(500, None, ""));
        // 2xx 不可能被当成拦截页（那些走 `inspect_success_head`）
        assert!(!is_edge_page(200, None, REAL_PAGE));
    }

    #[test]
    fn the_client_message_does_not_invent_a_cause() {
        let message = blocked_message(405);
        // 模板匹配不能证明端点迁移、具体拦截原因或换账号必然无效。
        assert!(!message.contains("迁移"));
        assert!(!message.contains("升级本程序"));
        assert!(!message.contains("未进入业务层"));
        assert!(!message.contains("换账号"));
        assert!(!message.contains("自动重探"));
        assert!(message.contains("匹配已知模板"));
        assert!(message.contains("HTTP 405"));
        assert!(message.contains(&format!("最长 {} 秒后", readiness::GATE_TTL_MS / 1000)));
        assert!(message.contains("下一次请求"));
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
        let error = gate_error("zcode-cn").expect("已暂缓通道应返回本地错误");
        assert_eq!(
            error.status_code,
            i32::from(readiness::LOCAL_PRECONDITION_STATUS)
        );
        assert!(error.message.contains("下一次请求"));
        assert!(!error.message.contains("自动重探"));
        // 另一家不受牵连
        assert!(!readiness::gated("zcode"));
        assert!(gate_error("zcode").is_none());
        // 复用 readiness 的寿命：到期自动放行，不需要谁去手工复位
        assert!(readiness::release("zcode-cn"));
        assert!(gate_error("zcode-cn").is_none());
    }
}
