//! Last-good release metadata: local first, conditionally refreshed from its source.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct CachedRelease {
    source: String,
    etag: Option<String>,
    release: serde_json::Value,
}

impl CachedRelease {
    pub(super) fn notes(&self) -> String {
        self.release
            .get("body")
            .and_then(|body| body.as_str())
            .unwrap_or_default()
            .chars()
            .take(65536)
            .collect()
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
    let cached: CachedRelease = serde_json::from_slice(&raw).ok()?;
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
                if let Ok(raw) = serde_json::to_vec(cached) {
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
            *self
                .0
                .asset
                .lock()
                .map_err(|_| UpdateError("update_failed"))? = None;
            self.store_release_cache(None);
            if let Ok(mut snapshot) = self.0.snapshot.lock() {
                snapshot.release_notes.clear();
            }
            self.set("no_release", "", None, 0);
            return Ok(());
        }
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            let cached = cached.ok_or(UpdateError("invalid_release"))?;
            return self.apply_release(&cached, false);
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
        let release = serde_json::from_slice(&raw).map_err(|_| UpdateError("update_failed"))?;
        self.apply_release(
            &CachedRelease {
                source: self.0.endpoints.release_api_url.clone(),
                etag,
                release,
            },
            true,
        )
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
