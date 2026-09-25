//! Release discovery, verified download, and handoff to the replacement worker.
mod operations;
#[cfg(test)]
mod tests;

use super::download::download_package;
use super::extract::extract_candidate;
use super::process::{created, spawn};
use super::release::{Asset, MAX_RELEASE_BYTES, UpdateEndpoints, current_asset_name, latest_asset};
use super::{Result, UpdateError};
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use serde::Serialize;
use serde_json::json;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_REDIRECTS: usize = 10;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub state: String,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub progress: u8,
    pub error: String,
    pub supported: bool,
    pub manual_update: String,
    pub release_url: String,
}

#[derive(Clone)]
pub struct UpdateManager(Arc<Inner>);

struct Inner {
    snapshot: Mutex<Snapshot>,
    start_transition: Mutex<()>,
    asset: Mutex<Option<Asset>>,
    executable: PathBuf,
    #[cfg(target_os = "linux")]
    dpkg_query: PathBuf,
    restart_args: Vec<String>,
    endpoints: UpdateEndpoints,
    client: Client,
    begin_handoff: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    reopen_handoff: Arc<dyn Fn() + Send + Sync>,
    handoff: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}

pub struct UpdateHooks {
    begin_handoff: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    reopen_handoff: Arc<dyn Fn() + Send + Sync>,
    handoff: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}

impl UpdateHooks {
    pub fn new<B, R, H>(begin_handoff: B, reopen_handoff: R, handoff: H) -> Self
    where
        B: Fn() -> Result<()> + Send + Sync + 'static,
        R: Fn() + Send + Sync + 'static,
        H: Fn() -> Result<()> + Send + Sync + 'static,
    {
        Self {
            begin_handoff: Arc::new(begin_handoff),
            reopen_handoff: Arc::new(reopen_handoff),
            handoff: Arc::new(handoff),
        }
    }

    fn immediate<H>(handoff: H) -> Self
    where
        H: Fn() -> Result<()> + Send + Sync + 'static,
    {
        Self::new(|| Ok(()), || {}, handoff)
    }
}

struct HandoffGuard {
    reopen: Arc<dyn Fn() + Send + Sync>,
    armed: bool,
}

impl Drop for HandoffGuard {
    fn drop(&mut self) {
        if self.armed {
            (self.reopen)();
        }
    }
}

impl UpdateManager {
    pub fn new(
        executable: PathBuf,
        _config: PathBuf,
        restart_args: Vec<String>,
        version: &str,
    ) -> Self {
        Self::with_endpoints(
            executable,
            restart_args,
            version,
            UpdateEndpoints::default(),
            || Err(UpdateError("worker_failed")),
        )
        .expect("default update client configuration is valid")
    }

    pub fn with_endpoints<F>(
        executable: PathBuf,
        restart_args: Vec<String>,
        version: &str,
        endpoints: UpdateEndpoints,
        handoff: F,
    ) -> Result<Self>
    where
        F: Fn() -> Result<()> + Send + Sync + 'static,
    {
        Self::with_startup_result(executable, restart_args, version, endpoints, false, handoff)
    }

    pub fn with_startup_result<F>(
        executable: PathBuf,
        restart_args: Vec<String>,
        version: &str,
        endpoints: UpdateEndpoints,
        installation_rolled_back: bool,
        handoff: F,
    ) -> Result<Self>
    where
        F: Fn() -> Result<()> + Send + Sync + 'static,
    {
        Self::with_startup_hooks(
            executable,
            restart_args,
            version,
            endpoints,
            installation_rolled_back,
            UpdateHooks::immediate(handoff),
        )
    }

    pub fn with_startup_hooks(
        executable: PathBuf,
        restart_args: Vec<String>,
        version: &str,
        endpoints: UpdateEndpoints,
        installation_rolled_back: bool,
        hooks: UpdateHooks,
    ) -> Result<Self> {
        let supported = installation_target(&executable).is_ok();
        let initial = Snapshot {
            state: if installation_rolled_back {
                "error".to_owned()
            } else {
                "idle".to_owned()
            },
            current_version: version.to_owned(),
            latest_version: None,
            progress: 0,
            error: if installation_rolled_back {
                "install_rolled_back".to_owned()
            } else {
                String::new()
            },
            supported,
            manual_update: String::new(),
            release_url: format!(
                "{}/releases/latest",
                endpoints.repository_url.trim_end_matches('/')
            ),
        };
        let mut client_builder = Client::builder()
            .https_only(!endpoints.allow_loopback_http)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .user_agent(format!("EMP/{version}"));
        if endpoints.allow_loopback_http {
            // Injected local release fixtures must bypass any ambient HTTP proxy.
            client_builder = client_builder.no_proxy();
        }
        let client = client_builder
            .build()
            .map_err(|_| UpdateError("update_failed"))?;
        Ok(Self(Arc::new(Inner {
            snapshot: Mutex::new(initial),
            start_transition: Mutex::new(()),
            asset: Mutex::new(None),
            executable,
            #[cfg(target_os = "linux")]
            dpkg_query: super::linux::default_query().to_owned(),
            restart_args,
            endpoints,
            client,
            begin_handoff: hooks.begin_handoff,
            reopen_handoff: hooks.reopen_handoff,
            handoff: hooks.handoff,
        })))
    }

    pub fn snapshot(&self) -> Snapshot {
        self.0
            .snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|_| Snapshot {
                state: "error".to_owned(),
                current_version: String::new(),
                latest_version: None,
                progress: 0,
                error: "update_failed".to_owned(),
                supported: false,
                manual_update: String::new(),
                release_url: format!(
                    "{}/releases/latest",
                    self.0.endpoints.repository_url.trim_end_matches('/')
                ),
            })
    }

    fn set(&self, state: &str, error: &str, latest: Option<String>, progress: u8) {
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            snapshot.state = state.to_owned();
            snapshot.error = error.to_owned();
            snapshot.latest_version = latest;
            snapshot.progress = progress;
        }
    }

    fn set_checked(&self, state: &str, latest: Option<String>, migration: bool) {
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            snapshot.state = state.to_owned();
            snapshot.error.clear();
            snapshot.latest_version = latest;
            snapshot.progress = 0;
            snapshot.manual_update = if migration {
                "linux_system_migration".to_owned()
            } else {
                String::new()
            };
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

pub fn version_tuple(value: &str) -> Result<(u64, u64, u64)> {
    super::release::parse_version(value)
}

pub fn installation_target(executable: &Path) -> Result<(PathBuf, String)> {
    if executable.is_symlink() {
        return Err(UpdateError("unsupported_installation"));
    }
    let executable = std::path::absolute(executable)?;
    #[cfg(target_os = "macos")]
    {
        let app = executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or(UpdateError("unsupported_installation"))?;
        if app.extension().is_none_or(|extension| extension != "app")
            || executable.strip_prefix(app).ok() != Some(Path::new("Contents/Resources/EMP"))
        {
            return Err(UpdateError("unsupported_installation"));
        }
        Ok((app.to_owned(), "Contents/Resources/EMP".to_owned()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok((executable, String::new()))
    }
}

fn create_job(parent: &Path) -> Result<PathBuf> {
    for _ in 0..10 {
        let path = parent.join(format!(".emp-update-{}-{}", std::process::id(), nonce()?));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(UpdateError("update_failed"))
}

fn probe_candidate_version(
    binary: &Path,
    version: &str,
    job: &Path,
    timeout: Duration,
) -> Result<()> {
    let output = job.join("candidate-version.stdout");
    let stdout = fs::File::create(&output)?;
    let mut command = Command::new(binary);
    command
        .arg("--version")
        .env("PYINSTALLER_RESET_ENVIRONMENT", "1")
        .env_remove("EMP_UPDATE_READY")
        .env_remove("EMP_UPDATE_RESULT")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    #[cfg(windows)]
    let mut child = super::process::spawn_quiet(&mut command).map_err(|error| {
        if error == UpdateError("worker_failed") {
            UpdateError("update_failed")
        } else {
            error
        }
    })?;
    #[cfg(not(windows))]
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(UpdateError("update_failed"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
    };
    if !status.success() {
        return Err(UpdateError("version_mismatch"));
    }
    let stdout = fs::read(output)?;
    if String::from_utf8_lossy(&stdout).trim() != format!("EMP {version}") {
        return Err(UpdateError("version_mismatch"));
    }
    Ok(())
}

fn nonce() -> Result<String> {
    nonce_with(getrandom::getrandom)
}

fn nonce_with(
    fill: impl FnOnce(&mut [u8]) -> std::result::Result<(), getrandom::Error>,
) -> Result<String> {
    let mut bytes = [0_u8; 16];
    fill(&mut bytes).map_err(|_| UpdateError("update_failed"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
