//! Native endpoint contracts grouped by transport and retry behavior.
use super::*;
use emp_transport::decode_content;
use std::sync::mpsc;

mod http;
mod incremental;
mod oversized;
mod sse;
mod support;
mod websocket;
