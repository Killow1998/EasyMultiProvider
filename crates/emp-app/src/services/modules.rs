pub(crate) mod aero;
mod packages;
mod ui;
mod worker;

use emp_modules::{Command, Module};
use emp_updater::{manager::UpdateManager, release::module_version as module_version_rank};
use packages::{Artifact, Install, Installs, Packages, release_channel};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use worker::Worker;

pub(crate) struct Modules {
    packages: Packages,
    saved: Mutex<Installs>,
    registry_error: Option<&'static str>,
    changes: Mutex<()>,
    runtime: BTreeMap<Module, Mutex<Runtime>>,
}

#[derive(Default)]
struct Runtime {
    worker: Option<Arc<ActiveWorker>>,
    error: Option<&'static str>,
    draining: Option<Arc<AtomicBool>>,
}

struct ActiveWorker {
    artifact: Artifact,
    process: Mutex<Option<Worker>>,
    aero: Mutex<Option<aero::Endpoint>>,
    stopped: Arc<AtomicBool>,
}

impl Drop for ActiveWorker {
    fn drop(&mut self) {
        self.process
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.stopped.store(true, Ordering::Release);
    }
}

impl ActiveWorker {
    fn call(&self, command: &Command) -> Result<Value, String> {
        let mut process = self.process.lock().map_err(|_| "module_unavailable")?;
        match process.as_mut().ok_or("module_unavailable")?.call(command) {
            Ok(result) => result,
            Err(error) => {
                *process = None;
                Err(error.into())
            }
        }
    }
}

impl Runtime {
    fn draining(&self) -> bool {
        self.draining
            .as_ref()
            .is_some_and(|stopped| !stopped.load(Ordering::Acquire))
    }

    fn active(&mut self) -> Option<Arc<ActiveWorker>> {
        let worker = self.worker.as_ref()?;
        let running = worker
            .process
            .lock()
            .ok()
            .is_some_and(|mut process| process.as_mut().is_some_and(Worker::is_running));
        if running {
            return Some(Arc::clone(worker));
        }
        self.worker = None;
        self.error = Some("module_stopped");
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Startup {
    Normal,
    BundledUpdate,
    PreparedUpdate,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Install,
    Update,
    Enable,
    Disable,
    Uninstall,
}

type TestModuleHandler = fn(Command) -> Result<Value, &'static str>;

#[cfg(test)]
fn test_module_handler(module: Module) -> Option<TestModuleHandler> {
    match module {
        Module::Decision => Some(emp_mod_decision::handle),
        Module::Gateway => Some(emp_mod_gateway::handle),
        Module::Aggregate => Some(emp_mod_aggregate::handle),
        Module::Team | Module::Aero => None,
    }
}

#[cfg(not(test))]
fn test_module_handler(_: Module) -> Option<TestModuleHandler> {
    None
}

impl Modules {
    pub(crate) fn open(
        root: PathBuf,
        startup: Startup,
        config: &Value,
        has_gateway_devices: Option<bool>,
    ) -> Result<Self, String> {
        let update = startup != Startup::Normal;
        let mut modules = Self {
            packages: Packages::new(root),
            saved: Mutex::new(Installs::new()),
            registry_error: None,
            changes: Mutex::new(()),
            runtime: Module::ALL
                .map(|module| (module, Mutex::new(Runtime::default())))
                .into(),
        };
        let saved = match modules.packages.load() {
            Ok(saved) => saved,
            Err(error) if update => return Err(error.into()),
            Err(error) => {
                modules.registry_error = Some(error);
                return Ok(modules);
            }
        };
        let bundled_transition = startup == Startup::BundledUpdate;
        let mut changed = saved.is_none() || bundled_transition;
        let mut saved = saved.unwrap_or_default();
        if bundled_transition {
            modules
                .packages
                .bundled_transition(config, has_gateway_devices, &mut saved)?;
        }
        for (&module, install) in saved.iter_mut().filter(|(_, install)| install.enabled) {
            let start = |digest| modules.start(module, digest);
            let selected = install.selected_digest();
            let attempt = selected.ok_or("incompatible_module").and_then(start);
            let (worker, error) = match attempt {
                Ok(active) => (active, None),
                Err(error) => {
                    if update && (!bundled_transition || selected.is_some()) {
                        return Err(error.into());
                    }
                    if let Some(active) = install
                        .previous
                        .as_ref()
                        .and_then(|artifact| start(&artifact.digest).ok())
                    {
                        (active, Some("module_update_rolled_back"))
                    } else {
                        modules.mark_unavailable(module, error);
                        continue;
                    }
                }
            };
            let artifact = &worker.artifact;
            if !bundled_transition
                && install.artifact.as_ref().map(|old| &old.digest) != Some(&artifact.digest)
            {
                install.artifact = Some(artifact.clone());
                install.previous = None;
                changed = true;
            }
            *modules.runtime[&module].lock().expect("module runtime") = Runtime {
                worker: Some(worker),
                error,
                ..Runtime::default()
            };
        }
        if changed && let Err(error) = modules.packages.save(&saved) {
            if update {
                return Err(error.into());
            }
            modules.registry_error = Some(error);
        }
        modules.saved = Mutex::new(saved);
        Ok(modules)
    }

    pub(crate) fn stop(&self) {
        for runtime in self.runtime.values() {
            let worker = runtime
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .worker
                .take();
            drop(worker);
        }
    }

    fn start(&self, module: Module, digest: &str) -> Result<Arc<ActiveWorker>, &'static str> {
        let path = self.packages.verified_path(module, digest)?;
        let worker = Worker::open(&path, module)?;
        Ok(Arc::new(ActiveWorker {
            artifact: Artifact {
                digest: digest.to_owned(),
                version: worker.module_version.clone(),
            },
            process: Mutex::new(Some(worker)),
            aero: Mutex::new(None),
            stopped: Arc::new(AtomicBool::new(false)),
        }))
    }

    fn worker(&self, module: Module) -> Option<Arc<ActiveWorker>> {
        self.runtime[&module].lock().ok()?.active()
    }

    pub(crate) fn active(&self, module: Module) -> bool {
        if test_module_handler(module).is_some()
            && self.runtime[&module]
                .lock()
                .expect("module runtime")
                .error
                .is_none()
            && self.registry_error.is_none()
        {
            return true;
        }
        self.worker(module).is_some()
    }

    pub(crate) fn mark_unavailable(&self, module: Module, error: &'static str) {
        *self.runtime[&module].lock().expect("module runtime") = Runtime {
            worker: None,
            error: Some(error),
            ..Runtime::default()
        };
    }

    pub(crate) fn call(&self, module: Module, command: Command) -> Result<Value, String> {
        if let Some(run) = test_module_handler(module) {
            return run(command).map_err(str::to_owned);
        }
        let worker = self.worker(module).ok_or("module_disabled")?;
        worker.call(&command)
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

    pub(crate) fn panel(&self, module: Module, revision: Option<&str>) -> Option<String> {
        let worker = self.worker(module)?;
        if revision.is_some_and(|revision| revision != worker.artifact.digest) {
            return None;
        }
        let process = worker.process.lock().ok()?;
        Some(process.as_ref()?.panel.clone())
    }

    pub(crate) fn status(&self) -> Vec<Value> {
        let saved = self.saved.lock().expect("module settings");
        Module::ALL
            .into_iter()
            .map(|module| {
                let current = saved.get(&module);
                let enabled = current.is_some_and(|install| install.enabled);
                let mut runtime = self.runtime[&module].lock().expect("module runtime");
                let running = runtime.active();
                let artifact = running.as_ref().map(|worker| &worker.artifact);
                let error = self.registry_error.or(runtime.error);
                json!({
                    "id":module,
                    "ui":ui::entry(module),
                    "version":module.version(),
                    "installed_version":current.and_then(|install| install.version(artifact)),
                    "active_version": artifact.map(|artifact| &artifact.version),
                    "active_revision": artifact.map(|artifact| &artifact.digest),
                    "enabled":enabled,
                    "active":running.is_some(),
                    "draining":runtime.draining(),
                    "restart_required": enabled && artifact.is_some_and(|artifact| {
                        current.and_then(Install::selected_digest) != Some(&artifact.digest)
                    }),
                    "error":error,
                })
            })
            .collect()
    }

    pub(crate) fn check_updates(&self, updater: &UpdateManager) -> Result<Vec<Value>, String> {
        if let Some(error) = self.registry_error {
            return Err(error.into());
        }
        let installed: Vec<_> = self
            .saved
            .lock()
            .map_err(|_| "module_unavailable")?
            .iter()
            .filter_map(|(&module, install)| {
                let worker = self.worker(module);
                let version = install.version(worker.as_ref().map(|worker| &worker.artifact))?;
                Some((module, version.to_owned()))
            })
            .collect();
        let targets: Vec<_> = installed
            .iter()
            .map(|(module, _)| (release_channel(*module), module.asset()))
            .collect();
        let targets: Vec<_> = targets
            .iter()
            .map(|(channel, asset)| (channel.as_str(), asset.as_str()))
            .collect();
        let releases = updater
            .official_module_releases(&targets)
            .map_err(|error| error.to_string())?;
        Ok(installed
            .into_iter()
            .zip(releases)
            .filter_map(|((module, current), release)| {
                let release = release?;
                (module_version_rank(&release.version) > module_version_rank(&current)).then(|| {
                    json!({
                        "id":module,
                        "current_version":current,
                        "latest_version":release.version,
                        "release_notes":release.notes,
                        "revision":release.revision(),
                    })
                })
            })
            .collect())
    }

    pub(crate) fn change(
        &self,
        module: Module,
        action: Action,
        expected_revision: Option<&str>,
        updater: &UpdateManager,
        before_uninstall: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if let Some(error) = self.registry_error {
            return Err(error.into());
        }
        let _change = self.changes.lock().map_err(|_| "module_unavailable")?;
        let mut updated = self.saved.lock().map_err(|_| "module_unavailable")?.clone();
        let mut current = self.worker(module);
        match action {
            Action::Install | Action::Update => {
                if action == Action::Update && !updated.contains_key(&module) {
                    return Err("module_not_installed".into());
                }
                let minimum = updated.get(&module).and_then(Install::selected_version);
                let Some(artifact) = self.packages.download(
                    module,
                    crate::VERSION,
                    updater,
                    minimum,
                    expected_revision,
                )?
                else {
                    return Ok(());
                };
                let install = updated.entry(module).or_insert_with(Install::enabled);
                if install.selected_digest() != Some(&artifact.digest) {
                    install.previous = current
                        .as_ref()
                        .map(|worker| worker.artifact.clone())
                        .or_else(|| install.artifact.take());
                    install.artifact = Some(artifact);
                }
            }
            Action::Enable | Action::Disable => {
                let install = updated.get_mut(&module).ok_or("module_not_installed")?;
                install.enabled = action == Action::Enable;
            }
            Action::Uninstall => {
                let install = updated.get(&module).ok_or("module_not_installed")?;
                if install.enabled || current.is_some() {
                    return Err("module_disable_before_uninstall".into());
                }
                before_uninstall()?;
                updated.remove(&module);
            }
        }
        let blocked = self.runtime[&module]
            .lock()
            .map_err(|_| "module_unavailable")?
            .error
            .filter(|error| *error == "invalid_gateway_clients");
        let next = if let Some(install) = updated.get(&module).filter(|install| install.enabled) {
            if let Some(error) = blocked {
                return Err(error.into());
            }
            let digest = install.selected_digest().ok_or("incompatible_module")?;
            match &current {
                Some(worker) if worker.artifact.digest == digest => Some(Arc::clone(worker)),
                _ => Some(self.start(module, digest)?),
            }
        } else {
            None
        };
        {
            let mut saved = self.saved.lock().map_err(|_| "module_unavailable")?;
            let mut runtime = self.runtime[&module]
                .lock()
                .map_err(|_| "module_unavailable")?;
            let replacing = match (&current, &next) {
                (Some(current), Some(next)) => !Arc::ptr_eq(current, next),
                _ => false,
            };
            if module == Module::Aero
                && ((replacing
                    && current
                        .as_ref()
                        .is_some_and(|worker| Arc::strong_count(worker) > 2))
                    || (next.is_some() && runtime.draining())
                    || (action == Action::Uninstall && runtime.draining()))
            {
                return Err("module_busy".into());
            }
            self.packages.save(&updated)?;
            if module == Module::Aero
                && next.is_none()
                && let Some(current) = &current
            {
                runtime.draining = Some(Arc::clone(&current.stopped));
            }
            if module == Module::Aero && replacing {
                runtime.worker.take();
                current.take();
            }
            runtime.worker = next;
            runtime.error = blocked;
            *saved = updated;
        }
        drop(current);
        if matches!(action, Action::Install | Action::Update | Action::Uninstall) {
            let saved = self.saved.lock().map_err(|_| "module_unavailable")?;
            self.packages.remove_unused(module, saved.get(&module))?;
        }
        Ok(())
    }

    pub(crate) fn prepare_update(
        &self,
        updater: &UpdateManager,
        version: &str,
    ) -> emp_updater::Result<()> {
        use emp_updater::UpdateError;
        if let Some(error) = self.registry_error {
            return Err(UpdateError(error));
        }
        let _change = self
            .changes
            .lock()
            .map_err(|_| UpdateError("module_update_failed"))?;
        let mut updated = self
            .saved
            .lock()
            .map_err(|_| UpdateError("module_update_failed"))?
            .clone();
        if !updated.values().any(|install| install.enabled) {
            return Ok(());
        }
        for (&module, install) in updated.iter_mut().filter(|(_, install)| install.enabled) {
            if install.artifact.as_ref().is_some_and(|artifact| {
                self.packages
                    .verified_path(module, &artifact.digest)
                    .is_ok()
            }) {
                continue;
            }
            let artifact = self
                .packages
                .download(module, version, updater, None, None)
                .map_err(|_| UpdateError("module_update_failed"))?
                .ok_or(UpdateError("module_update_failed"))?;
            install.previous = self.worker(module).map(|worker| worker.artifact.clone());
            install.artifact = Some(artifact);
        }
        self.packages
            .save(&updated)
            .map_err(|_| UpdateError("module_update_failed"))?;
        *self
            .saved
            .lock()
            .map_err(|_| UpdateError("module_update_failed"))? = updated;
        Ok(())
    }
}
