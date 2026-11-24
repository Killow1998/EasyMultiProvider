pub(crate) mod accounts;
pub(crate) mod aero;
pub(crate) mod catalog;
pub(crate) mod compact;
pub(crate) mod decisions;
pub(crate) mod gateway;
pub(crate) mod images;
pub(crate) mod inspection;
pub(crate) mod integration;
pub(crate) mod lifecycle;
pub(crate) mod modules;
pub(crate) mod provider_quota;
pub(crate) mod quota;
pub(crate) mod realtime;
pub(crate) mod responses;
pub(crate) mod search;
pub(crate) mod streaming;
pub(crate) mod support_report;
pub(crate) mod team;
pub(crate) mod websocket;

pub(crate) mod runtime;

pub(crate) mod calls;
pub(crate) mod diagnostics;
pub(crate) mod updates;
pub(crate) mod usage;

mod claude_response;
mod claude_stream;
mod failure_response;
pub(crate) use failure_response::router_error_reply;
mod history_response;
mod native_response;
