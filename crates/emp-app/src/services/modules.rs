mod worker;

use emp_modules::{Command, Module};
use emp_state::update::{manager::UpdateManager, release::module_version as module_version_rank};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Mutex,
};
use worker::Worker;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Install {
    versions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    module_versions: BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact: Option<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous: Option<Artifact>,
    enabled: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct Artifact {
    digest: String,
    version: String,
}

impl Install {
    fn selected_digest(&self) -> Option<&str> {
        self.artifact
            .as_ref()
            .map(|artifact| artifact.digest.as_str())
            .or_else(|| self.versions.get(crate::VERSION).map(String::as_str))
    }
}

type Installs = BTreeMap<Module, Install>;

pub(crate) struct Modules {
    root: PathBuf,
    saved: Mutex<Installs>,
    registry_error: Option<&'static str>,
    workers: BTreeMap<Module, Mutex<Option<Worker>>>,
    running: BTreeMap<Module, Artifact>,
    failures: BTreeMap<Module, &'static str>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Startup {
    Normal,
    BundledUpdate,
    PreparedUpdate,
}

impl Modules {
    pub(crate) fn open(root: PathBuf, startup: Startup) -> Result<Self, String> {
        let update = startup != Startup::Normal;
        let mut modules = Self {
            root,
            saved: Mutex::new(Installs::new()),
            registry_error: None,
            workers: BTreeMap::new(),
            running: BTreeMap::new(),
            failures: BTreeMap::new(),
        };
        let saved = match fs::read(modules.root.join("modules.json")) {
            Ok(raw) => serde_json::from_slice::<Installs>(&raw)
                .map(Some)
                .map_err(|_| "invalid_module_settings"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("module_directory_unavailable"),
        };
        let saved = match saved {
            Ok(saved) => saved,
            Err(error) if update => return Err(error.into()),
            Err(error) => {
                modules.registry_error = Some(error);
                modules
                    .failures
                    .extend(Module::ALL.map(|module| (module, error)));
                return Ok(modules);
            }
        };
        let bundled_transition = startup == Startup::BundledUpdate;
        let mut changed = saved.is_none() || bundled_transition;
        let mut saved = match saved {
            saved if bundled_transition => {
                let mut saved = saved.unwrap_or_default();
                saved.extend(modules.prepare_bundled_transition()?);
                saved
            }
            Some(saved) => saved,
            None => Installs::new(),
        };
        for (&module, install) in saved.iter_mut().filter(|(_, install)| install.enabled) {
            let start = |digest| {
                Self::verified_path(&modules.root, module, digest)
                    .and_then(|path| Worker::open(&path, module))
                    .map(|worker| (digest, worker))
            };
            let attempt = install
                .selected_digest()
                .ok_or("incompatible_module")
                .and_then(start);
            let (digest, worker) = match attempt {
                Ok(active) => active,
                Err(error) if update => return Err(error.into()),
                Err(error) => match install
                    .previous
                    .as_ref()
                    .and_then(|artifact| start(&artifact.digest).ok())
                {
                    Some(active) => {
                        modules.failures.insert(module, "module_update_rolled_back");
                        active
                    }
                    None => {
                        modules.failures.insert(module, error);
                        continue;
                    }
                },
            };
            let artifact = Artifact {
                digest: digest.to_owned(),
                version: worker.module_version.clone(),
            };
            if install.artifact.as_ref().map(|old| old.digest.as_str()) != Some(digest) {
                install.artifact = Some(artifact.clone());
                install.previous = None;
                changed = true;
            }
            modules.running.insert(module, artifact);
            modules.workers.insert(module, Mutex::new(Some(worker)));
        }
        if changed && let Err(error) = modules.write(&saved) {
            if update {
                return Err(error.into());
            }
            modules
                .failures
                .extend(Module::ALL.map(|module| (module, error)));
        }
        modules.saved = Mutex::new(saved);
        Ok(modules)
    }

    fn prepare_bundled_transition(&self) -> Result<Installs, String> {
        let payload: &[(Module, &[u8], &str)] =
            include!(concat!(env!("OUT_DIR"), "/module-transition.rs"));
        if payload.len() != 3 {
            return Err("legacy_module_transition_missing".into());
        }
        emp_state::filesystem::create_private_directory(&self.root)
            .map_err(|_| "module_directory_unavailable")?;
        payload
            .iter()
            .map(|&(module, bytes, digest)| {
                if format!("{:x}", Sha256::digest(bytes)) != digest {
                    return Err("module_checksum_failed".into());
                }
                let path = self.root.join(Self::filename(module, digest));
                if Self::verified_path(&self.root, module, digest).is_err() {
                    emp_state::filesystem::atomic_write_private_state(&path, bytes)
                        .map_err(|_| "module_directory_unavailable")?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                            .map_err(|_| "module_directory_unavailable")?;
                    }
                }
                Ok((
                    module,
                    Install {
                        enabled: true,
                        artifact: Some(Artifact {
                            digest: digest.into(),
                            version: module.version().into(),
                        }),
                        ..Install::default()
                    },
                ))
            })
            .collect()
    }

    pub(crate) fn active(&self, module: Module) -> bool {
        #[cfg(test)]
        if module != Module::Team && !self.failures.contains_key(&module) {
            return true;
        }
        self.workers
            .get(&module)
            .and_then(|worker| worker.lock().ok())
            .is_some_and(|mut slot| {
                let running = slot.as_mut().is_some_and(Worker::is_running);
                if !running {
                    *slot = None;
                }
                running
            })
    }

    pub(crate) fn mark_unavailable(&mut self, module: Module, error: &'static str) {
        self.workers.remove(&module);
        self.running.remove(&module);
        self.failures.insert(module, error);
    }

    pub(crate) fn call(&self, module: Module, command: Command) -> Result<Value, String> {
        #[cfg(test)]
        if module != Module::Team {
            let run = match module {
                Module::Decision => emp_mod_decision::handle,
                Module::Gateway => emp_mod_gateway::handle,
                Module::Aggregate => emp_mod_aggregate::handle,
                Module::Team => unreachable!(),
            };
            return run(command).map_err(str::to_owned);
        }
        let mut slot = self
            .workers
            .get(&module)
            .ok_or("module_disabled")?
            .lock()
            .map_err(|_| "module_unavailable")?;
        match slot.as_mut().ok_or("module_unavailable")?.call(&command) {
            Ok(result) => result,
            Err(error) => {
                *slot = None;
                Err(error.into())
            }
        }
    }

    pub(crate) fn call_typed<T: DeserializeOwned>(
        &self,
        module: Module,
        command: Command,
    ) -> Result<T, String> {
        self.call(module, command).and_then(|reply| {
            serde_json::from_value(reply).map_err(|_| "invalid_module_reply".into())
        })
    }

    pub(crate) fn panel(&self, module: Module) -> Option<String> {
        let mut slot = self.workers.get(&module)?.lock().ok()?;
        let worker = slot.as_mut()?;
        worker.is_running().then(|| worker.panel.clone())
    }

    pub(crate) fn status(&self) -> Vec<Value> {
        let saved = self.saved.lock().expect("module settings");
        Module::ALL
            .into_iter()
            .map(|module| {
                let current = saved.get(&module);
                let enabled = current.is_some_and(|install| install.enabled);
                let active = self.active(module);
                let error = self.failures.get(&module).copied().or_else(|| {
                    (self.workers.contains_key(&module) && !active).then_some("module_stopped")
                });
                json!({
                    "id":module,
                    "version":module.version(),
                    "installed_version":current.and_then(|install| {
                        install.artifact.as_ref().map(|artifact| artifact.version.as_str()).or_else(|| {
                            let version = install.versions.get_key_value(crate::VERSION)
                                .or_else(|| install.versions.last_key_value())?.0;
                            Some(install.module_versions.get(version).and_then(Option::as_deref).unwrap_or(version))
                        })
                    }),
                    "active_version": self.running.get(&module).filter(|_| active).map(|artifact| &artifact.version),
                    "active_revision": self.running.get(&module).filter(|_| active).map(|artifact| &artifact.digest),
                    "enabled":enabled,
                    "active":active,
                    "restart_required": (enabled != active && error.is_none()) || (enabled && active
                        && current.and_then(Install::selected_digest) != self.running.get(&module).map(|artifact| artifact.digest.as_str())),
                    "error":error,
                })
            })
            .collect()
    }

    pub(crate) fn change(
        &self,
        module: Module,
        action: &str,
        updater: &UpdateManager,
        before_uninstall: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if let Some(error) = self.registry_error {
            return Err(error.into());
        }
        let mut saved = self.saved.lock().map_err(|_| "module_unavailable")?;
        let mut updated = saved.clone();
        match action {
            "install" | "update" => {
                if action == "update" && !updated.contains_key(&module) {
                    return Err("module_not_installed".into());
                }
                let minimum = updated.get(&module).and_then(|install| {
                    install
                        .artifact
                        .as_ref()
                        .map(|artifact| artifact.version.as_str())
                        .or_else(|| {
                            install
                                .module_versions
                                .get(crate::VERSION)
                                .and_then(Option::as_deref)
                        })
                });
                let Some(artifact) = self.download(module, crate::VERSION, updater, minimum)?
                else {
                    return Ok(());
                };
                let install = updated.entry(module).or_insert_with(|| Install {
                    enabled: true,
                    ..Install::default()
                });
                if install.selected_digest() != Some(&artifact.digest) {
                    install.previous = self
                        .running
                        .get(&module)
                        .cloned()
                        .or_else(|| install.artifact.take());
                    install.artifact = Some(artifact);
                }
            }
            "enable" | "disable" => {
                let install = updated.get_mut(&module).ok_or("module_not_installed")?;
                if action == "enable" {
                    let digest = install.selected_digest().ok_or("incompatible_module")?;
                    Self::verified_path(&self.root, module, digest)?;
                }
                install.enabled = action == "enable";
            }
            "uninstall" => {
                let install = updated.get(&module).ok_or("module_not_installed")?;
                if install.enabled || self.active(module) {
                    return Err("module_disable_before_uninstall".into());
                }
                before_uninstall()?;
                updated.remove(&module);
            }
            _ => return Err("invalid_module_action".into()),
        }
        self.write(&updated)?;
        *saved = updated;
        if matches!(action, "install" | "update" | "uninstall") {
            self.remove_artifacts(module, saved.get(&module))?;
        }
        Ok(())
    }

    fn remove_artifacts(
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
                    digest.len() == 64
                        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                        && self
                            .running
                            .get(&module)
                            .is_none_or(|running| running.digest != digest)
                        && !install.is_some_and(|policy| {
                            policy.selected_digest() == Some(digest)
                                || policy
                                    .previous
                                    .as_ref()
                                    .is_some_and(|artifact| artifact.digest == digest)
                                || policy.versions.values().any(|value| value == digest)
                        })
                })
            {
                fs::remove_file(entry.path()).map_err(|_| "module_directory_unavailable")?;
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_update(
        &self,
        updater: &UpdateManager,
        version: &str,
    ) -> emp_state::update::Result<()> {
        use emp_state::update::UpdateError;
        if let Some(error) = self.registry_error {
            return Err(UpdateError(error));
        }
        let mut saved = self
            .saved
            .lock()
            .map_err(|_| UpdateError("module_update_failed"))?;
        if !saved.values().any(|install| install.enabled) {
            return Ok(());
        }
        let mut updated = saved.clone();
        for (&module, install) in updated.iter_mut().filter(|(_, install)| install.enabled) {
            if install.artifact.as_ref().is_some_and(|artifact| {
                Self::verified_path(&self.root, module, &artifact.digest).is_ok()
            }) {
                continue;
            }
            let artifact = self
                .download(module, version, updater, None)
                .map_err(|_| UpdateError("module_update_failed"))?
                .ok_or(UpdateError("module_update_failed"))?;
            install.previous = self.running.get(&module).cloned();
            install.artifact = Some(artifact);
        }
        self.write(&updated)
            .map_err(|_| UpdateError("module_update_failed"))?;
        *saved = updated;
        Ok(())
    }

    fn write(&self, installs: &Installs) -> Result<(), &'static str> {
        emp_state::filesystem::create_private_directory(&self.root)
            .map_err(|_| "module_directory_unavailable")?;
        let raw = serde_json::to_vec(installs).expect("module settings serialize");
        emp_state::filesystem::atomic_write_private_state(&self.root.join("modules.json"), &raw)
            .map_err(|_| "module_directory_unavailable")
    }

    fn download(
        &self,
        module: Module,
        version: &str,
        updater: &UpdateManager,
        minimum: Option<&str>,
    ) -> Result<Option<Artifact>, String> {
        emp_state::filesystem::create_private_directory(&self.root)
            .map_err(|_| "module_directory_unavailable")?;
        let temporary =
            tempfile::tempdir_in(&self.root).map_err(|_| "module_directory_unavailable")?;
        let downloaded = temporary
            .path()
            .join(format!("module{}", std::env::consts::EXE_SUFFIX));
        let channel = format!("emp-mod-{}-api{}-v", module.id(), emp_modules::HOST_API);
        let (digest, release_version) = updater
            .download_official_module(&channel, version, &module.asset(), &downloaded)
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
        if Self::verified_path(&self.root, module, &digest).is_err() {
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

    fn verified_path(root: &Path, module: Module, digest: &str) -> Result<PathBuf, &'static str> {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("module_checksum_failed");
        }
        let path = root.join(Self::filename(module, digest));
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
