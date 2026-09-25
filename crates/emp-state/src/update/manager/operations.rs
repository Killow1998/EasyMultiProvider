use super::*;

impl UpdateManager {
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

    pub(super) fn check(&self) -> Result<()> {
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
            && crate::update::linux::system_install_manual(
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
                    if migration {
                        "migration_required"
                    } else {
                        "available"
                    },
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
                    if migration {
                        "migration_required"
                    } else {
                        "current"
                    },
                    version,
                    migration,
                );
            }
        }
        Ok(())
    }

    pub(super) fn install(&self) -> Result<()> {
        let asset = self
            .0
            .asset
            .lock()
            .map_err(|_| UpdateError("update_failed"))?
            .clone()
            .ok_or(UpdateError("update_unavailable"))?;
        let (target, relative) = installation_target(&self.0.executable)?;
        #[cfg(target_os = "linux")]
        if crate::update::linux::system_install_manual(&target, &self.0.dpkg_query)? {
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

    pub(in crate::update) fn open_package(&self, raw_url: &str) -> Result<Response> {
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
