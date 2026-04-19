//! Platform-specific messaging adapters.
//!
//! Mirrors the Python `gateway/platforms/` directory.
//! Each adapter handles send/receive for its platform.

pub mod api_server;
pub mod dingtalk;
pub mod discord;
pub mod email;
pub mod feishu;
pub mod feishu_ws;
pub mod helpers;
pub mod slack;
pub mod telegram;
pub mod webhook;
pub mod wecom;
pub mod qqbot;
pub mod weixin;
pub mod whatsapp;
