mod access;
mod budget;
pub(crate) use access::RequestDenied;
mod budget_notices;
mod commands;
pub(crate) use commands::Command;
mod discovery;
mod sharing;
mod subscriptions;
mod transport;

use crate::app::ServerState;
use emp_modules::{
    Module,
    team::{BudgetStatuses, IdentityKey, IdentityPlan, Operation, ReportPlan, Request},
    workspace::Role,
};
use emp_state::config::is_team_service_id;
use emp_state::{FilesystemError, VaultStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Mutex, time::Duration};
use transport::{policy, request, send};

#[derive(Clone, Deserialize, Serialize)]
struct Session {
    base: String,
    token: String,
    identity: Value,
}

impl Session {
    fn workspace(&self) -> &str {
        self.identity["workspace"]["id"]
            .as_str()
            .expect("workspace ID")
    }

    fn member(&self) -> &str {
        self.identity["member"]["id"]
            .as_str()
            .expect("workspace member ID")
    }

    fn scope(&self, device: &str) -> String {
        format!("{}:{}:{device}", self.workspace(), self.member())
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Imported {
    workspace: String,
    share: String,
    version: i64,
    available: bool,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_identity: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, Default)]
struct Store {
    #[serde(default, skip_serializing)]
    device: String,
    session: Option<Session>,
    imported: BTreeMap<String, Imported>,
    #[serde(skip)]
    recovery_required: bool,
}

#[derive(Deserialize)]
struct ReportReceipt {
    inserted: usize,
    budget: BudgetStatuses,
}

pub(crate) struct TeamState {
    path: PathBuf,
    store: Mutex<Option<Store>>,
    configured_shares: std::collections::BTreeSet<String>,
    operations: Mutex<()>,
    background: Mutex<()>,
    events: tokio::sync::Notify,
    discovery: discovery::Discovery,
    budget_notices: budget_notices::Notices,
}

type Result<T> = std::result::Result<T, Failure>;

pub(crate) struct Failure(pub(crate) u16, pub(crate) String);

impl Failure {
    fn unavailable() -> Self {
        Self(503, "workspace_local_storage_unavailable".into())
    }

    fn invalid() -> Self {
        Self(400, "invalid_workspace_request".into())
    }

    fn signed_out() -> Self {
        Self(401, "workspace_login_required".into())
    }
}

impl TeamState {
    pub(crate) fn open(path: PathBuf, vault: &VaultStore, config: &Value) -> Self {
        let configured_shares: std::collections::BTreeSet<String> = config["accounts"]
            .as_array()
            .expect("normalized accounts")
            .iter()
            .filter(|entry| entry["workspace_shared"] == true)
            .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
            .collect();
        let damaged_store = || Store {
            recovery_required: true,
            ..Store::default()
        };
        let mut store: Option<Store> = match path.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(Store {
                recovery_required: !configured_shares.is_empty(),
                ..Store::default()
            }),
            _ => match vault.read_encrypted_json(&path) {
                Ok(raw) => Some(serde_json::from_value(raw).unwrap_or_else(|_| damaged_store())),
                Err(
                    FilesystemError::UnsupportedCredentialFormat
                    | FilesystemError::CredentialDecryptFailed
                    | FilesystemError::InvalidJson,
                ) => Some(damaged_store()),
                Err(_) => None,
            },
        };
        if let Some(store) = &mut store
            && store.device.is_empty()
            && let Ok(device) = std::fs::read_to_string(path.with_file_name("team-device-id"))
            && valid_identifier(&device)
        {
            store.device = device;
        }
        if let Some(store) = &mut store
            && store.session.as_ref().is_some_and(|session| {
                [
                    &session.identity["workspace"]["id"],
                    &session.identity["member"]["id"],
                ]
                .into_iter()
                .any(|id| !id.as_str().is_some_and(valid_identifier))
            })
        {
            store.session = None;
            store.recovery_required = true;
        }
        if let Some(store) = &mut store {
            for account in config["accounts"].as_array().expect("normalized accounts") {
                if account["workspace_shared"] == true
                    && let Some(id) = account["id"].as_str()
                    && let Some(record) = store.imported.get_mut(id)
                    && record.account_identity.is_none()
                    && record.expires_at.is_some()
                {
                    record.account_identity = subscriptions::stored_identity(vault, account);
                }
            }
        }
        let budget_notices =
            budget_notices::Notices::open(path.with_file_name("team-budget-notices.json"));
        Self {
            path,
            store: Mutex::new(store),
            configured_shares,
            operations: Mutex::new(()),
            background: Mutex::new(()),
            events: Default::default(),
            discovery: Default::default(),
            budget_notices,
        }
    }

    fn snapshot(&self) -> Result<Store> {
        self.store
            .lock()
            .map_err(|_| Failure::unavailable())?
            .clone()
            .ok_or_else(Failure::unavailable)
    }

    fn write(
        &self,
        vault: &VaultStore,
        store: &Store,
    ) -> std::result::Result<(), emp_state::FilesystemError> {
        self.persist_device(&store.device)?;
        vault.write_encrypted_json(
            &self.path,
            &serde_json::to_value(store).expect("workspace profile is serializable"),
        )
    }

    fn persist_device(&self, device: &str) -> std::result::Result<(), emp_state::FilesystemError> {
        if device.is_empty() {
            return Ok(());
        }
        emp_state::atomic_write_private_state(
            &self.path.with_file_name("team-device-id"),
            device.as_bytes(),
        )
    }

    fn save_store(&self, state: &ServerState, store: Store) -> Result<()> {
        self.write(&state.backend.configuration.vault, &store)
            .map_err(|_| Failure::unavailable())?;
        *self.store.lock().map_err(|_| Failure::unavailable())? = Some(store);
        if self.migration_restricted() {
            state.backend.lan_migration.cancel();
        }
        Ok(())
    }

    pub(crate) fn migration_restricted(&self) -> bool {
        self.store.lock().map_or(true, |store| {
            store.as_ref().is_none_or(|store| {
                store.session.as_ref().map_or(
                    store.recovery_required || !store.imported.is_empty(),
                    |session| !can_migrate(&session.identity),
                )
            })
        })
    }

    pub(crate) fn check_migration(&self, state: &ServerState) -> Result<()> {
        let denied = || Failure(403, "workspace_migration_permission_required".into());
        if self.migration_restricted() {
            return Err(denied());
        }
        if self.local_status()?["joined"] == true {
            let _operation = self.operations.lock().map_err(|_| Failure::unavailable())?;
            if !self
                .refresh_status(state, self.snapshot()?)?
                .migration_allowed
            {
                return Err(denied());
            }
        }
        Ok(())
    }

    pub(crate) fn change_module(
        &self,
        state: &ServerState,
        change: impl FnOnce() -> std::result::Result<(), String>,
    ) -> std::result::Result<(), String> {
        let _operation = self.operations.lock().map_err(|_| "module_unavailable")?;
        let _background = self.background.lock().map_err(|_| "module_unavailable")?;
        change()?;
        if !state.backend.modules.active(Module::Team) {
            self.discovery.stop();
        }
        self.event_ready();
        Ok(())
    }

    pub(crate) fn local_status(&self) -> Result<Value> {
        let store = self.store.lock().map_err(|_| Failure::unavailable())?;
        let Some(store) = store.as_ref() else {
            return Err(Failure::unavailable());
        };
        Ok(match &store.session {
            Some(session) => {
                let mut identity = session.identity.clone();
                identity
                    .as_object_mut()
                    .expect("workspace identity")
                    .remove("native_usage");
                json!({"joined":true,"base":session.base,"identity":identity})
            }
            None => {
                json!({"joined":false,"recovery_required":store.recovery_required || !store.imported.is_empty()})
            }
        })
    }

    pub(crate) fn notices(&self) -> Value {
        let Ok(store) = self.store.lock() else {
            return json!([]);
        };
        match store.as_ref().and_then(|store| store.session.as_ref()) {
            Some(session) => self.budget_notices.history(session),
            None => json!([]),
        }
    }

    #[cfg(windows)]
    pub(crate) fn take_desktop_notice(&self) -> Option<(String, String)> {
        self.budget_notices.take_desktop()
    }

    fn refresh_status(&self, state: &ServerState, mut store: Store) -> Result<IdentityPlan> {
        let session = store.session.as_mut().ok_or_else(Failure::signed_out)?;
        let (identity, plan) = remote_identity(state, session)?;
        if session.identity != identity {
            let membership_changed = session.identity["workspace"] != identity["workspace"]
                || session.identity["member"] != identity["member"];
            session.identity = identity;
            self.save_store(state, store)?;
            if membership_changed {
                super::catalog::refresh_catalog(state).map_err(|_| Failure::unavailable())?;
            }
        }
        Ok(plan)
    }

    pub(crate) fn event_ready(&self) {
        self.events.notify_one();
    }

    pub(crate) fn run(state: &ServerState) {
        let team = &state.backend.team;
        let runtime = &state.backend.transport.runtime;
        let mut next_budget_check = std::time::Instant::now();
        loop {
            let active = state.backend.modules.active(Module::Team);
            let stopped = runtime.block_on(async {
                tokio::select! {
                    () = state.wait_for_shutdown() => true,
                    () = team.events.notified() => false,
                    () = tokio::time::sleep(Duration::from_secs(15)), if active => false,
                }
            });
            if stopped {
                break;
            }
            let Ok(_background) = team.background.lock() else {
                break;
            };
            if !state.backend.modules.active(Module::Team) {
                next_budget_check = std::time::Instant::now();
                continue;
            }
            let _ = team.flush(state);
            if std::time::Instant::now() >= next_budget_check {
                next_budget_check = std::time::Instant::now() + Duration::from_secs(60);
                if let Ok(_operation) = team.operations.try_lock()
                    && let Ok(store) = team.snapshot()
                    && store.session.is_some()
                {
                    let _ = team.refresh_status(state, store);
                }
                if let Ok(store) = team.snapshot()
                    && let Some(session) = store.session
                    && let Ok(reply) = request(
                        state,
                        &session.base,
                        Some(&session.token),
                        Operation::BudgetStatus,
                        None,
                    )
                    && let Ok(budget) = serde_json::from_value(reply)
                {
                    team.observe_budget(state, &session, budget);
                    let _ = request(
                        state,
                        &session.base,
                        Some(&session.token),
                        Operation::Heartbeat,
                        None,
                    );
                }
            }
        }
        team.discovery.stop();
    }

    fn flush(&self, state: &ServerState) -> Result<()> {
        let store = self.snapshot()?;
        if let Some(session) = store.session {
            self.flush_batch(state, &session, &session.scope(&store.device))?;
        }
        Ok(())
    }

    fn flush_batch(&self, state: &ServerState, session: &Session, scope: &str) -> Result<bool> {
        let ledger = &state.backend.usage.ledger;
        let events = ledger
            .team_events(scope)
            .map_err(|_| Failure::unavailable())?;
        if events.is_empty() {
            return Ok(false);
        }
        let report_events = events
            .iter()
            .map(|(_, event)| serde_json::from_value(event.clone()))
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| Failure::unavailable())?;
        let plan: ReportPlan = policy(
            state,
            Request::Report {
                base: session.base.clone(),
                events: report_events,
            },
        )?;
        let report = serde_json::to_vec(&plan.report).expect("workspace report");
        let reply = send(state, &plan.endpoint, Some(&session.token), Some(report))?;
        let receipt: ReportReceipt = serde_json::from_value(reply)
            .map_err(|_| Failure(502, "invalid_workspace_reply".into()))?;
        if receipt.inserted > events.len() {
            return Err(Failure(502, "invalid_workspace_reply".into()));
        }
        self.observe_budget(state, session, receipt.budget);
        let ids: Vec<_> = events.iter().map(|(id, _)| id.as_str()).collect();
        ledger
            .ack_team_events(scope, &ids)
            .map_err(|_| Failure::unavailable())?;
        Ok(true)
    }

    fn observe_budget(&self, state: &ServerState, session: &Session, budget: BudgetStatuses) {
        let english = state
            .backend
            .configuration
            .read()
            .is_ok_and(|config| config["language"] == "en");
        if let Ok(store) = self.store.lock()
            && store
                .as_ref()
                .and_then(|store| store.session.as_ref())
                .is_some_and(|current| current.token == session.token)
        {
            self.budget_notices
                .observe(state, english, session, budget.daily);
            if let Some(weekly) = budget.weekly {
                self.budget_notices.observe(state, english, session, weekly);
            }
        }
    }
}

impl Imported {
    fn usable(&self) -> bool {
        self.available
            && self
                .expires_at
                .is_none_or(|expiry| expiry > crate::util::system_now() as i64)
    }
}

fn can_migrate(identity: &Value) -> bool {
    matches!(
        identity["member"]["role"].as_str(),
        Some("team_admin" | "vps_admin")
    )
}

fn valid_identifier(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn identity_key(identity: &Value) -> IdentityKey {
    IdentityKey {
        workspace: identity["workspace"]["id"].as_str().map(str::to_owned),
        member: identity["member"]["id"].as_str().map(str::to_owned),
    }
}

fn plan_identity(
    state: &ServerState,
    current: Option<&Session>,
    identity: &Value,
) -> Result<IdentityPlan> {
    let role = match identity["member"]["role"].as_str() {
        Some("vps_admin") => Some(Role::VpsAdmin),
        Some("team_admin") => Some(Role::TeamAdmin),
        Some("member") => Some(Role::Member),
        _ => None,
    };
    policy(
        state,
        Request::Identity {
            current: current.map(|session| identity_key(&session.identity)),
            received: identity_key(identity),
            role,
            sync: identity["member"]["sync"] == true,
        },
    )
}

fn remote_identity(state: &ServerState, session: &Session) -> Result<(Value, IdentityPlan)> {
    let identity = request(
        state,
        &session.base,
        Some(&session.token),
        Operation::Status,
        None,
    )?;
    let plan = plan_identity(state, Some(session), &identity)?;
    Ok((identity, plan))
}
