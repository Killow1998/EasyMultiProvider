use super::worker::Worker;
use emp_modules::Module;
use emp_updater::{manager::UpdateManager, release::module_version as module_version_rank};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::PathBuf,
};

#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct Install {
    versions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    module_versions: BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) artifact: Option<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) previous: Option<Artifact>,
    pub(super) enabled: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Artifact {
    pub(super) digest: String,
    pub(super) version: String,
}

impl Install {
    pub(super) fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub(super) fn version(&self) -> Option<&str> {
        self.artifact
            .as_ref()
            .map(|artifact| artifact.version.as_str())
            .or_else(|| {
                let (version, _) = self
                    .versions
                    .get_key_value(crate::VERSION)
                    .or_else(|| self.versions.last_key_value())?;
                Some(
                    self.module_versions
                        .get(version)
                        .and_then(Option::as_deref)
                        .unwrap_or(version),
                )
            })
    }

    pub(super) fn selected_version(&self) -> Option<&str> {
        self.artifact
            .as_ref()
            .map(|artifact| artifact.version.as_str())
            .or_else(|| {
                self.module_versions
                    .get(crate::VERSION)
                    .and_then(Option::as_deref)
            })
    }

    pub(super) fn selected_digest(&self) -> Option<&str> {
        self.artifact
            .as_ref()
            .map(|artifact| artifact.digest.as_str())
            .or_else(|| self.versions.get(crate::VERSION).map(String::as_str))
    }

    fn retains(&self, digest: &str) -> bool {
        self.selected_digest() == Some(digest)
            || self
                .previous
                .as_ref()
                .is_some_and(|artifact| artifact.digest == digest)
            || self.versions.values().any(|value| value == digest)
    }
}

pub(super) type Installs = BTreeMap<Module, Install>;

pub(super) fn release_channel(module: Module) -> String {
    format!("emp-mod-{}-api{}-v", module.id(), emp_modules::HOST_API)
}

fn is_artifact_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) struct Packages {
    root: PathBuf,
}

impl Packages {
    pub(super) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(super) fn load(&self) -> Result<Option<Installs>, &'static str> {
        match fs::read(self.root.join("modules.json")) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map(Some)
                .map_err(|_| "invalid_module_settings"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("module_directory_unavailable"),
        }
    }

    pub(super) fn bundled_transition(
        &self,
        config: &serde_json::Value,
        has_gateway_devices: Option<bool>,
        saved: &mut Installs,
    ) -> Result<(), String> {
        let payload: &[(Module, &[u8], &str)] =
            include!(concat!(env!("OUT_DIR"), "/module-transition.rs"));
        if payload.len() != 3
            || payload
                .iter()
                .any(|&(_, bytes, digest)| format!("{:x}", Sha256::digest(bytes)) != digest)
        {
            return Err("legacy_module_transition_missing".into());
        }
        emp_state::filesystem::create_private_directory(&self.root)
            .map_err(|_| "module_directory_unavailable")?;
        for &(module, bytes, digest) in payload {
            let configured = match module {
                Module::Decision => {
                    config["decision_provider"]
                        .as_str()
                        .is_some_and(|provider| !provider.is_empty())
                        || config["providers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|provider| provider.get("decision_api").is_some())
                }
                Module::Gateway => has_gateway_devices.unwrap_or(true),
                Module::Aggregate => config["model_groups"]
                    .as_array()
                    .is_some_and(|groups| !groups.is_empty()),
                Module::Team | Module::Aero => false,
            };
            if !configured && !saved.get(&module).is_some_and(|install| install.enabled) {
                continue;
            }
            let path = self.root.join(Self::filename(module, digest));
            if self.verified_path(module, digest).is_err() {
                emp_state::filesystem::atomic_write_private_state(&path, bytes)
                    .map_err(|_| "module_directory_unavailable")?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                        .map_err(|_| "module_directory_unavailable")?;
                }
            }
            let install = saved.entry(module).or_default();
            install.enabled = true;
            install
                .versions
                .insert(crate::VERSION.into(), digest.into());
            install.artifact = Some(Artifact {
                digest: digest.into(),
                version: module.version().into(),
            });
        }
        Ok(())
    }

    pub(super) fn remove_unused(
        &self,
        module: Module,
        install: Option<&Install>,
    ) -> Result<(), &'static str> {
        let prefix = format!("{}-", module.id());
        let entries = fs::read_dir(&self.root).map_err(|_| "module_directory_unavailable")?;
        for entry in entries {
            let entry = entry.map_err(|_| "module_directory_unavailable")?;
            if entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .and_then(|name| name.strip_suffix(std::env::consts::EXE_SUFFIX))
                .is_some_and(|digest| {
                    is_artifact_digest(digest)
                        && !install.is_some_and(|policy| policy.retains(digest))
                })
            {
                fs::remove_file(entry.path()).map_err(|_| "module_directory_unavailable")?;
            }
        }
        Ok(())
    }

    pub(super) fn save(&self, installs: &Installs) -> Result<(), &'static str> {
        let raw = serde_json::to_vec(installs).expect("module settings serialize");
        emp_state::filesystem::atomic_write_private_state(&self.root.join("modules.json"), &raw)
            .map_err(|_| "module_directory_unavailable")
    }

    pub(super) fn download(
        &self,
        module: Module,
        version: &str,
        updater: &UpdateManager,
        minimum: Option<&str>,
        expected_revision: Option<&str>,
    ) -> Result<Option<Artifact>, String> {
        emp_state::filesystem::create_private_directory(&self.root)
            .map_err(|_| "module_directory_unavailable")?;
        let temporary =
            tempfile::tempdir_in(&self.root).map_err(|_| "module_directory_unavailable")?;
        let downloaded = temporary
            .path()
            .join(format!("module{}", std::env::consts::EXE_SUFFIX));
        let (digest, release_version) = updater
            .download_official_module(
                &release_channel(module),
                version,
                &module.asset(),
                &downloaded,
                expected_revision,
            )
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&downloaded, fs::Permissions::from_mode(0o700))
                .map_err(|_| "module_directory_unavailable")?;
        }
        let worker = Worker::open(&downloaded, module)?;
        let module_version = worker.module_version.clone();
        drop(worker);
        let rank = module_version_rank(&module_version).ok_or("incompatible_module")?;
        if release_version.is_some_and(|version| module_version_rank(&version) != Some(rank)) {
            return Err("incompatible_module".into());
        }
        if minimum
            .and_then(module_version_rank)
            .is_some_and(|installed| installed > rank)
        {
            return Ok(None);
        }
        if self.verified_path(module, &digest).is_err() {
            let path = self.root.join(Self::filename(module, &digest));
            if path.exists() {
                fs::remove_file(&path).map_err(|_| "module_directory_unavailable")?;
            }
            fs::rename(downloaded, path).map_err(|_| "module_directory_unavailable")?;
        }
        Ok(Some(Artifact {
            digest,
            version: module_version,
        }))
    }

    fn filename(module: Module, digest: &str) -> String {
        format!("{}-{digest}{}", module.id(), std::env::consts::EXE_SUFFIX)
    }

    pub(super) fn verified_path(
        &self,
        module: Module,
        digest: &str,
    ) -> Result<PathBuf, &'static str> {
        if !is_artifact_digest(digest) {
            return Err("module_checksum_failed");
        }
        let path = self.root.join(Self::filename(module, digest));
        let metadata = path
            .symlink_metadata()
            .map_err(|_| "module_not_installed")?;
        if !metadata.is_file() {
            return Err("module_not_installed");
        }
        let mut hash = Sha256::new();
        let mut file = File::open(&path).map_err(|_| "module_not_installed")?;
        std::io::copy(&mut file, &mut hash).map_err(|_| "module_checksum_failed")?;
        if format!("{:x}", hash.finalize()) != digest {
            return Err("module_checksum_failed");
        }
        Ok(path)
    }
}
