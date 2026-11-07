//! Isolated CLI invocation policy; no spawning or response parsing.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LOCAL_NETWORK_ENV: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "CLAUDE_CONFIG_DIR",
];

fn copy_local_network_environment(command: &mut Command) {
    for name in LOCAL_NETWORK_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

pub(in crate::services::claude_cli) fn host_cli_home() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    }?;
    let home = PathBuf::from(home);
    (home.is_absolute() && home.is_dir()).then_some(home)
}

pub(in crate::services::claude_cli) struct LocalCommandConfig<'a> {
    pub(in crate::services::claude_cli) executable: &'a Path,
    pub(in crate::services::claude_cli) child_path: &'a OsStr,
    pub(in crate::services::claude_cli) home: &'a Path,
    pub(in crate::services::claude_cli) config_home: &'a Path,
    pub(in crate::services::claude_cli) cache_home: &'a Path,
    pub(in crate::services::claude_cli) temp: &'a Path,
    pub(in crate::services::claude_cli) working_directory: &'a Path,
}

fn isolated_command(
    executable: &Path,
    child_path: &OsStr,
    config_home: &Path,
    cache_home: &Path,
    temp: &Path,
    working_directory: &Path,
) -> Command {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("PATH", child_path)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_CACHE_HOME", cache_home)
        .env("TMPDIR", temp)
        .env("TMP", temp)
        .env("TEMP", temp)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_TELEMETRY", "1")
        .env("CLAUDE_CODE_DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("DISABLE_UPDATES", "1")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "dumb");
    command
        .current_dir(working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000200 | 0x08000000); // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        if let Some(windir) = std::env::var_os("WINDIR") {
            command.env("WINDIR", windir);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

fn local_command(config: LocalCommandConfig<'_>) -> Command {
    let LocalCommandConfig {
        executable,
        child_path,
        home,
        config_home,
        cache_home,
        temp,
        working_directory,
    } = config;
    let mut command = isolated_command(
        executable,
        child_path,
        config_home,
        cache_home,
        temp,
        working_directory,
    );
    // Auth/control retain the CLI-owned login while excluding hooks and user prompts.
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1");
    copy_local_network_environment(&mut command);
    command.args([
        "--setting-sources",
        "project",
        "--settings",
        "{\"disableAllHooks\":true}",
    ]);
    command
}

pub(in crate::services::claude_cli) fn auth_status_command(
    config: LocalCommandConfig<'_>,
) -> Command {
    let mut command = local_command(config);
    command.args(["auth", "status", "--json"]);
    command
}

pub(in crate::services::claude_cli) fn control_command(config: LocalCommandConfig<'_>) -> Command {
    let mut command = local_command(config);
    command.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--disable-slash-commands",
        "--no-session-persistence",
    ]);
    command
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
