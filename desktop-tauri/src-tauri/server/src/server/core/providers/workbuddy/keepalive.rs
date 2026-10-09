//! WorkBuddy 国际版的每日活跃保活请求构造。
//!
//! ── 这是什么 ──────────────────────────────────────────────
//! 国际版有一个「每日活跃奖励」：活动开放时领一份奖励，并且要求账号当天有
//! 一次模型调用（保持活跃）。参考客户端把「活动探测 → 条件领取 → 免费模型
//! 最小对话」接到既有签到调度器上；本仓的对应编排在
//! `billing::activity` 的 `workbuddy_daily_activity`，本文件只负责**把一发
//! 保活对话的请求素材拼出来**（URL / 头 / 体 / 代理）。
//!
//! ── 为什么保活要和真对话共用一套构造 ──────────────────────
//! 保活的本质是"让上游把这次调用记成一次正常使用"，所以它必须长得和
//! 真对话一模一样：头集合与 URL 直接复用 `adapter::chat_headers` /
//! `adapter::chat_completions_url`（两套实现必然漂移，漂移的后果是被
//! 客户端画像识别拦下）。这里只固定参考客户端使用的**最小流式 body**，
//! 并补齐国际版浏览器画像的会话头。
//!
//! ── 模型链 ────────────────────────────────────────────────
//! [`DEFAULT_FREE_MODELS`] 是参考客户端实测不消耗账号积分的免费模型，按
//! 可用性优先级排列；运行时真正使用的模型链可在签到中心自定义
//! （`billing::keepalive` 读配置），列表为空或全失败时逐个回退到下一个。

use serde_json::Value;

use crate::server::core::upstream::request::TransportRequest;

use super::adapter;

/// 国际版每日活跃任务默认使用的免费模型，按可用性优先级尝试。
///
/// 这些模型在参考客户端中实测为 x0.00，不会消耗账号积分；它是**缺省值**
/// （配置里没写 `checkinKeepalive.models` 时用它），用户自定义后的链路
/// 存在 config.json，读写见 `billing::keepalive`。列表保持集中，
/// 便于上游模型调整时只改一处。
pub const DEFAULT_FREE_MODELS: &[&str] = adapter::DAILY_ACTIVITY_FREE_MODELS;

/// 复用地区感知的每日活跃请求构造，保留 H 的地区、账号端点及代理语义。
///
/// 活跃任务必须沿用 WorkBuddy 对话链路的端点、鉴权头和账号代理；这里只固定
/// 参考客户端使用的最小流式 body（16 个 token 上限的一次 "hi"）。调用方
/// （`billing::activity::poke_daily_activity`）负责通过传输层发送并消费响应体。
pub fn build_daily_activity_request(
    session: &Value,
    model: &str,
) -> Result<TransportRequest, String> {
    adapter::build_daily_activity_request(session, model)
}
