mod catalog;

pub(crate) use catalog::{augment_catalog, augment_public, validate_update};

use super::{ActiveWorker, Modules};
use crate::{
    app::ServerState,
    server::ServerHooks,
    services::{
        connection_admission::{BusinessToken, ConnectionSlot},
        gateway_clients::Caller,
        updates::RequestPermit,
    },
};
use emp_modules::{Command, Module};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(super) struct Endpoint {
    pub(crate) url: String,
    pub(crate) inbound_key: String,
    profiles: Value,
}

type ActiveCalls = Mutex<BTreeMap<String, (CallerDelegation, Option<CallLease>)>>;

pub(crate) struct Callers {
    active: Arc<ActiveCalls>,
    pending_headers: Arc<tokio::sync::Semaphore>,
}

impl Default for Callers {
    fn default() -> Self {
        Self {
            active: Arc::default(),
            pending_headers: Arc::new(tokio::sync::Semaphore::new(64)),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CallLease {
    pub(crate) connection: Arc<ConnectionSlot>,
    pub(crate) work: Arc<RequestPermit>,
}

pub(crate) struct CallbackLease {
    pub(crate) business: BusinessToken,
    pub(crate) work: Arc<RequestPermit>,
}

#[derive(Clone)]
pub(crate) struct CallerDelegation {
    pub(crate) caller: Caller,
    pub(crate) authorization: Option<String>,
}

pub(crate) enum CallParent {
    Request(BusinessToken),
    Connection(Arc<ConnectionSlot>),
}

pub(crate) struct Call {
    active: Option<(Arc<ActiveCalls>, CallParent)>,
    pub(crate) key: String,
}

impl Callers {
    pub(crate) fn delegate(
        &self,
        caller: Caller,
        authorization: Option<String>,
        lease: Option<CallLease>,
        parent: impl FnOnce() -> CallParent,
    ) -> Option<Call> {
        let key = crate::util::random_hex(32).ok()?;
        let mut calls = self.active.lock().ok()?;
        let parent = parent();
        calls.insert(
            key.clone(),
            (
                CallerDelegation {
                    caller,
                    authorization,
                },
                lease,
            ),
        );
        Some(Call {
            active: Some((self.active.clone(), parent)),
            key,
        })
    }

    pub(crate) fn classifies_callbacks(&self) -> bool {
        self.active
            .lock()
            .is_ok_and(|calls| calls.values().any(|(_, lease)| lease.is_some()))
    }

    pub(crate) fn acquire_header(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.pending_headers.clone().try_acquire_owned().ok()
    }

    pub(crate) fn identify(&self, key: &str) -> Option<CallerDelegation> {
        let calls = self.active.lock().ok()?;
        let (identity, _) = calls.get(key)?;
        Some(identity.clone())
    }

    pub(crate) fn identify_callback(
        &self,
        key: &str,
        hooks: &ServerHooks,
    ) -> Option<(CallerDelegation, Option<CallbackLease>)> {
        let calls = hooks.lock_callback_delegations(&self.active).ok()?;
        let (identity, lease) = calls.get(key)?;
        Some((
            identity.clone(),
            lease.as_ref().map(|lease| CallbackLease {
                business: lease.connection.admit_callback(),
                work: lease.work.clone(),
            }),
        ))
    }
}

impl Call {
    pub(crate) fn finish_with(&mut self, publish: impl FnOnce()) {
        let Some((active, parent)) = self.active.take() else {
            publish();
            return;
        };
        let mut calls = active.lock().unwrap_or_else(|error| error.into_inner());
        let handoff = || {
            let revoked = calls.remove(&self.key);
            publish();
            revoked
        };
        let revoked = match parent {
            CallParent::Request(business) => business.finish_with(handoff),
            CallParent::Connection(slot) => slot.with_admission(handoff),
        };
        drop(calls);
        drop(revoked);
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.finish_with(|| ());
    }
}

pub(crate) fn route_name(profile: &Value) -> String {
    format!(
        "aero/{}",
        profile["name"].as_str().expect("normalized aero name")
    )
}

pub(crate) fn profile<'a>(config: &'a Value, model: &str) -> Option<&'a Value> {
    let matches =
        |route: &str| route == model || emp_state::config::client_model_id(route).as_ref() == model;
    let profile = config["aero_profiles"].as_array()?.iter().find(|profile| {
        matches(&route_name(profile))
            || profile["route_names"]
                .as_array()
                .expect("normalized aero aliases")
                .iter()
                .any(|route| matches(route.as_str().expect("normalized aero alias")))
    })?;
    let existing = emp_state::config::resolve_client_model(config, model);
    let existing = existing.as_deref().unwrap_or(model);
    (matches!(
        emp_domain::resolve_route_without_catalog(config, existing),
        Err(emp_domain::RouteResolutionError::UnknownModel(_))
    ) && !config["model_groups"]
        .as_array()
        .is_some_and(|groups| groups.iter().any(|group| group["id"] == existing)))
    .then_some(profile)
}

pub(crate) struct Relay {
    pub(crate) url: String,
    pub(crate) inbound_key: String,
    _worker: Arc<ActiveWorker>,
}

impl Modules {
    pub(crate) fn aero(&self, state: &ServerState, config: &Value) -> Result<Relay, String> {
        let worker = self.worker(Module::Aero).ok_or("module_disabled")?;
        let mut endpoint = worker.aero.lock().map_err(|_| "module_unavailable")?;
        let mut profiles = BTreeMap::new();
        for profile in config["aero_profiles"].as_array().into_iter().flatten() {
            let roles = json!({"planner":profile["planner"],"executor":profile["executor"]});
            let name = route_name(profile);
            let aliases = profile["route_names"]
                .as_array()
                .expect("normalized aero aliases");
            for alias in std::iter::once(name.as_str()).chain(
                aliases
                    .iter()
                    .map(|alias| alias.as_str().expect("normalized aero alias")),
            ) {
                profiles.insert(alias.to_owned(), roles.clone());
                let encoded = emp_state::config::client_model_id(alias);
                if encoded.as_ref() != alias {
                    profiles.insert(encoded.into_owned(), roles.clone());
                }
            }
        }
        let profiles = json!(profiles);
        match endpoint.as_mut() {
            Some(endpoint) if endpoint.profiles != profiles => {
                worker.call(&Command::AeroConfigure {
                    profiles: profiles.clone(),
                })?;
                endpoint.profiles = profiles;
            }
            Some(_) => {}
            None => {
                let inbound_key = crate::util::random_hex(32).map_err(|_| "module_unavailable")?;
                let upstream_key = crate::util::random_hex(32).map_err(|_| "module_unavailable")?;
                let state_dir = state
                    .backend
                    .configuration
                    .config_path
                    .parent()
                    .expect("configuration directory")
                    .join("aero");
                emp_state::filesystem::create_private_directory(&state_dir)
                    .map_err(|_| "module_directory_unavailable")?;
                let reply = worker.call(&Command::AeroStart {
                    state_dir,
                    core_base_url: format!("http://127.0.0.1:{}", state.port),
                    inbound_key: inbound_key.clone(),
                    upstream_key,
                    profiles: profiles.clone(),
                })?;
                #[derive(Deserialize)]
                struct Started {
                    endpoint: String,
                }
                let started: Started =
                    serde_json::from_value(reply).map_err(|_| "invalid_module_reply")?;
                let url = url::Url::parse(&started.endpoint).map_err(|_| "invalid_module_reply")?;
                if url.scheme() != "http"
                    || url.host_str() != Some("127.0.0.1")
                    || url.port().is_none()
                    || url.path() != "/v1"
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err("invalid_module_reply".into());
                }
                *endpoint = Some(Endpoint {
                    url: url.into(),
                    inbound_key,
                    profiles,
                });
            }
        }
        let endpoint = endpoint.as_ref().expect("started Aero endpoint");
        Ok(Relay {
            url: endpoint.url.clone(),
            inbound_key: endpoint.inbound_key.clone(),
            _worker: Arc::clone(&worker),
        })
    }
}
