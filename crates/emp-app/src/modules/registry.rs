use crate::http::request::{MAX_HEADER_BYTES, MODULE_RUNTIME_PREFIX, REQUEST_VERSION};
use emp_modules::{
    manifest::{Contribution, HookPhase, ModuleManifest, RouteAuth},
    peer::PeerHandle,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub struct ModuleRegistry {
    active: Mutex<BTreeMap<String, Arc<ActiveModule>>>,
}

pub struct ActiveModule {
    pub generation: u64,
    pub connection: PeerHandle,
    pub hooks: BTreeSet<HookPhase>,
    pub catalog: bool,
    pub panels: BTreeMap<String, Panel>,
    pub(crate) routes: BTreeMap<String, BTreeMap<String, HttpRoute>>,
}

pub struct Panel {
    pub entry: String,
    pub revision: String,
}

pub(crate) struct HttpRoute {
    pub(crate) id: String,
    pub(crate) auth: RouteAuth,
}

impl ModuleRegistry {
    pub fn publish(
        &self,
        manifest: ModuleManifest,
        generation: u64,
        connection: PeerHandle,
    ) -> Result<(), String> {
        let mut module = ActiveModule {
            generation,
            connection,
            hooks: BTreeSet::new(),
            catalog: false,
            panels: BTreeMap::new(),
            routes: BTreeMap::new(),
        };
        for contribution in manifest.contributions {
            match contribution {
                Contribution::HttpRoute {
                    id,
                    method,
                    path,
                    auth,
                } => {
                    let line_len = method.len()
                        + MODULE_RUNTIME_PREFIX.len()
                        + manifest.id.len()
                        + path.len()
                        + REQUEST_VERSION.len()
                        + 2;
                    if path.chars().any(char::is_whitespace) || line_len > MAX_HEADER_BYTES {
                        return Err(format!(
                            "route {id} cannot be represented in an HTTP request"
                        ));
                    }
                    module
                        .routes
                        .entry(path)
                        .or_default()
                        .insert(method, HttpRoute { id, auth });
                }
                Contribution::Hook { phase } => {
                    module.hooks.insert(phase);
                }
                Contribution::Catalog => module.catalog = true,
                Contribution::Panel {
                    id,
                    entry,
                    revision,
                } => {
                    module.panels.insert(id, Panel { entry, revision });
                }
            }
        }
        let module = Arc::new(module);
        let replaced = self
            .active
            .lock()
            .expect("module registry")
            .insert(manifest.id, module);
        drop(replaced);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<ActiveModule>> {
        self.active
            .lock()
            .expect("module registry")
            .get(id)
            .cloned()
    }

    pub fn revoke(&self, id: &str, generation: u64) {
        let removed = {
            let mut active = self.active.lock().expect("module registry");
            if active
                .get(id)
                .is_some_and(|module| module.generation == generation)
            {
                active.remove(id)
            } else {
                None
            }
        };
        drop(removed);
    }
}
