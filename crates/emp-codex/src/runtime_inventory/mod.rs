mod discovery;
pub(crate) mod launcher;
mod trust;
mod version;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Cached {
    checked: Instant,
    value: Value,
    candidate_identities: Vec<(PathBuf, Option<ExecutableIdentity>)>,
    helper: Result<SelectedHelper, HelperUnavailable>,
}

impl Cached {
    fn candidates_are_current(&self) -> bool {
        self.candidate_identities
            .iter()
            .all(|(path, identity)| ExecutableIdentity::read(path) == *identity)
    }

    fn helper(&self) -> Result<&SelectedHelper, HelperUnavailable> {
        if !self.candidates_are_current() {
            return Err(HelperUnavailable::ProbeFailed);
        }
        self.helper.as_ref().map_err(|error| *error)
    }
}

struct SelectedHelper {
    path: PathBuf,
    version: String,
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
    cache: Mutex<Option<Arc<Cached>>>,
    probe: Mutex<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HelperUnavailable {
    NotFound,
    Untrusted,
    ProbeFailed,
}

impl HelperUnavailable {
    fn from_probe(failure: &str) -> Self {
        match failure {
            "not_found" => Self::NotFound,
            "untrusted_path"
            | "writable_path"
            | "untrusted_launcher"
            | "bundle_verification_failed" => Self::Untrusted,
            _ => Self::ProbeFailed,
        }
    }
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
    pub fn snapshot(&self, refresh: bool) -> Value {
        self.snapshot_requested(refresh, Instant::now())
            .value
            .clone()
    }

    fn snapshot_requested(&self, refresh: bool, requested: Instant) -> Arc<Cached> {
        if let Some(cached) = self.reusable_snapshot(refresh, requested) {
            return cached;
        }
        let _probe = self.probe.lock().expect("runtime inventory probe");
        if let Some(cached) = self.reusable_snapshot(refresh, requested) {
            return cached;
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
        for candidate in &candidates {
            let mut item = observe_candidate(candidate);
            let available = item["status"] == "available";
            item["source"] = json!(candidate.source);
            item["name"] = json!(candidate.name);
            if item.get("path").is_none() {
                item["path"] = json!(candidate.path);
            }
            item["available"] = json!(available);
            item["running"] = json!(candidate.running);
            item["host_version"] = json!(candidate.host_version);
            item["helper"] = json!(false);
            runtimes.push(item);
        }
        let chosen = choose_helper(&runtimes);
        let helper = chosen.map_or_else(
            || {
                Err(runtimes
                    .iter()
                    .filter_map(|item| item["failure"].as_str())
                    .map(HelperUnavailable::from_probe)
                    .max()
                    .unwrap_or(HelperUnavailable::NotFound))
            },
            |index| {
                let candidate = &runtimes[index];
                Ok(SelectedHelper {
                    path: PathBuf::from(candidate["path"].as_str().expect("observed path")),
                    version: candidate["installed"]
                        .as_str()
                        .expect("observed version")
                        .to_owned(),
                })
            },
        );
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
        let cached = Arc::new(Cached {
            checked: Instant::now(),
            value,
            candidate_identities,
            helper,
        });
        *self.cache.lock().expect("runtime inventory") = Some(Arc::clone(&cached));
        cached
    }

    fn reusable_snapshot(&self, refresh: bool, requested: Instant) -> Option<Arc<Cached>> {
        let cache = self.cache.lock().expect("runtime inventory");
        let cached = cache.as_ref()?;
        (cached.checked >= requested
            || (!refresh && cached.checked.elapsed() < Duration::from_secs(60)))
        .then(|| Arc::clone(cached))
    }

    pub fn prepared_executable(&self) -> Result<crate::PreparedExecutable, HelperUnavailable> {
        let requested = Instant::now();
        let snapshot = self.snapshot_requested(false, requested);
        if let Ok(helper) = snapshot.helper()
            && let Ok(prepared) = crate::PreparedExecutable::prepare(&helper.path)
        {
            return Ok(prepared);
        }
        let refreshed = self.snapshot_requested(true, requested);
        crate::PreparedExecutable::prepare(&refreshed.helper()?.path)
            .map_err(|error| HelperUnavailable::from_probe(version::trust_failure(error)))
    }

    pub fn observe_selected_version(&self) -> Option<String> {
        let snapshot = self.snapshot_requested(false, Instant::now());
        Some(snapshot.helper().ok()?.version.clone())
    }

    pub fn selected_trusted_version(&self) -> Option<String> {
        let cached = self.cache.lock().ok()?.clone()?;
        Some(cached.helper().ok()?.version.clone())
    }
}

fn choose_helper(runtimes: &[Value]) -> Option<usize> {
    runtimes
        .iter()
        .position(|item| item["available"] == true && item["source"] == "configured")
        .or_else(|| {
            runtimes
                .iter()
                .enumerate()
                .filter(|(_, item)| item["available"] == true)
                .max_by_key(|(index, item)| {
                    (
                        item["running"] == true,
                        item["installed"]
                            .as_str()
                            .and_then(|version| semver::Version::parse(version).ok()),
                        std::cmp::Reverse(*index),
                    )
                })
                .map(|(index, _)| index)
        })
}

fn observe_candidate(candidate: &discovery::Candidate) -> Value {
    if candidate.unavailable {
        return version::failed("not_found");
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
mod tests;
