use crate::http::{
    auth::SESSION_HEADER,
    request::{MODULE_RUNTIME_PREFIX, REQUEST_VERSION, request_head_end},
};
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
    active: Mutex<BTreeMap<Arc<str>, Arc<ActiveModule>>>,
}

pub struct ActiveModule {
    pub id: Arc<str>,
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
        port: u16,
        local_proxy_key_len: usize,
    ) -> Result<(), String> {
        let mut module = ActiveModule {
            id: Arc::from(manifest.id),
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
                    let (header, prefix, token_len) = match auth {
                        RouteAuth::Management | RouteAuth::Proxy => {
                            (SESSION_HEADER, "", emp_state::WEB_SESSION_TOKEN_LENGTH)
                        }
                        RouteAuth::LocalProxy => ("Authorization", "Bearer ", local_proxy_key_len),
                    };
                    let head = format!(
                        "{method} {MODULE_RUNTIME_PREFIX}{}{path} {REQUEST_VERSION}\r\nHost:127.0.0.1:{port}\r\n{header}:{prefix}{}\r\n\r\n",
                        module.id,
                        "x".repeat(token_len),
                    );
                    if path.chars().any(char::is_whitespace)
                        || !matches!(request_head_end(head.as_bytes(), 0), Ok(Some(_)))
                    {
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
            .insert(module.id.clone(), module);
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
