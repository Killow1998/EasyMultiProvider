//! Black-box fixtures for user journeys: a real EMP process started from the
//! built binary with temporary CODEX_HOME/config roots, plus an in-process
//! loopback upstream that records every request it receives.

use std::path::Path;
use std::time::Duration;

mod http;
mod process;
mod upstream;

#[allow(unused_imports)]
pub use self::{
    http::{Response, http},
    process::{Emp, Workspace, emp_command},
    upstream::{Upstream, UpstreamRequest},
};

/// Synthetic Codex login token; `/v1/*` callers present it as a bearer token.
pub const NATIVE_TOKEN: &str = "fixture-native-token";
/// Synthetic provider API key; it must never be echoed by management APIs.
pub const PROVIDER_KEY: &str = "fixture-provider-key";
pub const ORIGINAL_CODEX_CONFIG: &str = "# user settings\n[features]\nweb_search = true\n";

const IO_TIMEOUT: Duration = Duration::from_secs(15);

pub fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent directory");
    }
    std::fs::write(path, contents).expect("write fixture file");
}
