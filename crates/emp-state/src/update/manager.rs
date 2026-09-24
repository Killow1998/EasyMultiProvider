//! Release discovery, verified download, and handoff to the replacement worker.
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

    pub fn start(&self, operation: &str) -> Result<Snapshot> {
        let _transition = self
            .0
            .start_transition
            .lock()
            .map_err(|_| UpdateError("update_failed"))?;
        let current = self.snapshot();
        if matches!(
            current.state.as_str(),
            "checking" | "downloading" | "verifying" | "waiting" | "installing"
        ) {
            return Ok(current);
        }
        let check = match operation {
            "check" => {
                *self
                    .0
                    .asset
                    .lock()
                    .map_err(|_| UpdateError("update_failed"))? = None;
                self.set("checking", "", None, 0);
                if let Ok(mut snapshot) = self.0.snapshot.lock() {
                    snapshot.manual_update.clear();
                }
                true
            }
            "install"
                if current.supported
                    && current.manual_update.is_empty()
                    && self
                        .0
                        .asset
                        .lock()
                        .map_err(|_| UpdateError("update_failed"))?
                        .is_some() =>
            {
                self.set("downloading", "", current.latest_version, 0);
                false
            }
            _ => return Err(UpdateError("update_unavailable")),
        };
        let manager = self.clone();
        if std::thread::Builder::new()
            .name("emp-update".to_owned())
            .spawn(move || {
                let result = if check {
                    manager.check()
                } else {
                    manager.install()
                };
                if let Err(error) = result {
                    let latest = manager.snapshot().latest_version;
                    manager.set("error", error.0, latest, 0);
                }
            })
            .is_err()
        {
            let latest = self.snapshot().latest_version;
            self.set("error", "update_failed", latest, 0);
            return Err(UpdateError("update_failed"));
        }
        Ok(self.snapshot())
    }

    fn check(&self) -> Result<()> {
        let response = self.get(
            &self.0.endpoints.release_api_url,
            "application/vnd.github+json",
            Duration::from_secs(20),
            "update_failed",
        )?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            *self
                .0
                .asset
                .lock()
                .map_err(|_| UpdateError("update_failed"))? = None;
            self.set("no_release", "", None, 0);
            return Ok(());
        }
        if !response.status().is_success() {
            return Err(UpdateError("check_failed"));
        }
        let mut raw = Vec::new();
        response
            .take((MAX_RELEASE_BYTES + 1) as u64)
            .read_to_end(&mut raw)
            .map_err(|_| UpdateError("update_failed"))?;
        if raw.len() > MAX_RELEASE_BYTES {
            return Err(UpdateError("invalid_release"));
        }
        let snapshot = self.snapshot();
        let desired = current_asset_name()?;
        #[cfg(target_os = "linux")]
        let migration = snapshot.supported
            && super::linux::system_install_manual(
                &installation_target(&self.0.executable)?.0,
                &self.0.dpkg_query,
            )?;
        #[cfg(not(target_os = "linux"))]
        let migration = false;
        match latest_asset(&raw, &desired, &snapshot.current_version, &self.0.endpoints)? {
            Some(asset) => {
                let latest = asset.version.clone();
                *self
                    .0
                    .asset
                    .lock()
                    .map_err(|_| UpdateError("update_failed"))? = Some(asset);
                self.set_checked(
                    if migration { "migration_required" } else { "available" },
                    Some(latest),
                    migration,
                );
            }
            None => {
                *self
                    .0
                    .asset
                    .lock()
                    .map_err(|_| UpdateError("update_failed"))? = None;
                let version = serde_json::from_slice::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("tag_name")?
                            .as_str()
                            .map(|tag| tag.trim_start_matches('v').to_owned())
                    });
                self.set_checked(
                    if migration { "migration_required" } else { "current" },
                    version,
                    migration,
                );
            }
        }
        Ok(())
    }

    fn install(&self) -> Result<()> {
        let asset = self
            .0
            .asset
            .lock()
            .map_err(|_| UpdateError("update_failed"))?
            .clone()
            .ok_or(UpdateError("update_unavailable"))?;
        let (target, relative) = installation_target(&self.0.executable)?;
        #[cfg(target_os = "linux")]
        if super::linux::system_install_manual(&target, &self.0.dpkg_query)? {
            return Err(UpdateError("system_install_manual"));
        }
        let parent = target
            .parent()
            .ok_or(UpdateError("unsupported_installation"))?;
        if !target.exists() {
            return Err(UpdateError("directory_not_writable"));
        }
        let job = create_job(parent)?;
        let result = self.install_job(&asset, &target, &relative, &job);
        if result.is_err() {
            let _ = fs::remove_dir_all(&job);
        }
        result
    }

    fn install_job(&self, asset: &Asset, target: &Path, relative: &str, job: &Path) -> Result<()> {
        let package = job.join(&asset.name);
        download_package(self, asset, &package, |progress| {
            self.set("downloading", "", Some(asset.version.clone()), progress);
        })?;
        self.set("verifying", "", Some(asset.version.clone()), 100);
        let candidate = extract_candidate(&package, job, relative)?;
        let binary = if relative.is_empty() {
            candidate.clone()
        } else {
            candidate.join(relative)
        };
        probe_candidate_version(&binary, &asset.version, job, Duration::from_secs(30))?;

        let worker = job.join(if cfg!(windows) {
            "worker.exe"
        } else {
            "worker"
        });
        // The helper runs the protocol understood by this manager version. A separate copy also
        // lets Windows replace the original executable after its parent exits.
        fs::copy(&self.0.executable, &worker)?;
        let pid = std::process::id();
        let birth = created(pid).ok_or(UpdateError("worker_failed"))?;
        let update_nonce = nonce()?;
        let plan = json!({
            "target": target,
            "candidate": candidate,
            "relative_binary": relative,
            "parents": [{"pid": pid, "created": birth}],
            "args": self.0.restart_args,
            "version": asset.version,
            "nonce": update_nonce,
        });
        crate::filesystem::atomic_write_private_state(
            &job.join("plan.json"),
            &serde_json::to_vec(&plan).map_err(|_| UpdateError("update_failed"))?,
        )
        .map_err(|_| UpdateError("update_failed"))?;

        self.set("waiting", "", Some(asset.version.clone()), 100);
        (self.0.begin_handoff)()?;
        let mut handoff_guard = HandoffGuard {
            reopen: Arc::clone(&self.0.reopen_handoff),
            armed: true,
        };
        let mut worker_process = spawn(
            &worker,
            &[
                "--emp-apply-update".to_owned(),
                job.join("plan.json").to_string_lossy().into_owned(),
            ],
            &[],
            false,
        )?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while !job.join("worker-ready").is_file() {
            if worker_process.child.try_wait()?.is_some() || Instant::now() >= deadline {
                worker_process.stop();
                return Err(UpdateError("worker_failed"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.set("installing", "", Some(asset.version.clone()), 100);
        if let Err(error) = (self.0.handoff)() {
            worker_process.stop();
            return Err(error);
        }
        handoff_guard.armed = false;
        Ok(())
    }

    pub(super) fn open_package(&self, raw_url: &str) -> Result<Response> {
        self.get(
            raw_url,
            "application/octet-stream",
            Duration::from_secs(600),
            "update_failed",
        )
    }

    fn get(
        &self,
        raw_url: &str,
        accept: &str,
        timeout: Duration,
        transport_error: &'static str,
    ) -> Result<Response> {
        let mut url = Url::parse(raw_url).map_err(|_| UpdateError("invalid_download_url"))?;
        for redirect in 0..=MAX_REDIRECTS {
            self.validate_url(&url)?;
            let response = self
                .0
                .client
                .get(url.clone())
                .timeout(timeout)
                .header("Accept", accept)
                .send()
                .map_err(|_| UpdateError(transport_error))?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            if redirect == MAX_REDIRECTS {
                return Err(UpdateError("invalid_download_url"));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or(UpdateError("invalid_download_url"))?;
            url = response
                .url()
                .join(location)
                .map_err(|_| UpdateError("invalid_download_url"))?;
        }
        Err(UpdateError("invalid_download_url"))
    }

    fn validate_url(&self, url: &Url) -> Result<()> {
        if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
            return Err(UpdateError("invalid_download_url"));
        }
        if url.scheme() == "https" {
            let allowed = matches!(
                url.host_str(),
                Some(
                    "api.github.com"
                        | "github.com"
                        | "release-assets.githubusercontent.com"
                        | "objects.githubusercontent.com"
                )
            );
            if allowed && url.port_or_known_default() == Some(443) {
                return Ok(());
            }
        }
        if self.0.endpoints.allow_loopback_http
            && url.scheme() == "http"
            && url.host_str().is_some_and(is_loopback_host)
        {
            let repo = Url::parse(&self.0.endpoints.repository_url)
                .map_err(|_| UpdateError("invalid_download_url"))?;
            let api = Url::parse(&self.0.endpoints.release_api_url)
                .map_err(|_| UpdateError("invalid_download_url"))?;
            let same_origin = [repo, api].iter().any(|endpoint| {
                endpoint.scheme() == url.scheme()
                    && endpoint.host_str() == url.host_str()
                    && endpoint.port_or_known_default() == url.port_or_known_default()
            });
            if same_origin {
                return Ok(());
            }
        }
        Err(UpdateError("invalid_download_url"))
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

#[cfg(test)]
mod tests {
    #[cfg(any(unix, windows))]
    use super::probe_candidate_version;
    use super::{UpdateManager, nonce_with};
    use crate::update::release::UpdateEndpoints;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    fn candidate_script(root: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let binary = root.join(name);
        std::fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        binary
    }

    #[cfg(unix)]
    #[test]
    fn candidate_version_probe_accepts_only_successful_exact_output() {
        let root = tempfile::TempDir::new().unwrap();
        let valid = candidate_script(root.path(), "valid", "printf 'EMP 0.12.0\\n'");
        assert!(
            probe_candidate_version(&valid, "0.12.0", root.path(), Duration::from_secs(1)).is_ok()
        );

        let nonzero = candidate_script(root.path(), "nonzero", "printf 'EMP 0.12.0\\n'; exit 7");
        assert_eq!(
            probe_candidate_version(&nonzero, "0.12.0", root.path(), Duration::from_secs(1))
                .unwrap_err(),
            crate::update::UpdateError("version_mismatch")
        );
        assert_eq!(
            probe_candidate_version(&valid, "0.12.1", root.path(), Duration::from_secs(1))
                .unwrap_err(),
            crate::update::UpdateError("version_mismatch")
        );
    }

    #[cfg(unix)]
    #[test]
    fn candidate_version_probe_maps_spawn_failures_like_python() {
        let root = tempfile::TempDir::new().unwrap();
        assert_eq!(
            probe_candidate_version(
                &root.path().join("missing"),
                "0.12.0",
                root.path(),
                Duration::from_secs(1)
            )
            .unwrap_err(),
            crate::update::UpdateError("update_failed")
        );
        let denied = candidate_script(root.path(), "denied", "exit 0");
        std::fs::set_permissions(&denied, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        assert_eq!(
            probe_candidate_version(&denied, "0.12.0", root.path(), Duration::from_secs(1))
                .unwrap_err(),
            crate::update::UpdateError("directory_not_writable")
        );
    }

    #[cfg(unix)]
    #[test]
    fn candidate_version_probe_times_out_and_reaps_the_child() {
        let root = tempfile::TempDir::new().unwrap();
        let hanging = candidate_script(
            root.path(),
            "hanging",
            "printf '%s\\n' \"$$\"; exec sleep 5",
        );
        let started = Instant::now();
        assert_eq!(
            probe_candidate_version(&hanging, "0.12.0", root.path(), Duration::from_millis(100))
                .unwrap_err(),
            crate::update::UpdateError("update_failed")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: i32 = std::fs::read_to_string(root.path().join("candidate-version.stdout"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[cfg(windows)]
    #[test]
    fn candidate_version_probe_restores_error_mode_after_bad_image() {
        use windows_sys::Win32::System::Diagnostics::Debug::GetThreadErrorMode;

        let root = tempfile::TempDir::new().unwrap();
        let binary = root.path().join("candidate.exe");
        std::fs::write(&binary, b"deliberately invalid executable").unwrap();
        let previous = unsafe { GetThreadErrorMode() };
        assert_eq!(
            probe_candidate_version(&binary, "0.12.0", root.path(), Duration::from_secs(1))
                .unwrap_err(),
            crate::update::UpdateError("update_failed")
        );
        assert_eq!(unsafe { GetThreadErrorMode() }, previous);
    }

    #[test]
    fn concurrent_checks_start_only_one_release_job() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let release_gate = Arc::new(Barrier::new(3));
        let server_gate = Arc::clone(&release_gate);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0, "release request closed before its headers");
                request.extend_from_slice(&buffer[..count]);
            }
            server_gate.wait();
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(300);
            let mut requests = 1;
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut extra, _)) => {
                        requests += 1;
                        let mut sink = [0_u8; 512];
                        let _ = extra.read(&mut sink);
                        let _ = extra.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fake release server failed: {error}"),
                }
            }
            requests
        });
        let manager = UpdateManager::with_endpoints(
            std::env::current_exe().unwrap(),
            Vec::new(),
            env!("CARGO_PKG_VERSION"),
            UpdateEndpoints::for_source(&base, format!("{base}/latest")),
            || Err(crate::update::UpdateError("worker_failed")),
        )
        .unwrap();
        let callers_gate = Arc::new(Barrier::new(3));
        let first_manager = manager.clone();
        let first_callers_gate = Arc::clone(&callers_gate);
        let first_release_gate = Arc::clone(&release_gate);
        let first = thread::spawn(move || {
            first_callers_gate.wait();
            let snapshot = first_manager.start("check").unwrap();
            assert_eq!(snapshot.state, "checking");
            first_release_gate.wait();
        });
        let second_manager = manager.clone();
        let second_callers_gate = Arc::clone(&callers_gate);
        let second_release_gate = Arc::clone(&release_gate);
        let second = thread::spawn(move || {
            second_callers_gate.wait();
            let snapshot = second_manager.start("check").unwrap();
            assert_eq!(snapshot.state, "checking");
            second_release_gate.wait();
        });
        callers_gate.wait();
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(server.join().unwrap(), 1, "only one check request is sent");
        let deadline = Instant::now() + Duration::from_secs(3);
        while manager.snapshot().state == "checking" && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(manager.snapshot().state, "no_release");
    }

    #[test]
    fn startup_rollback_is_visible_in_the_initial_snapshot() {
        let manager = UpdateManager::with_startup_result(
            std::env::current_exe().unwrap(),
            Vec::new(),
            env!("CARGO_PKG_VERSION"),
            UpdateEndpoints::default(),
            true,
            || Err(crate::update::UpdateError("worker_failed")),
        )
        .unwrap();
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.state, "error");
        assert_eq!(snapshot.error, "install_rolled_back");
    }

    #[test]
    fn nonce_random_source_failure_is_returned() {
        let error = nonce_with(|_| Err(getrandom::Error::UNSUPPORTED)).unwrap_err();
        assert_eq!(error, crate::update::UpdateError("update_failed"));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn dpkg_owned_install_requires_manual_migration_before_any_download() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        let directory = tempfile::tempdir().unwrap();
        let query = directory.path().join("dpkg-query");
        fs::write(
            &query,
            "#!/bin/sh\nif [ \"$1\" = --search ]; then printf 'easy-multi-provider: /usr/bin/EMP\\n'; else printf 'install ok installed\\t0.11.2'; fi\n",
        )
        .unwrap();
        fs::set_permissions(&query, fs::Permissions::from_mode(0o700)).unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let release = serde_json::json!({
            "tag_name":"v9.0.0", "draft":false, "prerelease":false,
            "assets":[{"name":"EMP-linux-x86_64.tar.gz", "digest":format!("sha256:{}", "a".repeat(64)),
                "size":7, "browser_download_url":format!("{base}/releases/download/v9.0.0/EMP-linux-x86_64.tar.gz") }]
        })
        .to_string();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{release}",
                release.len()
            )
            .unwrap();
        });
        let mut manager = UpdateManager::with_endpoints(
            PathBuf::from("/usr/bin/EMP"),
            Vec::new(),
            "0.11.2",
            UpdateEndpoints::for_source(&base, format!("{base}/latest")),
            || Err(crate::update::UpdateError("worker_failed")),
        )
        .unwrap();
        Arc::get_mut(&mut manager.0).unwrap().dpkg_query = query;
        manager.check().unwrap();
        server.join().unwrap();
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.state, "migration_required");
        assert_eq!(snapshot.manual_update, "linux_system_migration");
        assert_eq!(
            manager.start("install").unwrap_err(),
            crate::update::UpdateError("update_unavailable")
        );

        // Ownership can change after a check: the installer must reject it before opening
        // the package URL or creating a replacement job.
        manager.0.snapshot.lock().unwrap().manual_update.clear();
        assert_eq!(
            manager.install(),
            Err(crate::update::UpdateError("system_install_manual"))
        );
    }
}
