use super::{
    Failure, Result, Session, TeamState, plan_identity,
    transport::{plan_endpoint, request, send},
};
use crate::{app::ServerState, services::catalog};
use emp_modules::{Module, team::Operation};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(crate) enum Command {
    Discover,
    Join,
    Login,
    Status,
    Logout,
    Handover,
    Publish,
    Sync,
    Usage,
    IssueInvite,
    Forward { operation: Operation, body: bool },
}

impl Command {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        let (operation, body) = match name {
            "discover" => return Some(Self::Discover),
            "join" => return Some(Self::Join),
            "login" => return Some(Self::Login),
            "status" => return Some(Self::Status),
            "logout" => return Some(Self::Logout),
            "handover" => return Some(Self::Handover),
            "publish" => return Some(Self::Publish),
            "sync" => return Some(Self::Sync),
            "usage" => return Some(Self::Usage),
            "issue_invite" => return Some(Self::IssueInvite),
            "members" => (Operation::Members, true),
            "member_access" => (Operation::MemberAccess, true),
            "reject_member" => (Operation::RejectMember, true),
            "remove_member" => (Operation::RemoveMember, true),
            "budget" => (Operation::Budget, true),
            "weekly_budget" => (Operation::WeeklyBudget, true),
            "review_service" => (Operation::ReviewService, true),
            "remove_service" => (Operation::RemoveService, true),
            "member_tiers" => (Operation::MemberTiers, true),
            "profile" => (Operation::Profile, true),
            "profile_visibility" => (Operation::ProfileVisibility, true),
            "subscription_reminder" => (Operation::SubscriptionReminder, true),
            "audit" => (Operation::Audit, true),
            "profiles" => (Operation::Profiles, false),
            "overview" => (Operation::Overview, false),
            "backup" => (Operation::Backup, false),
            "subscriptions" => (Operation::Subscriptions, false),
            "list_members" => (Operation::ListMembers, false),
            "shared_services" => (Operation::SharedServices, false),
            "join_code" => (Operation::JoinCode, false),
            _ => return None,
        };
        Some(Self::Forward { operation, body })
    }
}

impl TeamState {
    pub(crate) fn command(
        &self,
        state: &ServerState,
        command: Option<Command>,
        body: &Value,
    ) -> Result<Value> {
        let _operation = self.operations.lock().map_err(|_| Failure::unavailable())?;
        if !state.backend.modules.active(Module::Team) {
            return Err(Failure(404, "module_disabled".into()));
        }
        if matches!(command, Some(Command::Discover)) {
            return self.discovery.find(state);
        }
        let mut store = self.snapshot()?;
        if matches!(command, Some(Command::Join)) {
            let base = body["base"].as_str().ok_or_else(Failure::invalid)?;
            return request(state, base, None, Operation::Join, Some(body));
        }
        if matches!(command, Some(Command::Login)) {
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
            store.recovery_required = false;
            self.save_store(state, store)?;
            self.events.notify_one();
            catalog::refresh_catalog(state).map_err(|_| Failure::unavailable())?;
            return self.local_status();
        }
        let session = store.session.as_mut().ok_or_else(Failure::signed_out)?;
        let base = session.base.as_str();
        let token = Some(session.token.as_str());
        match command.ok_or_else(Failure::invalid)? {
            Command::Status => {
                self.refresh_status(state, store)?;
                self.local_status()
            }
            Command::Logout | Command::Handover => {
                if matches!(command, Some(Command::Handover)) {
                    request(state, base, token, Operation::Handover, Some(body))?;
                }
                if let Err(error) = request(state, base, token, Operation::Logout, Some(&json!({})))
                    && error.0 < 500
                    && error.0 != 401
                {
                    return Err(error);
                }
                self.purge_local(state)?;
                self.local_status()
            }
            Command::Publish => self.publish(state, session, body),
            Command::Sync => self.sync(state, store),
            Command::Usage => {
                self.events.notify_one();
                if let Some(period) = body.get("period") {
                    request(
                        state,
                        base,
                        token,
                        Operation::UsageFiltered,
                        Some(&json!({"period":period})),
                    )
                } else {
                    request(state, base, token, Operation::Usage, None)
                }
            }
            Command::Forward {
                operation,
                body: forward_body,
            } => request(state, base, token, operation, forward_body.then_some(body)),
            Command::IssueInvite => {
                let endpoint = plan_endpoint(state, base, Operation::IssueInvite)?;
                let reply = send(state, &endpoint, token, Some(b"{}".to_vec()))?;
                if let Some(origin) = endpoint.announcement_origin {
                    let _ = self.discovery.announce(state, &origin, &reply);
                }
                Ok(reply)
            }
            Command::Discover | Command::Join | Command::Login => unreachable!(),
        }
    }
}
