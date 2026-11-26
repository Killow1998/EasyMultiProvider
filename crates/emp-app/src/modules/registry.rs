use super::process::{DataEndpoint, ModuleProcess, ValidatedManifest};
use crate::http::{
    auth::SESSION_HEADER,
    request::{MODULE_RUNTIME_PREFIX, REQUEST_VERSION, request_head_end},
};
use axum::http::HeaderValue;
use emp_modules::{
    manifest::{Contribution, HookPhase, RouteAuth},
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
    pub(crate) data_endpoint: Option<DataEndpoint>,
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
    pub(crate) id: HeaderValue,
    pub(crate) auth: RouteAuth,
}

pub(crate) fn shortest_credential_header(auth: RouteAuth, local_proxy_key_len: usize) -> String {
    let (header, prefix, token_len) = match auth {
        RouteAuth::Management => (SESSION_HEADER, "", emp_state::WEB_SESSION_TOKEN_LENGTH),
        RouteAuth::LocalProxy => ("Authorization", "Bearer ", local_proxy_key_len),
        RouteAuth::Proxy => ("Authorization", "Bearer ", 1),
    };
    format!("{header}:{prefix}{}", "x".repeat(token_len))
}

impl ModuleRegistry {
    pub fn publish(
        &self,
        manifest: ValidatedManifest,
        generation: u64,
        process: &ModuleProcess,
        port: u16,
        local_proxy_key_len: usize,
    ) -> Result<(), String> {
        let (id, contributions) = manifest.into_parts();
        let mut module = ActiveModule {
            id: Arc::from(id),
            generation,
            connection: process.connection(),
            data_endpoint: process.data_endpoint.clone(),
            hooks: BTreeSet::new(),
            catalog: false,
            panels: BTreeMap::new(),
            routes: BTreeMap::new(),
        };
        for contribution in contributions {
            match contribution {
                Contribution::HttpRoute {
                    id,
                    method,
                    path,
                    auth,
                } => {
                    let credential = shortest_credential_header(auth, local_proxy_key_len);
                    let head = format!(
                        "{method} {MODULE_RUNTIME_PREFIX}{}{path} {REQUEST_VERSION}\r\nHost:127.0.0.1:{port}\r\n{credential}\r\n\r\n",
                        module.id,
                    );
                    if path.chars().any(char::is_whitespace)
                        || !matches!(request_head_end(head.as_bytes(), 0), Ok(Some(_)))
                    {
                        return Err(format!(
                            "route {} cannot be represented in an HTTP request",
                            String::from_utf8_lossy(id.as_bytes())
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

    #[allow(
        dead_code,
        reason = "hook snapshots are used by the unwired dispatcher"
    )]
    pub(crate) fn hooks(&self) -> Vec<Arc<ActiveModule>> {
        self.active
            .lock()
            .expect("module registry")
            .values()
            .filter(|module| !module.hooks.is_empty())
            .cloned()
            .collect()
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
