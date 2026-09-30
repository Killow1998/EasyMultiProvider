use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MAX_CHILD_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHILD_STDERR_BYTES: usize = 1024 * 1024;
#[cfg(not(test))]
const CLI_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(test)]
const CLI_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_POLL: Duration = Duration::from_millis(25);
const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(super) enum InvocationMode<'a> {
    CpaRelay {
        port: u16,
        token: &'a str,
        home: &'a Path,
    },
    LocalLogin {
        home: &'a Path,
    },
}

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

pub(super) fn host_cli_home() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    }?;
    let home = PathBuf::from(home);
    (home.is_absolute() && home.is_dir()).then_some(home)
}

pub(super) struct CommandConfig<'a> {
    pub(super) executable: &'a Path,
    pub(super) child_path: &'a OsStr,
    pub(super) model: &'a str,
    pub(super) effort: Option<&'a str>,
    pub(super) prompt_file: &'a Path,
    pub(super) schema: &'a str,
    pub(super) input_format: InputFormat,
    pub(super) mode: InvocationMode<'a>,
    pub(super) config_home: &'a Path,
    pub(super) cache_home: &'a Path,
    pub(super) temp: &'a Path,
    pub(super) working_directory: &'a Path,
}

pub(super) struct AuthStatusCommandConfig<'a> {
    pub(super) executable: &'a Path,
    pub(super) child_path: &'a OsStr,
    pub(super) home: &'a Path,
    pub(super) config_home: &'a Path,
    pub(super) cache_home: &'a Path,
    pub(super) temp: &'a Path,
    pub(super) working_directory: &'a Path,
}

#[derive(Clone, Copy)]
pub(super) enum InputFormat {
    Text,
    StreamJson,
}

impl InputFormat {
    fn input_arg(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::StreamJson => "stream-json",
        }
    }

    fn output_arg(self) -> &'static str {
        match self {
            Self::Text => "json",
            Self::StreamJson => "stream-json",
        }
    }
}

pub(super) fn command(config: CommandConfig<'_>) -> Command {
    let CommandConfig {
        executable,
        child_path,
        model,
        effort,
        prompt_file,
        schema,
        input_format,
        mode,
        config_home,
        cache_home,
        temp,
        working_directory,
    } = config;
    let output_format = input_format.output_arg();
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
    match mode {
        InvocationMode::CpaRelay { port, token, home } => {
            command
                .env("HOME", home)
                .env("USERPROFILE", home)
                .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
                .env("ANTHROPIC_AUTH_TOKEN", token)
                .env("ANTHROPIC_MODEL", model)
                .arg("--bare");
        }
        InvocationMode::LocalLogin { home } => {
            command
                .env("HOME", home)
                .env("USERPROFILE", home)
                .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1");
            copy_local_network_environment(&mut command);
        }
    }
    command.args([
        "--print",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--max-turns",
        "1",
        "--output-format",
        output_format,
        "--model",
        model,
    ]);
    if matches!(mode, InvocationMode::LocalLogin { .. }) {
        command.args([
            "--setting-sources",
            "project",
            "--settings",
            "{\"disableAllHooks\":true}",
        ]);
    }
    command.current_dir(working_directory);
    if let Some(effort) = effort {
        command.args(["--effort", effort]);
    }
    if matches!(input_format, InputFormat::StreamJson) {
        command.arg("--verbose");
    }
    command
        .args([
            "--json-schema",
            schema,
            "--input-format",
            input_format.input_arg(),
            "--system-prompt-file",
        ])
        .arg(prompt_file)
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

pub(super) fn auth_status_command(config: AuthStatusCommandConfig<'_>) -> Command {
    let AuthStatusCommandConfig {
        executable,
        child_path,
        home,
        config_home,
        cache_home,
        temp,
        working_directory,
    } = config;
    let mut command = Command::new(executable);
    // The installed CLI accepts these root settings flags before `auth
    // status`; use the same project-only source, hook suppression, empty cwd,
    // and Claude.md guard as inference while retaining the CLI-owned auth home.
    command
        .env_clear()
        .env("PATH", child_path)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_CACHE_HOME", cache_home)
        .env("TMPDIR", temp)
        .env("TMP", temp)
        .env("TEMP", temp)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_TELEMETRY", "1")
        .env("CLAUDE_CODE_DISABLE_ERROR_REPORTING", "1")
        .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("DISABLE_UPDATES", "1")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "dumb");
    copy_local_network_environment(&mut command);
    command
        .args([
            "--setting-sources",
            "project",
            "--settings",
            "{\"disableAllHooks\":true}",
            "auth",
            "status",
            "--json",
        ])
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

#[derive(Default)]
struct BoundedOutput {
    bytes: Vec<u8>,
    too_large: bool,
}

pub(super) fn run(
    command: Command,
    input: Arc<[u8]>,
    cancelled: &AtomicBool,
    cancellation_requested: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout(
        command,
        input,
        cancelled,
        CLI_TIMEOUT,
        cancellation_requested,
    )
}

pub(super) fn run_auth_status(
    command: Command,
    cancelled: &AtomicBool,
    cancellation_requested: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout_policy(
        command,
        Arc::from(&b""[..]),
        cancelled,
        AUTH_STATUS_TIMEOUT,
        true,
        cancellation_requested,
    )
}

#[derive(Clone, Copy)]
pub(super) enum CancellationReason {
    DownstreamDisconnected,
    ServerShutdown,
}

fn run_with_timeout(
    command: Command,
    input: Arc<[u8]>,
    cancelled: &AtomicBool,
    timeout: Duration,
    cancellation_requested: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout_policy(
        command,
        input,
        cancelled,
        timeout,
        false,
        cancellation_requested,
    )
}

fn run_with_timeout_policy(
    mut command: Command,
    input: Arc<[u8]>,
    cancelled: &AtomicBool,
    timeout: Duration,
    allow_nonzero_exit: bool,
    mut cancellation_requested: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    thread::scope(|scope| {
        let child = command.spawn().map_err(|_| "claude_cli_spawn_failed")?;
        let mut tree = ChildTree::new(child)?;
        let (stdout, stderr, stdin) = match (
            tree.child.stdout.take(),
            tree.child.stderr.take(),
            tree.child.stdin.take(),
        ) {
            (Some(stdout), Some(stderr), Some(stdin)) => (stdout, stderr, stdin),
            _ => {
                tree.stop();
                let _ = tree.wait();
                return Err("claude_cli_pipe_failed");
            }
        };
        let stdout_task = drain_pipe(scope, stdout, MAX_CHILD_STDOUT_BYTES);
        let stderr_task = drain_pipe(scope, stderr, MAX_CHILD_STDERR_BYTES);
        let input_task = scope.spawn(move || {
            let mut stdin = stdin;
            stdin.write_all(&input).is_ok()
        });
        let mut timed_out = false;
        let mut cancellation_reason = None;
        let deadline = Instant::now() + timeout;
        loop {
            match tree.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => {
                    cancelled.store(true, Ordering::Release);
                    break;
                }
            }
            if Instant::now() >= deadline {
                timed_out = true;
                cancelled.store(true, Ordering::Release);
                break;
            }
            if let Some(reason) = cancellation_requested() {
                cancellation_reason = Some(reason);
                cancelled.store(true, Ordering::Release);
                break;
            }
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(CHILD_POLL);
        }
        // Kill the entire process group/job before joining pipes. The CLI
        // parent may already have exited while a descendant still holds one.
        tree.stop();
        let status = tree.wait().ok();
        let input_ok = input_task.join().unwrap_or(false);
        let stdout = stdout_task.join().unwrap_or_default();
        let stderr = stderr_task.join().unwrap_or_default();
        let too_large = stdout.too_large || stderr.too_large;
        drop(stderr.bytes);
        if let Some(reason) = cancellation_reason {
            return Err(match reason {
                CancellationReason::DownstreamDisconnected => "claude_cli_disconnected",
                CancellationReason::ServerShutdown => "claude_cli_shutdown",
            });
        }
        if timed_out {
            return Err("claude_cli_timeout");
        }
        if too_large {
            return Err("claude_cli_output_too_large");
        }
        if !input_ok {
            return Err("claude_cli_stdin_failed");
        }
        if !allow_nonzero_exit && !status.is_some_and(|status| status.success()) {
            return Err("claude_cli_process_failed");
        }
        Ok(stdout.bytes)
    })
}

fn drain_pipe<'scope, R: Read + Send + 'scope>(
    scope: &'scope thread::Scope<'scope, '_>,
    mut pipe: R,
    max: usize,
) -> thread::ScopedJoinHandle<'scope, BoundedOutput> {
    scope.spawn(move || {
        let mut bytes = Vec::new();
        let mut too_large = false;
        let mut buffer = [0_u8; 8192];
        loop {
            let Ok(read) = pipe.read(&mut buffer) else {
                break;
            };
            if read == 0 {
                break;
            }
            let remaining = max.saturating_sub(bytes.len());
            bytes.extend_from_slice(&buffer[..read.min(remaining)]);
            too_large |= read > remaining;
        }
        BoundedOutput { bytes, too_large }
    })
}

struct ChildTree {
    child: Child,
    stopped: bool,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl ChildTree {
    #[allow(unused_mut)]
    fn new(mut child: Child) -> Result<Self, &'static str> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW,
            };
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() || unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0
            {
                if !job.is_null() {
                    unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err("claude_cli_process_tree_unavailable");
            }
            Ok(Self {
                child,
                stopped: false,
                job,
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {
                child,
                stopped: false,
            })
        }
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        }
        #[cfg(unix)]
        {
            thread::sleep(Duration::from_millis(100));
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }
}

impl Drop for ChildTree {
    fn drop(&mut self) {
        if !self.stopped {
            self.stop();
            let _ = self.child.wait();
        }
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_is_optional_and_model_is_configured() {
        let dir = tempfile::tempdir().expect("temp dir");
        let working_directory = dir.path().join("workspace");
        std::fs::create_dir(&working_directory).expect("private working directory");
        let prompt_file = dir.path().join("prompt");
        let command = command(CommandConfig {
            executable: Path::new("/bin/true"),
            child_path: OsStr::new("/bin"),
            model: "upstream-model",
            effort: Some("high"),
            prompt_file: &prompt_file,
            schema: "{}",
            input_format: InputFormat::StreamJson,
            mode: InvocationMode::CpaRelay {
                port: 1234,
                token: "local-only-token",
                home: dir.path(),
            },
            config_home: dir.path(),
            cache_home: dir.path(),
            temp: dir.path(),
            working_directory: &working_directory,
        });
        let args = command
            .get_args()
            .map(OsStr::to_string_lossy)
            .collect::<Vec<_>>();
        assert_eq!(command.get_current_dir(), Some(working_directory.as_path()));
        let envs = command
            .get_envs()
            .filter_map(|(key, value)| Some((key.to_string_lossy(), value?.to_string_lossy())));
        let envs = envs.collect::<Vec<_>>();
        assert!(
            envs.iter()
                .any(|(key, value)| key == "DISABLE_AUTOUPDATER" && value == "1")
        );
        assert!(
            envs.iter()
                .any(|(key, value)| key == "DISABLE_UPDATES" && value == "1")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "upstream-model"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--effort", "high"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--input-format", "stream-json"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--output-format", "stream-json"])
        );
        assert!(args.iter().any(|arg| arg == "--verbose"));
        assert!(!args.iter().any(|arg| arg == "opus" || arg == "low"));
    }

    #[test]
    fn local_login_command_uses_host_auth_without_provider_credentials_or_user_hooks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let working_directory = dir.path().join("empty-workspace");
        std::fs::create_dir(&working_directory).expect("private working directory");
        let prompt_file = dir.path().join("system-prompt.txt");
        let command = command(CommandConfig {
            executable: Path::new("/bin/true"),
            child_path: OsStr::new("/usr/bin:/bin"),
            model: "configured-alias",
            effort: None,
            prompt_file: &prompt_file,
            schema: "{}",
            input_format: InputFormat::StreamJson,
            mode: InvocationMode::LocalLogin { home: dir.path() },
            config_home: &dir.path().join("xdg-config"),
            cache_home: &dir.path().join("xdg-cache"),
            temp: dir.path(),
            working_directory: &working_directory,
        });
        let args = command
            .get_args()
            .map(OsStr::to_string_lossy)
            .collect::<Vec<_>>();
        let envs = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();
        let env_names = envs.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>();

        assert_eq!(command.get_current_dir(), Some(working_directory.as_path()));
        assert!(!args.iter().any(|arg| arg == "--bare"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "configured-alias"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--setting-sources", "project"])
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--settings", r#"{"disableAllHooks":true}"#] })
        );
        assert!(args.windows(2).any(|pair| pair == ["--tools", ""]));
        assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
        assert!(args.iter().any(|arg| arg == "--disable-slash-commands"));
        assert!(args.iter().any(|arg| arg == "--no-session-persistence"));
        assert!(args.windows(2).any(|pair| pair == ["--max-turns", "1"]));
        assert!(envs.iter().any(|(key, value)| {
            key == "CLAUDE_CODE_DISABLE_CLAUDE_MDS" && value.as_deref() == Some("1")
        }));
        assert!(LOCAL_NETWORK_ENV.contains(&"CLAUDE_CONFIG_DIR"));
        for forbidden in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "NODE_OPTIONS",
        ] {
            assert!(
                !env_names.contains(&forbidden),
                "inherited {forbidden} blocked"
            );
        }
    }

    #[test]
    fn auth_status_command_is_a_bounded_supported_status_invocation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let working_directory = dir.path().join("empty-workspace");
        std::fs::create_dir(&working_directory).expect("private working directory");
        let command = auth_status_command(AuthStatusCommandConfig {
            executable: Path::new("/bin/true"),
            child_path: OsStr::new("/usr/bin:/bin"),
            home: dir.path(),
            config_home: dir.path(),
            cache_home: dir.path(),
            temp: dir.path(),
            working_directory: &working_directory,
        });
        let args = command
            .get_args()
            .map(OsStr::to_string_lossy)
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "--setting-sources",
                "project",
                "--settings",
                r#"{"disableAllHooks":true}"#,
                "auth",
                "status",
                "--json"
            ]
        );
        assert_eq!(command.get_current_dir(), Some(working_directory.as_path()));
        let env_names = command
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for forbidden in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "NODE_OPTIONS",
        ] {
            assert!(!env_names.iter().any(|name| name == forbidden));
        }
        assert!(command.get_envs().any(|(key, value)| {
            key == OsStr::new("CLAUDE_CODE_DISABLE_CLAUDE_MDS") && value == Some(OsStr::new("1"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_disconnect_stop_the_child_process_group() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 10"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            run_with_timeout(
                command,
                Arc::from(&b"input"[..]),
                &cancelled,
                Duration::from_millis(20),
                || None
            ),
            Err("claude_cli_timeout")
        );

        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 10"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            run_with_timeout(
                command,
                Arc::from(&b"input"[..]),
                &cancelled,
                Duration::from_secs(1),
                || Some(CancellationReason::DownstreamDisconnected)
            ),
            Err("claude_cli_disconnected")
        );
    }

    #[cfg(unix)]
    #[test]
    fn normal_parent_exit_kills_descendant_holding_child_pipes_open() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("sh");
        command
            .args(["-c", "(sleep 10) & exit 0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        assert!(
            run_with_timeout(
                command,
                Arc::from(&b"input"[..]),
                &cancelled,
                Duration::from_secs(2),
                || None
            )
            .is_ok()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a descendant holding inherited pipes must be killed after normal parent exit"
        );
    }
}
