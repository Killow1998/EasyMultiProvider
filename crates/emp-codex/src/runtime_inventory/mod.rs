//! Cached installation inventory and independent helper/target selection.
mod discovery;
mod trust;
mod version;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
pub use version::{MINIMUM_CODEX, minimum_codex, outdated_codex_client};

struct Cached {
    checked: Instant,
    value: Value,
}
pub struct RuntimeInventory {
    home: PathBuf,
    user_home: PathBuf,
    configured: Option<PathBuf>,
    path_cli: Mutex<Option<PathBuf>>,
    cache: Mutex<Option<Cached>>,
}
impl RuntimeInventory {
    pub fn new(home: PathBuf, configured: Option<PathBuf>) -> Self {
        Self {
            home,
            configured,
            user_home: PathBuf::from(
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .unwrap_or_default(),
            ),
            path_cli: Mutex::new(discovery::path_cli()),
            cache: Mutex::new(None),
        }
    }
    /// Every supported installation reads the same `config.toml`, so there is
    /// nothing to choose: the helper is the configured binary, else the first
    /// supported one found.
    pub fn snapshot(&self, refresh: bool) -> Value {
        let mut cache = self.cache.lock().expect("runtime inventory");
        if !refresh
            && let Some(cached) = cache.as_ref()
            && cached.checked.elapsed() < Duration::from_secs(60)
        {
            return cached.value.clone();
        }
        let mut fallback = self.path_cli.lock().expect("runtime PATH selection");
        if let Some(path) = discovery::path_cli() {
            *fallback = Some(path);
        }
        let candidates = discovery::discover(
            &self.home,
            &self.user_home,
            self.configured.as_deref(),
            fallback.as_deref(),
        );
        drop(fallback);
        let mut runtimes = Vec::new();
        for chunk in candidates.chunks(4) {
            let observed = std::thread::scope(|scope| {
                chunk
                    .iter()
                    .map(|candidate| scope.spawn(|| version::observe(&candidate.path)))
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
        let chosen = runtimes
            .iter()
            .position(|item| item["supported"] == true && item["source"] == "configured")
            .or_else(|| runtimes.iter().position(|item| item["supported"] == true));
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
        value["runtimes"] = json!(runtimes);
        *cache = Some(Cached {
            checked: Instant::now(),
            value: value.clone(),
        });
        value
    }
    pub fn executable(&self) -> String {
        let value = self.snapshot(false);
        value["runtimes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["helper"] == true)
            .and_then(|item| item["path"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                self.configured
                    .as_deref()
                    .map(Path::to_path_buf)
                    .or_else(|| self.path_cli.lock().ok()?.clone())
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "codex".into())
    }
}
