//! LobsterAI 原生 Provider。
//!
//! 推理面使用 OpenAI Chat Completions 形态，账号凭证、OAuth、续期、额度和活动奖励
//! 共用本目录的协议实现；公共转发编排仍通过 [`ProviderAdapter`] 调用，不另起账号体系。

pub mod adapter;
pub mod auth;
pub mod balance;
pub mod checkin;
pub mod credentials;
pub mod models;
pub mod oauth;
pub mod refresh;

/// 账号记录与注册表使用的稳定 provider id。
pub const PROVIDER_ID: &str = "lobsterai";
