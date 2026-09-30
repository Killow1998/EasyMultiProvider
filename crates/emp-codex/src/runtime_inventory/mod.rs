//! Cached installation inventory and independent helper/target selection.
mod discovery;
pub(crate) mod launcher;
mod trust;
mod version;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Cached {
    checked: Instant,
    value: Value,
    candidate_identities: Vec<(PathBuf, Option<ExecutableIdentity>)>,
}

impl Cached {
    fn candidates_are_current(&self) -> bool {
        self.candidate_identities
            .iter()
            .all(|(path, identity)| ExecutableIdentity::read(path) == *identity)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutableIdentity {
    canonical: PathBuf,
    length: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    owner: u32,
    #[cfg(unix)]
    group: u32,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl ExecutableIdentity {
    fn read(path: &Path) -> Option<Self> {
        let canonical = path.canonicalize().ok()?;
        let metadata = fs::metadata(&canonical).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                canonical,
                length: metadata.len(),
                modified: metadata.modified().ok(),
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                owner: metadata.uid(),
                group: metadata.gid(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
            })
        }
        #[cfg(not(unix))]
        {
            Some(Self {
                canonical,
                length: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }
}

pub struct RuntimeInventory {
    home: PathBuf,
    user_home: PathBuf,
    configured: Option<PathBuf>,
    path_var: Option<OsString>,
    nvm_roots: Vec<PathBuf>,
    cache: Mutex<Option<Cached>>,
    /// Serialize slow observations without blocking cached-value readers.
    probe: Mutex<()>,
}
impl RuntimeInventory {
    pub fn new(home: PathBuf, configured: Option<PathBuf>) -> Self {
        let user_home = PathBuf::from(
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .unwrap_or_default(),
        );
        let nvm_dir = std::env::var_os("NVM_DIR");
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        Self::with_discovery(
            home,
            user_home.clone(),
            configured,
            std::env::var_os("PATH"),
            discovery::nvm_roots(&user_home, nvm_dir.as_deref(), xdg_config_home.as_deref()),
        )
    }

    fn with_discovery(
        home: PathBuf,
        user_home: PathBuf,
        configured: Option<PathBuf>,
        path_var: Option<OsString>,
        nvm_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            home,
            user_home,
            configured,
            path_var,
            nvm_roots,
            cache: Mutex::new(None),
            probe: Mutex::new(()),
        }
    }
    /// Observe executable engines without claiming protocol compatibility.
    /// The helper prefers the configured engine, then the first available engine.
    pub fn snapshot(&self, refresh: bool) -> Value {
        self.snapshot_requested(refresh, Instant::now())
    }

    fn snapshot_requested(&self, refresh: bool, requested: Instant) -> Value {
        if let Some(value) = self.reusable_snapshot(refresh, requested) {
            return value;
        }
        let _probe = self.probe.lock().expect("runtime inventory probe");
        // A refresh completed while we waited: even concurrent forced
        // requests share that observation rather than queueing more probes.
        if let Some(value) = self.reusable_snapshot(refresh, requested) {
            return value;
        }
        let candidates = discovery::discover(
            &self.home,
            &self.user_home,
            self.configured.as_deref(),
            self.path_var.as_deref(),
            &self.nvm_roots,
        );
        let candidate_identities = candidates
            .iter()
            .flat_map(|candidate| {
                std::iter::once(&candidate.path).chain(candidate.fallbacks.iter())
            })
            .map(|path| (path.clone(), ExecutableIdentity::read(path)))
            .collect::<Vec<_>>();
        let mut runtimes = Vec::new();
        // One observation at a time, including any private bundle copies.
        // The old cache remains readable throughout discovery and probing.
        for candidate in &candidates {
            let mut item = observe_candidate(candidate);
            let available = item["status"] == "available";
            item["source"] = json!(candidate.source);
            item["name"] = json!(candidate.name);
            if item.get("path").is_none() {
                item["path"] = json!(candidate.path);
            }
            item["available"] = json!(available);
            item["host_version"] = json!(candidate.host_version);
            item["helper"] = json!(false);
            runtimes.push(item);
        }
        let chosen = choose_helper(&runtimes);
        let mut value = if let Some(index) = chosen {
            runtimes[index]["helper"] = json!(true);
            runtimes[index].clone()
        } else {
            runtimes
                .first()
                .cloned()
                .unwrap_or_else(|| version::public(None, "unavailable"))
        };
        value.as_object_mut().unwrap().retain(|key, _| {
            matches!(
                key.as_str(),
                "installed" | "status" | "source" | "host_version"
            )
        });
        if value.get("source").is_none() {
            value["source"] = json!("path_cli");
        }
        value["helper_source"] = chosen
            .map(|index| runtimes[index]["source"].clone())
            .unwrap_or(Value::Null);
        value["runtimes"] = json!(runtimes);
        *self.cache.lock().expect("runtime inventory") = Some(Cached {
            checked: Instant::now(),
            value: value.clone(),
            candidate_identities,
        });
        value
    }

    fn reusable_snapshot(&self, refresh: bool, requested: Instant) -> Option<Value> {
        let cache = self.cache.lock().expect("runtime inventory");
        let cached = cache.as_ref()?;
        (cached.checked >= requested
            || (!refresh && cached.checked.elapsed() < Duration::from_secs(60)))
        .then(|| cached.value.clone())
    }
    /// Own a launchable helper, including any verified temporary Mac bundle.
    /// Never turn a signature check into permission to execute its shared
    /// source path: callers must retain this owner through the operation.
    pub fn prepared_executable(&self) -> Option<crate::PreparedExecutable> {
        let snapshot = self.snapshot(false);
        if self.cached_candidates_are_current()
            && let Some(path) = self.selected_path(&snapshot)
            && let Ok(prepared) = crate::PreparedExecutable::prepare(Path::new(path))
        {
            return Some(prepared);
        }
        let refreshed = self.snapshot(true);
        if !self.cached_candidates_are_current() {
            return None;
        }
        crate::PreparedExecutable::prepare(Path::new(self.selected_path(&refreshed)?)).ok()
    }

    /// Read an already verified observation after checking source identities.
    /// Never launch, copy, or verify signatures here: catalog persistence may
    /// call this while holding its configuration mutex. A cold/changed cache
    /// returns None; the ordinary inventory refresh populates observations.
    pub fn selected_trusted_version(&self) -> Option<String> {
        let cached = self.cache.lock().ok()?;
        let cached = cached.as_ref()?;
        if !cached.candidates_are_current() {
            return None;
        }
        let selected = cached.value["runtimes"]
            .as_array()?
            .iter()
            .find(|runtime| runtime["helper"].as_bool() == Some(true))?;
        if selected["available"].as_bool() != Some(true)
            || selected["status"].as_str() != Some("available")
        {
            return None;
        }
        let version = selected["installed"].as_str()?;
        Some(version.to_owned())
    }

    fn selected_path<'a>(&self, value: &'a Value) -> Option<&'a str> {
        value["runtimes"]
            .as_array()
            .and_then(|runtimes| runtimes.iter().find(|item| item["helper"] == true))
            .and_then(|helper| helper["path"].as_str())
    }

    fn cached_candidates_are_current(&self) -> bool {
        self.cache
            .lock()
            .expect("runtime inventory")
            .as_ref()
            .is_some_and(Cached::candidates_are_current)
    }
}

fn choose_helper(runtimes: &[Value]) -> Option<usize> {
    runtimes
        .iter()
        .position(|item| item["available"] == true && item["source"] == "configured")
        .or_else(|| runtimes.iter().position(|item| item["available"] == true))
}

fn observe_candidate(candidate: &discovery::Candidate) -> Value {
    if candidate.unavailable {
        return version::public(None, "unavailable");
    }
    let mut observation = version::observe(&candidate.path);
    if observation["status"] != "available" {
        for path in &candidate.fallbacks {
            if !discovery::verified_fallback(candidate, path) {
                continue;
            }
            let fallback = version::observe(path);
            if fallback["status"] == "available" {
                observation = fallback;
                observation["path"] = json!(path);
                break;
            }
        }
    }
    observation
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        RuntimeInventory,
        trust::tests::{private_dir, script},
    };
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    #[test]
    fn prepared_executable_reprobes_when_a_cached_runtime_is_replaced_or_removed() {
        let root = private_dir();
        let codex_home = root.path().join("codex-home");
        let user_home = root.path().join("user-home");
        std::fs::create_dir(&codex_home).unwrap();
        std::fs::create_dir(&user_home).unwrap();
        let binary = script(root.path(), "codex", "echo codex-cli 0.158.0", 0o700);
        let inventory = RuntimeInventory::with_discovery(
            codex_home,
            user_home,
            Some(binary.clone()),
            Some(OsString::new()),
            Vec::new(),
        );

        assert_eq!(inventory.prepared_executable().unwrap().path(), binary);
        assert_eq!(inventory.snapshot(false)["helper_source"], "configured");

        let replacement = root.path().join("replacement");
        script(root.path(), "replacement", "echo codex-cli 0.148.0", 0o700);
        std::fs::rename(&replacement, &binary).unwrap();
        assert_eq!(inventory.prepared_executable().unwrap().path(), binary);
        assert_eq!(
            inventory.selected_trusted_version().as_deref(),
            Some("0.148.0")
        );

        script(root.path(), "replacement", "echo codex-cli 0.158.0", 0o700);
        std::fs::rename(&replacement, &binary).unwrap();
        assert_eq!(inventory.prepared_executable().unwrap().path(), binary);

        std::fs::remove_file(&binary).unwrap();
        assert!(inventory.prepared_executable().is_none());
    }

    #[test]
    fn constructor_context_does_not_need_a_live_path_or_nvm_install() {
        let root = private_dir();
        let home = root.path().join("home");
        let user_home = root.path().join("user");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&user_home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let inventory = RuntimeInventory::with_discovery(
            home,
            user_home,
            None,
            Some(OsString::new()),
            Vec::new(),
        );
        assert!(inventory.prepared_executable().is_none());
    }

    #[test]
    fn cached_version_does_not_wait_for_or_repeat_a_delayed_refresh() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let root = private_dir();
        let home = root.path().join("home");
        let user_home = root.path().join("user");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&user_home).unwrap();
        let calls = root.path().join("calls");
        let delay = root.path().join("delay");
        let entered = root.path().join("entered");
        let release = root.path().join("release");
        let body = format!(
            "echo called >> '{}'\nif [ -f '{}' ]; then\n: > '{}'\nwhile [ ! -f '{}' ]; do /bin/sleep 0.01; done\nfi\necho codex-cli 0.158.0",
            calls.display(),
            delay.display(),
            entered.display(),
            release.display()
        );
        let binary = script(root.path(), "codex", &body, 0o700);
        let inventory = RuntimeInventory::with_discovery(
            home,
            user_home,
            Some(binary),
            Some(OsString::new()),
            Vec::new(),
        );
        inventory.snapshot(false);
        assert_eq!(
            inventory.selected_trusted_version().as_deref(),
            Some("0.158.0")
        );
        std::fs::write(&delay, b"").unwrap();

        let inventory = &inventory;
        let read = std::thread::scope(|scope| {
            let refreshing = scope.spawn(|| inventory.snapshot(true));
            let deadline = Instant::now() + Duration::from_secs(1);
            while !entered.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(entered.exists(), "delayed probe did not start");
            // Capture receipt before releasing the first probe. The queued
            // forced request must share the observation even if scheduled late.
            let requested = Instant::now();
            let queued = scope.spawn(move || inventory.snapshot_requested(true, requested));
            let (sender, receiver) = mpsc::channel();
            scope.spawn(move || sender.send(inventory.selected_trusted_version()).unwrap());
            let read = receiver.recv_timeout(Duration::from_millis(500));
            // Always release/reap children, including on the regression path
            // where a cache reader times out behind the old broad mutex.
            std::fs::write(&release, b"").unwrap();
            assert_eq!(refreshing.join().unwrap()["installed"], "0.158.0");
            assert_eq!(queued.join().unwrap()["installed"], "0.158.0");
            read
        });
        assert_eq!(read.unwrap().as_deref(), Some("0.158.0"));
        assert_eq!(
            std::fs::read_to_string(calls).unwrap().lines().count(),
            2,
            "one initial probe and one coalesced refresh; cached reads never probe"
        );
    }

    #[test]
    fn selected_trusted_version_never_probes_a_cold_stale_or_changed_cache() {
        let root = tempfile::Builder::new()
            .prefix("emp-inventory-version-")
            .tempdir()
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.path().join("codex-home");
        let user_home = root.path().join("user-home");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&user_home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&user_home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let version_calls = root.path().join("version-calls");
        let app_server_calls = root.path().join("app-server-calls");
        let body = format!(
            "if [ \"$1\" = \"--version\" ]; then echo called >> '{}'; echo 'codex-cli 0.155.0-alpha.9.2'; else echo called >> '{}'; fi",
            version_calls.display(),
            app_server_calls.display()
        );
        let binary = script(root.path(), "codex", &body, 0o700);
        assert_eq!(
            super::trust::trusted_binary(&binary),
            Some(binary.canonicalize().unwrap()),
            "test runtime must satisfy the unchanged executable trust policy"
        );
        let inventory = RuntimeInventory::with_discovery(
            home,
            user_home,
            Some(binary.clone()),
            Some(OsString::new()),
            Vec::new(),
        );

        assert_eq!(inventory.selected_trusted_version(), None);
        assert!(!version_calls.exists(), "cold read must not launch a probe");
        inventory.snapshot(false);
        assert_eq!(
            inventory.selected_trusted_version().as_deref(),
            Some("0.155.0-alpha.9.2")
        );
        inventory.cache.lock().unwrap().as_mut().unwrap().checked =
            std::time::Instant::now() - std::time::Duration::from_secs(120);
        assert_eq!(
            inventory.selected_trusted_version().as_deref(),
            Some("0.155.0-alpha.9.2")
        );
        // Simulate a cached observation made with a different owner without
        // requiring privilege to chown test fixtures to another account.
        {
            let mut cache = inventory.cache.lock().unwrap();
            let identity = cache.as_mut().unwrap().candidate_identities[0]
                .1
                .as_mut()
                .unwrap();
            identity.owner = identity.owner.wrapping_add(1);
        }
        assert_eq!(inventory.selected_trusted_version(), None);
        {
            let mut cache = inventory.cache.lock().unwrap();
            cache.as_mut().unwrap().candidate_identities[0].1 =
                super::ExecutableIdentity::read(&binary);
        }
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(inventory.selected_trusted_version(), None);
        assert_eq!(
            std::fs::read_to_string(&version_calls)
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert!(!app_server_calls.exists());
    }

    #[test]
    fn selected_trusted_version_is_absent_when_no_version_was_observed() {
        let root = tempfile::Builder::new()
            .prefix("emp-inventory-version-")
            .tempdir()
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.path().join("codex-home");
        let user_home = root.path().join("user-home");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&user_home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&user_home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let binary = script(root.path(), "codex", "echo 'unknown'", 0o700);
        let inventory = RuntimeInventory::with_discovery(
            home,
            user_home,
            Some(binary),
            Some(OsString::new()),
            Vec::new(),
        );

        assert_eq!(inventory.selected_trusted_version(), None);
    }

    #[test]
    fn unavailable_configured_launcher_is_reported_without_probing_it() {
        use super::{discovery::Candidate, observe_candidate};

        let root = private_dir();
        let marker = root.path().join("launcher-was-executed");
        let launcher = script(
            root.path(),
            "codex.cmd",
            &format!("touch '{}'", marker.display()),
            0o700,
        );
        let observation = observe_candidate(&Candidate {
            source: "configured",
            name: "Configured Codex",
            path: launcher,
            host_version: None,
            fallbacks: Vec::new(),
            unavailable: true,
        });
        assert_eq!(observation["status"], "unavailable");
        assert_eq!(observation["installed"], serde_json::Value::Null);
        assert!(!marker.exists(), "unavailable launcher was probed");
    }

    #[test]
    #[ignore = "manual installed Codex 0.158.0 inventory probe; set EMP_CODEX_0158_BINARY"]
    fn installed_codex_0158_is_reported_available_by_runtime_inventory() {
        let binary = std::env::var_os("EMP_CODEX_0158_BINARY")
            .expect("EMP_CODEX_0158_BINARY must point to the installed 0.158.0 runtime");
        let home = PathBuf::from(std::env::var_os("CODEX_HOME").expect("disposable CODEX_HOME"));
        let user_home = PathBuf::from(std::env::var_os("HOME").expect("disposable HOME"));
        let inventory = RuntimeInventory::with_discovery(
            home,
            user_home,
            Some(PathBuf::from(binary)),
            Some(OsString::new()),
            Vec::new(),
        );

        let snapshot = inventory.snapshot(true);
        assert_eq!(snapshot["installed"], "0.158.0");
        assert_eq!(snapshot["status"], "available");
        assert_eq!(snapshot["helper_source"], "configured");
        assert_eq!(snapshot["runtimes"][0]["helper"], true);
    }
}
