//! MiniMax Code 原生 Provider。
//!
//! 上游推理接口是 Anthropic Messages 形态；账号、OAuth 续期、额度和每日签到
//! 均使用同一份 access/refresh token。所有网络请求仍由既有上游编排层发出，
//! 本目录只提供协议适配与账号级管理接口。

pub mod adapter;
pub mod auth;
pub mod balance;
pub mod checkin;
pub mod credentials;
pub mod models;
pub mod oauth;
pub mod refresh;

pub const PROVIDER_ID: &str = "minimax-code";

pub use adapter::MINIMAX_CODE_ADAPTER;
