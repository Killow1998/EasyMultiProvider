mod budget_notices;
mod discovery;
mod sharing;
mod subscriptions;
mod transport;

use crate::app::ServerState;
use emp_modules::{
    Module,
    team::{BudgetStatus, IdentityKey, IdentityPlan, Operation, ReportPlan, Request},
    workspace::Role,
};
use emp_state::VaultStore;
use emp_state::config::is_team_service_id;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Mutex, time::Duration};
use transport::{plan_endpoint, policy, request, send};

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
}

#[derive(Clone, Deserialize, Serialize, Default)]
struct Store {
    #[serde(default, skip_serializing)]
    device: String,
    session: Option<Session>,
    imported: BTreeMap<String, Imported>,
}

pub(crate) struct TeamState {
    path: PathBuf,
    store: Mutex<Option<Store>>,
    configured_shares: std::collections::BTreeSet<String>,
    operations: Mutex<()>,
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
        let mut store: Option<Store> = match path.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(Store::default()),
            _ => vault
                .read_encrypted_json(&path)
                .ok()
                .and_then(|raw| serde_json::from_value(raw).ok()),
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
        }
        let budget_notices =
            budget_notices::Notices::open(path.with_file_name("team-budget-notices.json"));
        Self {
            path,
            store: Mutex::new(store),
            configured_shares: config["accounts"]
                .as_array()
                .expect("normalized accounts")
                .iter()
                .filter(|entry| entry["workspace_shared"] == true)
                .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
                .collect(),
            operations: Mutex::new(()),
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
                store
                    .session
                    .as_ref()
                    .is_some_and(|session| !can_migrate(&session.identity))
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

    pub(crate) fn remove_module_data(&self, state: &ServerState) -> Result<()> {
        let _operation = self.operations.lock().map_err(|_| Failure::unavailable())?;
        self.purge_local(state)
    }

    pub(crate) fn local_status(&self) -> Result<Value> {
        let store = self.store.lock().map_err(|_| Failure::unavailable())?;
        Ok(
            match store.as_ref().and_then(|store| store.session.as_ref()) {
                Some(session) => {
                    json!({"joined":true,"base":session.base,"identity":session.identity})
                }
                None => json!({"joined":false}),
            },
        )
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

    pub(crate) fn command(
        &self,
        state: &ServerState,
        operation: &str,
        body: &Value,
    ) -> Result<Value> {
        if operation == "discover" {
            return self.discovery.find(state);
        }
        let _operation = self.operations.lock().map_err(|_| Failure::unavailable())?;
        let mut store = self.snapshot()?;
        if operation == "join" {
            let base = body["base"].as_str().ok_or_else(Failure::invalid)?;
            return request(state, base, None, Operation::Join, Some(body));
        }
        if operation == "login" {
            let base = body["base"].as_str().ok_or_else(Failure::invalid)?;
            let name = body["name"].as_str().ok_or_else(Failure::invalid)?;
            let password = body["password"].as_str().ok_or_else(Failure::invalid)?;
            if store.device.is_empty() {
                store.device = crate::util::random_hex(16).map_err(|_| Failure::unavailable())?;
            }
            let mut identity = request(
                state,
                base,
                None,
                Operation::Login,
                Some(&json!({
                    "name":name,"password":password,"device":store.device,
                })),
            )?;
            let token = identity
                .as_object_mut()
                .and_then(|value| value.remove("token"))
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(Failure::unavailable)?;
            plan_identity(state, None, &identity)?;
            store.session = Some(Session {
                base: base.to_owned(),
                token,
                identity,
            });
            self.save_store(state, store)?;
            self.events.notify_one();
            super::catalog::refresh_catalog(state).map_err(|_| Failure::unavailable())?;
            return self.local_status();
        }
        let session = store.session.as_mut().ok_or_else(Failure::signed_out)?;
        let base = session.base.as_str();
        let token = Some(session.token.as_str());
        let response = match operation {
            "status" => {
                self.refresh_status(state, store)?;
                return self.local_status();
            }
            "logout" | "handover" => {
                if operation == "handover" {
                    request(state, base, token, Operation::Handover, Some(body))?;
                } else if session.identity["member"]["role"] != "member" {
                    return Err(Failure(403, "workspace_handover_required".into()));
                }
                if let Err(error) = request(state, base, token, Operation::Logout, Some(&json!({})))
                    && error.0 < 500
                    && error.0 != 401
                {
                    return Err(error);
                }
                self.purge_local(state)?;
                return self.local_status();
            }
            "publish" => return self.publish(state, session, body),
            "sync" => return self.sync(state, store),
            "usage" => {
                self.events.notify_one();
                if let Some(period) = body.get("period") {
                    request(
                        state,
                        base,
                        token,
                        Operation::UsageFiltered,
                        Some(&json!({"period":period})),
                    )?
                } else {
                    request(state, base, token, Operation::Usage, None)?
                }
            }
            "members" => request(state, base, token, Operation::Members, Some(body))?,
            "member_access" => request(state, base, token, Operation::MemberAccess, Some(body))?,
            "reject_member" => request(state, base, token, Operation::RejectMember, Some(body))?,
            "budget" => request(state, base, token, Operation::Budget, Some(body))?,
            "review_service" => request(state, base, token, Operation::ReviewService, Some(body))?,
            "member_tiers" => request(state, base, token, Operation::MemberTiers, Some(body))?,
            "profile" => request(state, base, token, Operation::Profile, Some(body))?,
            "profile_visibility" => {
                request(state, base, token, Operation::ProfileVisibility, Some(body))?
            }
            "subscription_reminder" => request(
                state,
                base,
                token,
                Operation::SubscriptionReminder,
                Some(body),
            )?,
            "profiles" => request(state, base, token, Operation::Profiles, None)?,
            "overview" => request(state, base, token, Operation::Overview, None)?,
            "audit" => request(state, base, token, Operation::Audit, Some(body))?,
            "backup" => request(state, base, token, Operation::Backup, None)?,
            "subscriptions" => request(state, base, token, Operation::Subscriptions, None)?,
            "list_members" => request(state, base, token, Operation::ListMembers, None)?,
            "shared_services" => request(state, base, token, Operation::SharedServices, None)?,
            "join_code" => request(state, base, token, Operation::JoinCode, None)?,
            "issue_invite" => {
                let endpoint = plan_endpoint(state, base, Operation::IssueInvite)?;
                let reply = send(state, &endpoint, token, Some(b"{}".to_vec()))?;
                if let Some(origin) = endpoint.announcement_origin {
                    let _ = self.discovery.announce(state, &origin, &reply);
                }
                reply
            }
            _ => return Err(Failure::invalid()),
        };
        Ok(response)
    }

    fn refresh_status(&self, state: &ServerState, mut store: Store) -> Result<IdentityPlan> {
        let session = store.session.as_mut().ok_or_else(Failure::signed_out)?;
        let (identity, plan) = remote_identity(state, session)?;
        if session.identity != identity {
            session.identity = identity;
            self.save_store(state, store)?;
            super::catalog::refresh_catalog(state).map_err(|_| Failure::unavailable())?;
        }
        Ok(plan)
    }

    pub(crate) fn scope(&self, provider: &str) -> Option<Value> {
        let store = self.store.lock().ok()?;
        let store = store.as_ref()?;
        let source = store
            .imported
            .get(provider)
            .filter(|source| source.usable())?;
        let session = store.session.as_ref().filter(|session| {
            session.workspace() == source.workspace && session.identity["member"]["sync"] == true
        })?;
        Some(json!({
            "scope":session.scope(&store.device),
            "share":source.share,
        }))
    }

    pub(crate) fn check_route(
        &self,
        provider: &str,
        active: bool,
        tier: Option<&str>,
    ) -> std::result::Result<(), &'static str> {
        if !is_team_service_id(provider) {
            return Ok(());
        }
        let store = self
            .store
            .lock()
            .map_err(|_| "Team workspace is unavailable")?;
        let store = store.as_ref().ok_or("Team workspace is unavailable")?;
        let Some(record) = store.imported.get(provider) else {
            return if self.configured_shares.contains(provider) {
                Err("This team's service is not currently shared with you")
            } else {
                Ok(())
            };
        };
        if record.expires_at.is_none() {
            return Err("This team's service is not currently shared with you");
        }
        let session = store
            .session
            .as_ref()
            .filter(|session| {
                active
                    && record.usable()
                    && session.workspace() == record.workspace
                    && session.identity["member"]["sync"] == true
            })
            .ok_or("This team's service is not currently shared with you")?;
        let member = &session.identity["member"];
        match tier {
            Some("ultrafast") if member["ultrafast"] != true => {
                Err("Ultrafast is not permitted for this workspace member")
            }
            Some("priority" | "fast") if member["fast"] != true => {
                Err("Fast is not permitted for this workspace member")
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn unavailable_providers<'a>(
        &self,
        active: bool,
        config: &'a Value,
    ) -> Vec<&'a str> {
        ["accounts", "providers"]
            .into_iter()
            .flat_map(|kind| config[kind].as_array().expect("normalized services"))
            .filter_map(|entry| entry["id"].as_str())
            .filter(|id| is_team_service_id(id) && self.check_route(id, active, None).is_err())
            .collect()
    }

    pub(crate) fn shared_subscription(&self, id: &str) -> bool {
        is_team_service_id(id)
            && (self.configured_shares.contains(id)
                || self.store.lock().map_or(true, |store| {
                    store
                        .as_ref()
                        .is_none_or(|store| store.imported.contains_key(id))
                }))
    }

    pub(crate) fn label_subscription_permissions(&self, accounts: &mut Value) {
        let Ok(store) = self.store.lock() else {
            return;
        };
        let Some(store) = store.as_ref() else { return };
        for account in accounts.as_array_mut().expect("public account list") {
            let Some(id) = account["id"].as_str().filter(|id| {
                store.imported.contains_key(*id) || self.configured_shares.contains(*id)
            }) else {
                continue;
            };
            let can_reset = store.imported.get(id).is_some_and(|record| {
                record.usable()
                    && store.session.as_ref().is_some_and(|session| {
                        session.workspace() == record.workspace
                            && session.identity["member"]["sync"] == true
                            && matches!(
                                session.identity["member"]["role"].as_str(),
                                Some("team_admin" | "vps_admin")
                            )
                    })
            });
            account["team_shared"] = json!(true);
            account["team_reset_allowed"] = json!(can_reset);
        }
    }

    pub(crate) fn shared_subscription_reset(&self, state: &ServerState, id: &str) -> bool {
        if !self.shared_subscription(id) {
            return true;
        }
        let Ok(store) = self.snapshot() else {
            return false;
        };
        let Some(record) = store.imported.get(id) else {
            return false;
        };
        let Some(session) = store.session.filter(|session| {
            record.usable()
                && session.workspace() == record.workspace
                && matches!(
                    session.identity["member"]["role"].as_str(),
                    Some("team_admin" | "vps_admin")
                )
        }) else {
            return false;
        };
        state.backend.modules.active(Module::Team)
            && request(
                state,
                &session.base,
                Some(&session.token),
                Operation::ResetAuthority,
                Some(&json!({"share":record.share,"version":record.version})),
            )
            .is_ok_and(|reply| reply["workspace"] == record.workspace)
    }

    pub(crate) fn event_ready(&self) {
        self.events.notify_one();
    }

    pub(crate) fn run(state: &ServerState) {
        let team = &state.backend.team;
        let runtime = &state.backend.transport.runtime;
        let mut next_budget_check = std::time::Instant::now();
        loop {
            let stopped = runtime.block_on(async {
                tokio::select! {
                    () = state.wait_for_shutdown() => true,
                    () = team.events.notified() => false,
                    () = tokio::time::sleep(Duration::from_secs(15)) => false,
                }
            });
            if stopped {
                break;
            }
            let _ = team.flush(state);
            if std::time::Instant::now() >= next_budget_check {
                next_budget_check = std::time::Instant::now() + Duration::from_secs(60);
                if let Ok(store) = team.snapshot()
                    && let Some(session) = store.session
                    && let Ok(budget) = request(
                        state,
                        &session.base,
                        Some(&session.token),
                        Operation::BudgetStatus,
                        None,
                    )
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
        let Some(session) = store.session else {
            return Ok(());
        };
        let scope = session.scope(&store.device);
        let ledger = &state.backend.usage.ledger;
        let events = ledger
            .team_events(&scope)
            .map_err(|_| Failure::unavailable())?;
        if events.is_empty() {
            return Ok(());
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
        self.observe_budget(state, &session, reply["budget"].clone());
        let ids: Vec<_> = events.iter().map(|(id, _)| id.as_str()).collect();
        ledger
            .ack_team_events(&scope, &ids)
            .map_err(|_| Failure::unavailable())
    }

    fn observe_budget(&self, state: &ServerState, session: &Session, budget: Value) {
        let Ok(budget) = serde_json::from_value::<BudgetStatus>(budget) else {
            return;
        };
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
            self.budget_notices.observe(state, english, session, budget);
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
