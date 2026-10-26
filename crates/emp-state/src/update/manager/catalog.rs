//! Last-good release metadata: local first, conditionally refreshed from its source.
use super::*;
use crate::update::release::{normalize_current_version, parse_version};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct CachedRelease {
    source: String,
    etag: Option<String>,
    release: serde_json::Value,
    #[serde(default)]
    history: Vec<Value>,
    #[serde(default)]
    history_has_more: bool,
}

fn stable_releases(releases: &[Value]) -> Vec<&Value> {
    let mut stable = releases
        .iter()
        .filter(|release| release["draft"] == false && release["prerelease"] == false)
        .filter_map(|release| Some((parse_version(release["tag_name"].as_str()?).ok()?, release)))
        .collect::<Vec<_>>();
    stable.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
    stable.dedup_by_key(|(version, _)| *version);
    stable.into_iter().map(|(_, release)| release).collect()
}

fn notes(release: &Value) -> String {
    release["body"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(65536)
        .collect()
}

fn history_notes(releases: &[&Value]) -> Vec<Value> {
    releases
        .iter()
        .map(|release| json!({"version":release["tag_name"], "body":notes(release)}))
        .collect()
}

impl CachedRelease {
    fn from_feed(source: String, etag: Option<String>, feed: Value) -> Result<Self> {
        let Some(releases) = feed.as_array() else {
            return Ok(Self {
                source,
                etag,
                release: feed,
                history: Vec::new(),
                history_has_more: false,
            });
        };
        let stable = stable_releases(releases);
        let release = (*stable.first().ok_or(UpdateError("invalid_release"))?).clone();
        Ok(Self {
            source,
            etag,
            release,
            history: history_notes(&stable),
            history_has_more: releases.len() >= 100,
        })
    }

    pub(super) fn history(&self, current: &str) -> Vec<Value> {
        let current = normalize_current_version(current).ok();
        let latest = self.version().and_then(|value| parse_version(&value).ok());
        let notes = self
            .history
            .iter()
            .filter(|note| {
                let version = note["version"]
                    .as_str()
                    .and_then(|value| parse_version(value).ok());
                version.is_some_and(|version| Some(version) > current && Some(version) <= latest)
            })
            .cloned()
            .collect::<Vec<_>>();
        if notes.is_empty() {
            vec![json!({"version":self.version(),"body":self.notes()})]
        } else {
            notes
        }
    }

    pub(super) fn history_complete(&self, current: &str) -> bool {
        !self.history_has_more
            || self.history.iter().any(|note| {
                note["version"]
                    .as_str()
                    .and_then(|value| parse_version(value).ok())
                    .is_some_and(|version| Some(version) <= normalize_current_version(current).ok())
            })
    }

    pub(super) fn notes(&self) -> String {
        notes(&self.release)
    }

    pub(super) fn version(&self) -> Option<String> {
        self.release
            .get("tag_name")
            .and_then(|tag| tag.as_str())
            .map(|tag| tag.trim_start_matches('v').to_owned())
    }
}

pub(super) fn load(
    diagnostic_path: Option<&Path>,
    endpoints: &UpdateEndpoints,
    current_version: &str,
) -> Option<CachedRelease> {
    let path = diagnostic_path?.with_file_name("update-release.json");
    let raw = crate::read_file_limited(&path, MAX_RELEASE_BYTES + 4096).ok()?;
    let mut cached: CachedRelease = serde_json::from_slice(&raw).ok()?;
    if cached.source == "https://api.github.com/repos/Killow1998/EasyMultiProvider/releases/latest"
        && endpoints.release_api_url == crate::update::release::RELEASE_API_URL
    {
        cached.source = endpoints.release_api_url.clone();
        cached.etag = None;
    }
    if cached.source != endpoints.release_api_url {
        return None;
    }
    let raw = serde_json::to_vec(&cached.release).ok()?;
    if raw.len() > MAX_RELEASE_BYTES {
        return None;
    }
    latest_asset(
        &raw,
        &current_asset_name().ok()?,
        current_version,
        endpoints,
    )
    .ok()?;
    Some(cached)
}

impl UpdateManager {
    fn store_release_cache(&self, cached: Option<CachedRelease>) {
        if let Some(path) = self
            .0
            .diagnostic_path
            .as_deref()
            .map(|path| path.with_file_name("update-release.json"))
        {
            // A cache write failure must not discard the successful network result.
            if let Some(cached) = &cached {
                if let Ok(raw) = serde_json::to_vec(cached)
                    && raw.len() <= MAX_RELEASE_BYTES
                {
                    let _ = crate::atomic_write_private_state(&path, &raw);
                }
            } else {
                let _ = fs::remove_file(path);
            }
        }
        if let Ok(mut stored) = self.0.release_cache.lock() {
            *stored = cached;
        }
    }

    pub(super) fn check(&self) -> Result<()> {
        self.retry_network(|| {
            self.stage("check_release");
            self.check_once()
        })
    }

    fn check_once(&self) -> Result<()> {
        let cached = self
            .0
            .release_cache
            .lock()
            .ok()
            .and_then(|value| value.clone());
        let response = self.get(
            &self.0.endpoints.release_api_url,
            "application/vnd.github+json",
            Duration::from_secs(20),
            "update_failed",
            cached.as_ref().and_then(|value| value.etag.as_deref()),
        )?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return self.clear_release();
        }
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            let mut cached = cached.ok_or(UpdateError("invalid_release"))?;
            let before = (cached.history.len(), cached.history_has_more);
            self.complete_history(&mut cached);
            let changed = before != (cached.history.len(), cached.history_has_more);
            return self.apply_release(&cached, changed);
        }
        if !response.status().is_success() {
            self.http_error(response.status().as_u16());
            return Err(UpdateError("check_failed"));
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 1024)
            .map(str::to_owned);
        self.stage("read_release");
        let mut raw = Vec::new();
        response
            .take((MAX_RELEASE_BYTES + 1) as u64)
            .read_to_end(&mut raw)
            .map_err(|error| {
                self.io_error("read_release", error);
                UpdateError("update_failed")
            })?;
        if raw.len() > MAX_RELEASE_BYTES {
            return Err(UpdateError("invalid_release"));
        }
        let release: Value =
            serde_json::from_slice(&raw).map_err(|_| UpdateError("update_failed"))?;
        if release.as_array().is_some_and(Vec::is_empty) {
            return self.clear_release();
        }
        let mut cached =
            CachedRelease::from_feed(self.0.endpoints.release_api_url.clone(), etag, release)?;
        self.complete_history(&mut cached);
        self.apply_release(&cached, true)
    }

    fn clear_release(&self) -> Result<()> {
        *self
            .0
            .asset
            .lock()
            .map_err(|_| UpdateError("update_failed"))? = None;
        self.store_release_cache(None);
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            snapshot.release_notes.clear();
            snapshot.release_history.clear();
            snapshot.release_history_complete = true;
        }
        self.set("no_release", "", None, 0);
        Ok(())
    }

    fn complete_history(&self, cached: &mut CachedRelease) {
        let current = self.snapshot().current_version;
        let Ok(mut url) = Url::parse(&self.0.endpoints.release_api_url) else {
            return;
        };
        if !url.path().ends_with("/releases") {
            return;
        }
        // Notes are optional; a failed or oversized older page cannot block an update.
        // The first page owns the ETag. Completed history survives its 304 response.
        for page in 2..=5 {
            if cached.history_complete(&current) {
                break;
            }
            url.set_query(Some(&format!("per_page=100&page={page}")));
            let result = (|| -> Result<Vec<Value>> {
                let response = self.get(
                    url.as_str(),
                    "application/vnd.github+json",
                    Duration::from_secs(8),
                    "check_failed",
                    None,
                )?;
                if !response.status().is_success() {
                    return Err(UpdateError("check_failed"));
                }
                let mut raw = Vec::new();
                response
                    .take((MAX_RELEASE_BYTES + 1) as u64)
                    .read_to_end(&mut raw)
                    .map_err(|_| UpdateError("check_failed"))?;
                if raw.len() > MAX_RELEASE_BYTES {
                    return Err(UpdateError("invalid_release"));
                }
                serde_json::from_slice(&raw).map_err(|_| UpdateError("invalid_release"))
            })();
            let Ok(older) = result else {
                break;
            };
            let mut combined = cached.clone();
            combined
                .history
                .extend(history_notes(&stable_releases(&older)));
            combined.history.sort_by_key(|note| {
                std::cmp::Reverse(
                    note["version"]
                        .as_str()
                        .and_then(|version| parse_version(version).ok()),
                )
            });
            combined
                .history
                .dedup_by(|a, b| a["version"] == b["version"]);
            combined.history_has_more = older.len() >= 100;
            if serde_json::to_vec(&combined).map_or(true, |bytes| bytes.len() > MAX_RELEASE_BYTES) {
                break;
            }
            *cached = combined;
        }
    }

    fn apply_release(&self, cached: &CachedRelease, persist: bool) -> Result<()> {
        let raw =
            serde_json::to_vec(&cached.release).map_err(|_| UpdateError("invalid_release"))?;
        let snapshot = self.snapshot();
        let desired = current_asset_name()?;
        #[cfg(target_os = "linux")]
        let migration = snapshot.supported
            && crate::update::linux::system_install_manual(
                &installation_target(&self.0.executable)?.0,
                &self.0.dpkg_query,
            )?;
        #[cfg(not(target_os = "linux"))]
        let migration = false;
        self.stage("parse_release");
        let asset = latest_asset(&raw, &desired, &snapshot.current_version, &self.0.endpoints)?;
        if persist {
            self.store_release_cache(Some(cached.clone()));
        }
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            snapshot.release_notes = cached.notes();
            snapshot.release_history = cached.history(&snapshot.current_version);
            snapshot.release_history_complete = cached.history_complete(&snapshot.current_version);
        }
        let state = if migration {
            "migration_required"
        } else if asset.is_some() {
            "available"
        } else {
            "current"
        };
        *self
            .0
            .asset
            .lock()
            .map_err(|_| UpdateError("update_failed"))? = asset;
        self.set_checked(state, cached.version(), migration);
        Ok(())
    }
}
