//! Platform-specific messaging adapters.
//!
//! Mirrors the Python `gateway/platforms/` directory.
//! Each adapter handles send/receive for its platform.

pub mod api_server;
pub mod bluebubbles;
pub mod dingtalk;
pub mod discord;
pub mod email;
pub mod feishu;
pub mod feishu_comment;
pub mod feishu_comment_rules;
pub mod feishu_ws;
pub mod helpers;
pub mod homeassistant;
pub mod mattermost;
pub mod matrix;
pub mod signal;
pub mod slack;
pub mod sms;
pub mod telegram;
pub mod telegram_network;
pub mod webhook;
pub mod wecom;
pub mod wecom_callback;
pub mod qqbot;
pub mod weixin;
pub mod whatsapp;
pub mod whatsapp_identity;
