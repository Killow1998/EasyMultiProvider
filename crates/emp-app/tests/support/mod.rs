//! Black-box fixtures for user journeys: a real EMP process started from the
//! built binary with temporary CODEX_HOME/config roots, plus an in-process
//! loopback upstream that records every request it receives.

use emp_test_support::{TestBudget, test_timeout};
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
mod accounts;
pub(super) mod http;
#[cfg(target_os = "linux")]
mod https;
#[cfg(unix)]
pub mod modules;
mod process;
mod sse;
mod startup;
#[cfg(unix)]
#[allow(dead_code, reason = "Only the e2e target uses Team process fixtures")]
pub mod team;
mod upstream;
mod websocket;

#[cfg(target_os = "linux")]
#[allow(
    unused_imports,
    reason = "Only some integration test targets use the HTTPS fixture"
)]
pub use https::HttpsProxy;

#[allow(
    unused_imports,
    reason = "Integration test targets use different subsets of the shared fixtures"
)]
pub use self::{
    http::{Response, SseClient, http},
    process::{Emp, Workspace, emp_command},
    upstream::{Upstream, UpstreamRequest},
    websocket::{WebSocket, WebSocketUpstream},
};

#[cfg(unix)]
#[allow(unused_imports, reason = "Only the e2e target imports accounts")]
pub use accounts::import_account;

/// Synthetic Codex login token; `/v1/*` callers present it as a bearer token.
pub const NATIVE_TOKEN: &str = "fixture-native-token";
/// Synthetic provider API key; it must never be echoed by management APIs.
pub const PROVIDER_KEY: &str = "fixture-provider-key";
pub const ORIGINAL_CODEX_CONFIG: &str = "# user settings\n[features]\nweb_search = true\n";

fn io_timeout() -> TestBudget {
    test_timeout(Duration::from_secs(15))
}

pub fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent directory");
    }
    std::fs::write(path, contents).expect("write fixture file");
}
