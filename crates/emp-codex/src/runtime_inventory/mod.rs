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
pub use version::{MINIMUM_CODEX, minimum_codex, outdated_codex_client};

struct Cached {
    checked: Instant,
    value: Value,
    candidate_identities: Vec<(PathBuf, Option<ExecutableIdentity>)>,
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
        }
    }
    /// Installations admitted by the version gate share the same `config.toml`.
    /// The helper is the configured binary, else the first installation that
    /// passes that gate; this does not establish protocol/history compatibility.
    pub fn snapshot(&self, refresh: bool) -> Value {
        let mut cache = self.cache.lock().expect("runtime inventory");
        if !refresh
            && let Some(cached) = cache.as_ref()
            && cached.checked.elapsed() < Duration::from_secs(60)
        {
            return cached.value.clone();
        }
        let candidates = discovery::discover(
            &self.home,
            &self.user_home,
            self.configured.as_deref(),
            self.path_var.as_deref(),
            &self.nvm_roots,
        );
        let mut runtimes = Vec::new();
        for chunk in candidates.chunks(4) {
            let observed = std::thread::scope(|scope| {
                chunk
                    .iter()
                    .map(|candidate| scope.spawn(|| observe_candidate(candidate)))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|worker| {
                        worker
                            .join()
                            .unwrap_or_else(|_| version::public(None, "unknown"))
                    })
                    .collect::<Vec<_>>()
            });
            for (candidate, mut item) in chunk.iter().zip(observed) {
                let supported = item["status"] == "supported";
                item["source"] = json!(candidate.source);
                item["name"] = json!(candidate.name);
                item["path"] = json!(candidate.path);
                item["supported"] = json!(supported);
                item["helper"] = json!(false);
                runtimes.push(item);
            }
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
        value
            .as_object_mut()
            .unwrap()
            .retain(|key, _| matches!(key.as_str(), "installed" | "status" | "minimum" | "source"));
        if value.get("source").is_none() {
            value["source"] = json!("path_cli");
        }
        value["helper_source"] = chosen
            .map(|index| runtimes[index]["source"].clone())
            .unwrap_or(Value::Null);
        let candidate_identities = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.path.clone(),
                    ExecutableIdentity::read(&candidate.path),
                )
            })
            .collect();
        value["runtimes"] = json!(runtimes);
        *cache = Some(Cached {
            checked: Instant::now(),
            value: value.clone(),
            candidate_identities,
        });
        value
    }
    pub fn executable(&self) -> String {
        let value = self.snapshot(false);
        let selected = value["runtimes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["helper"] == true)
            .and_then(|item| item["path"].as_str());
        if self.cached_candidates_are_current() {
            match selected {
                Some(path) if trust::trusted_binary(Path::new(path)).is_some() => {
                    return path.to_owned();
                }
                Some(_) => {}
                None => return self.path_cli_fallback(),
            }
        }

        // A candidate may have been replaced or removed since the UI snapshot
        // was cached. Re-probe before handing a cached path to quota work.
        let refreshed = self.snapshot(true);
        if self.cached_candidates_are_current()
            && let Some(path) = self.selected_path(&refreshed)
            && trust::trusted_binary(Path::new(path)).is_some()
        {
            return path.to_owned();
        }
        self.path_cli_fallback()
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
            .is_some_and(|cache| {
                cache
                    .candidate_identities
                    .iter()
                    .all(|(path, identity)| ExecutableIdentity::read(path) == *identity)
            })
    }

    fn path_cli_fallback(&self) -> String {
        self.path_var
            .as_deref()
            .and_then(discovery::path_cli_in)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "codex".into())
    }
}

fn choose_helper(runtimes: &[Value]) -> Option<usize> {
    runtimes
        .iter()
        .position(|item| item["supported"] == true && item["source"] == "configured")
        .or_else(|| runtimes.iter().position(|item| item["supported"] == true))
}

fn observe_candidate(candidate: &discovery::Candidate) -> Value {
    if candidate.unavailable {
        version::public(None, "unavailable")
    } else {
        version::observe(&candidate.path)
    }
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
    fn executable_reprobes_when_a_cached_runtime_is_replaced_or_removed() {
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

        assert_eq!(inventory.executable(), binary.to_string_lossy());
        assert_eq!(inventory.snapshot(false)["helper_source"], "configured");

        let replacement = root.path().join("replacement");
        script(root.path(), "replacement", "echo codex-cli 0.148.0", 0o700);
        std::fs::rename(&replacement, &binary).unwrap();
        assert_eq!(inventory.executable(), "codex");

        script(root.path(), "replacement", "echo codex-cli 0.158.0", 0o700);
        std::fs::rename(&replacement, &binary).unwrap();
        assert_eq!(inventory.executable(), binary.to_string_lossy());

        std::fs::remove_file(&binary).unwrap();
        assert_eq!(inventory.executable(), "codex");
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
        assert_eq!(inventory.executable(), "codex");
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
            unavailable: true,
        });
        assert_eq!(observation["status"], "unavailable");
        assert_eq!(observation["installed"], serde_json::Value::Null);
        assert!(!marker.exists(), "unavailable launcher was probed");
    }

    #[test]
    #[ignore = "manual installed Codex 0.158.0 inventory probe; set EMP_CODEX_0158_BINARY"]
    fn installed_codex_0158_is_reported_supported_by_runtime_inventory() {
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
        assert_eq!(snapshot["status"], "supported");
        assert_eq!(snapshot["helper_source"], "configured");
        assert_eq!(snapshot["runtimes"][0]["helper"], true);
    }
}
