use super::{
    Failure, Result, Session, TeamState, plan_identity,
    transport::{plan_endpoint, request, send},
};
use crate::{app::ServerState, services::catalog};
use emp_modules::{Module, team::Operation, workspace::NATIVE_SHARE};
use serde_json::{Value, json};

pub(crate) enum Command {
    Discover,
    Join {
        base: String,
        body: Value,
    },
    Login {
        base: String,
        name: String,
        password: String,
    },
    InvalidCredentials,
    Status,
    Logout,
    Handover(Value),
    Publish {
        native_source: bool,
        account_id: Result<Option<String>>,
    },
    PublishSource,
    Sync,
    Usage(Option<Value>),
    IssueInvite,
    Forward(Operation, Value),
    Query(Operation),
    Unknown,
}

impl Command {
    pub(crate) fn parse(name: &str, mut body: Value) -> Self {
        match name {
            "discover" => Self::Discover,
            "join" => match body["base"].as_str() {
                Some(base) => Self::Join {
                    base: base.to_owned(),
                    body,
                },
                None => Self::InvalidCredentials,
            },
            "login" => match (
                body["base"].as_str(),
                body["name"].as_str(),
                body["password"].as_str(),
            ) {
                (Some(base), Some(name), Some(password)) => Self::Login {
                    base: base.to_owned(),
                    name: name.to_owned(),
                    password: password.to_owned(),
                },
                _ => Self::InvalidCredentials,
            },
            "status" => Self::Status,
            "logout" => Self::Logout,
            "handover" => Self::Handover(body),
            "publish" => Self::Publish {
                native_source: body["provider"].is_null() || body["provider"] == NATIVE_SHARE,
                account_id: body
                    .get("account_id")
                    .map(|id| id.as_str().map(str::to_owned).ok_or_else(Failure::invalid))
                    .transpose(),
            },
            "publish_source" => Self::PublishSource,
            "sync" => Self::Sync,
            "usage" => Self::Usage(body.as_object_mut().expect("JSON object").remove("period")),
            "issue_invite" => Self::IssueInvite,
            "members" => Self::Forward(Operation::Members, body),
            "member_access" => Self::Forward(Operation::MemberAccess, body),
            "reject_member" => Self::Forward(Operation::RejectMember, body),
            "remove_member" => Self::Forward(Operation::RemoveMember, body),
            "budget" => Self::Forward(Operation::Budget, body),
            "weekly_budget" => Self::Forward(Operation::WeeklyBudget, body),
            "review_service" => Self::Forward(Operation::ReviewService, body),
            "remove_service" => Self::Forward(Operation::RemoveService, body),
            "member_tiers" => Self::Forward(Operation::MemberTiers, body),
            "profile" => Self::Forward(Operation::Profile, body),
            "profile_visibility" => Self::Forward(Operation::ProfileVisibility, body),
            "subscription_reminder" => Self::Forward(Operation::SubscriptionReminder, body),
            "audit" => Self::Forward(Operation::Audit, body),
            "profiles" => Self::Query(Operation::Profiles),
            "overview" => Self::Query(Operation::Overview),
            "backup" => Self::Query(Operation::Backup),
            "subscriptions" => Self::Query(Operation::Subscriptions),
            "list_members" => Self::Query(Operation::ListMembers),
            "shared_services" => Self::Query(Operation::SharedServices),
            "join_code" => Self::Query(Operation::JoinCode),
            _ => Self::Unknown,
        }
    }
}

impl TeamState {
    pub(crate) fn command(&self, state: &ServerState, command: Command) -> Result<Value> {
        let _operation = self.operations.lock().map_err(|_| Failure::unavailable())?;
        if !state.backend.modules.active(Module::Team) {
            return Err(Failure(404, "module_disabled".into()));
        }
        if matches!(command, Command::Discover) {
            return self.discovery.find(state);
        }
        let mut store = self.snapshot()?;
        let command = match command {
            Command::InvalidCredentials => return Err(Failure::invalid()),
            Command::Join { base, body } => {
                return request(state, &base, None, Operation::Join, Some(&body));
            }
            Command::Login {
                base,
                name,
                password,
            } => {
                if store.device.is_empty() {
                    store.device =
                        crate::util::random_hex(16).map_err(|_| Failure::unavailable())?;
                }
                let mut identity = request(
                    state,
                    &base,
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
                    base,
                    token,
                    identity,
                });
                store.recovery_required = false;
                self.save_store(state, store)?;
                self.events.notify_one();
                catalog::refresh_catalog(state).map_err(|_| Failure::unavailable())?;
                return self.local_status();
            }
            command => command,
        };
        let session = store.session.as_mut().ok_or_else(Failure::signed_out)?;
        let base = session.base.as_str();
        let token = Some(session.token.as_str());
        match command {
            Command::Status => {
                self.refresh_status(state, store)?;
                self.local_status()
            }
            Command::Logout | Command::Handover(_) => {
                if let Command::Handover(body) = command {
                    request(state, base, token, Operation::Handover, Some(&body))?;
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
            Command::Publish {
                native_source,
                account_id,
            } => self.publish(state, session, native_source, account_id),
            Command::PublishSource => {
                if session.identity["member"]["publish"] != true {
                    return Err(Failure(403, "workspace_permission_required".into()));
                }
                super::subscriptions::upload_details(state)
            }
            Command::Sync => self.sync(state, store),
            Command::Usage(period) => {
                self.events.notify_one();
                if let Some(period) = period {
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
            Command::Forward(operation, body) => {
                request(state, base, token, operation, Some(&body))
            }
            Command::Query(operation) => request(state, base, token, operation, None),
            Command::Unknown => Err(Failure::invalid()),
            Command::IssueInvite => {
                let endpoint = plan_endpoint(state, base, Operation::IssueInvite)?;
                let reply = send(state, &endpoint, token, Some(b"{}".to_vec()))?;
                if let Some(origin) = endpoint.announcement_origin {
                    let _ = self.discovery.announce(state, &origin, &reply);
                }
                Ok(reply)
            }
            Command::Discover
            | Command::Join { .. }
            | Command::Login { .. }
            | Command::InvalidCredentials => unreachable!(),
        }
    }
}
