//! Authenticated, isolated local control queries shared by quota and model discovery.
use super::{ClaudeCliError, process, verify_local_subscription};
use crate::app::ServerState;
use std::sync::atomic::AtomicBool;

pub(super) fn with_login<T>(
    state: &ServerState,
    observing: bool,
    read: impl FnOnce(process::LocalCommandConfig<'_>, u64, &AtomicBool) -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    let executable =
        emp_codex::installed_cli::resolve_claude_cli().ok_or("claude_cli_unavailable")?;
    let home = process::host_cli_home().ok_or("claude_cli_local_home_unavailable")?;
    let temp = tempfile::TempDir::new().map_err(|_| "claude_cli_temp_unavailable")?;
    let config = temp.path().join("config");
    let cache = temp.path().join("cache");
    let cwd = temp.path().join("workspace");
    for path in [&config, &cache, &cwd] {
        std::fs::create_dir(path).map_err(|_| "claude_cli_temp_unavailable")?;
    }
    let cancelled = AtomicBool::new(false);
    let login = verify_local_subscription(
        state,
        &executable,
        &home,
        &config,
        &cache,
        temp.path(),
        &cwd,
        &cancelled,
        &mut None,
    );
    let identity = match login {
        Ok(Some(identity)) => identity,
        other => {
            state.backend.claude_quota.begin(None);
            state
                .backend
                .management_events
                .publish(crate::services::management_events::Change::Quota);
            return Err(match other {
                Err(ClaudeCliError::Failure(code)) => code,
                _ => "claude_cli_login_required",
            });
        }
    };
    let generation = if observing {
        state.backend.claude_quota.begin(Some(identity))
    } else {
        state.backend.claude_quota.identify(identity)
    };
    state
        .backend
        .management_events
        .publish(crate::services::management_events::Change::Quota);
    read(
        process::LocalCommandConfig {
            executable: &executable.executable,
            child_path: &executable.child_path,
            home: &home,
            config_home: &config,
            cache_home: &cache,
            temp: temp.path(),
            working_directory: &cwd,
        },
        generation,
        &cancelled,
    )
}
