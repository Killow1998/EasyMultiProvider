use super::{
    Imported, Result, Session, Store, TeamState, can_migrate, subscriptions, transport::request,
};
use crate::app::ServerState;
use emp_modules::{Module, team::Operation};
use emp_state::config::is_team_service_id;
use serde_json::{Value, json};
use std::borrow::Cow;

#[derive(Debug, Clone, Copy)]
pub(crate) enum RequestDenied {
    ShareUnavailable,
    Fast,
    Ultrafast,
    BudgetReached(emp_modules::team::BudgetKind),
    BudgetUnavailable,
}

impl RequestDenied {
    pub(crate) fn public_error(
        self,
        state: &ServerState,
    ) -> emp_router::native_http::NativeHttpError {
        use emp_modules::team::BudgetKind;
        let (status, code, chinese, english) = match self {
            Self::ShareUnavailable => (
                403,
                "workspace_permission_required",
                "此团队订阅目前未向你开放。",
                "This team subscription is not available to your account.",
            ),
            Self::Fast => (
                403,
                "workspace_fast_forbidden",
                "团队管理员尚未授予你 Fast（快速模式）权限。",
                "Your team administrator has not enabled Fast for you.",
            ),
            Self::Ultrafast => (
                403,
                "workspace_ultrafast_forbidden",
                "团队管理员尚未授予你 Ultrafast（极速模式）权限。",
                "Your team administrator has not enabled Ultrafast for you.",
            ),
            Self::BudgetReached(BudgetKind::DailyUsd) => (
                403,
                "workspace_budget_reached",
                "你已达到团队每日 USD 上限，团队调用已暂停。",
                "You have reached your daily team USD limit. Team requests are paused.",
            ),
            Self::BudgetReached(BudgetKind::WeeklyUsd) => (
                403,
                "workspace_budget_reached",
                "你已达到团队每周 USD 上限，团队调用已暂停。",
                "You have reached your weekly team USD limit. Team requests are paused.",
            ),
            Self::BudgetUnavailable => (
                503,
                "workspace_budget_unavailable",
                "暂时无法向团队 VPS 核对预算，团队调用已暂停。",
                "EMP could not confirm your budget with the team VPS. Team requests are paused.",
            ),
        };
        let english_ui = state
            .backend
            .configuration
            .read()
            .is_ok_and(|config| config["language"] == "en");
        let mut error = emp_router::native_http::NativeHttpError::router(
            status,
            if english_ui { english } else { chinese },
        );
        error.body["error"]["code"] = code.into();
        error
    }
}

impl Session {
    fn can_sync(&self, record: &Imported) -> bool {
        self.workspace() == record.workspace && self.identity["member"]["sync"] == true
    }
}

impl Store {
    pub(super) fn native_usage_grant(&self, identity: &str) -> Option<(&Session, &str)> {
        let session = self
            .session
            .as_ref()
            .filter(|session| session.identity["member"]["sync"] == true)?;
        let share = match session.identity["native_usage"].as_array() {
            Some(grants) => grants.iter().find_map(|grant| {
                let share = grant["share"]
                    .as_str()
                    .filter(|id| super::valid_identifier(id))?;
                (grant["account_identity"] == identity).then_some(share)
            }),
            None => self
                .imported
                .values()
                .find(|source| {
                    source.usable()
                        && session.workspace() == source.workspace
                        && source.account_identity.as_deref() == Some(identity)
                })
                .map(|source| source.share.as_str()),
        }?;
        Some((session, share))
    }
}

impl TeamState {
    pub(crate) fn replacement_subscription<'a>(
        &self,
        state: &ServerState,
        accounts: &'a [Value],
        account: &Value,
    ) -> Option<&'a Value> {
        if account["workspace_shared"] == true || account["_emp_shared_source"].is_string() {
            return None;
        }
        let id = account["id"].as_str()?;
        let (source, shared_expiry, private_auth) =
            self.matching_subscription(state, id, account)?;
        if self.check_route(&source, true, None).is_err() {
            return None;
        }
        if account["credential_status"] != "invalid"
            && subscriptions::subscription(&private_auth)
                .is_none_or(|private| Some(private.expires_at) >= shared_expiry)
        {
            return None;
        }
        accounts.iter().find(|candidate| {
            candidate["id"] == source
                && candidate["workspace_shared"] == true
                && candidate["enabled"] != false
                && candidate["credential_status"] != "invalid"
        })
    }

    pub(crate) fn project_subscription_credentials(&self, state: &ServerState, config: &mut Value) {
        let accounts = config["accounts"]
            .as_array_mut()
            .expect("normalized accounts");
        let replacements = accounts
            .iter()
            .enumerate()
            .filter_map(|(index, account)| {
                self.replacement_subscription(state, accounts, account)
                    .map(|source| (index, source.clone()))
            })
            .collect::<Vec<_>>();
        for (index, source) in replacements {
            let account = &mut accounts[index];
            account["_emp_catalog_auth_file"] = account["auth_file"].clone();
            account["auth_file"] = source["auth_file"].clone();
            account["credential_status"] = source["credential_status"].clone();
            if !source["quota"].is_null() {
                account["quota"] = source["quota"].clone();
            }
            account["_emp_shared_source"] = source["id"].clone();
        }
    }

    pub(crate) fn native_usage_scope(&self, identity: &str) -> Option<Value> {
        let store = self.store.lock().ok()?;
        let store = store.as_ref()?;
        let (session, share) = store.native_usage_grant(identity)?;
        Some(json!({"scope":session.scope(&store.device),"share":share}))
    }

    pub(crate) fn usage_scope(
        &self,
        state: &ServerState,
        route: &emp_core::ResolvedRoute,
    ) -> Option<Value> {
        if route.provider_id == "codex-native" || !state.backend.modules.active(Module::Team) {
            return None;
        }
        let account = route.provider.value().get("account")?;
        let provider = self.subscription_source(state, &route.provider_id, account);
        if !self.shared_subscription(&provider) {
            let identity =
                subscriptions::stored_identity(&state.backend.configuration.vault, account)?;
            return self.native_usage_scope(&identity);
        }
        let store = self.store.lock().ok()?;
        let store = store.as_ref()?;
        let record = store
            .imported
            .get(provider.as_ref())
            .filter(|source| source.usable())?;
        let session = store
            .session
            .as_ref()
            .filter(|session| session.can_sync(record))?;
        Some(json!({"scope":session.scope(&store.device),"share":record.share}))
    }

    pub(crate) fn subscription_source<'a>(
        &self,
        state: &ServerState,
        provider: &'a str,
        account: &Value,
    ) -> Cow<'a, str> {
        if let Some(source) = account["_emp_shared_source"].as_str() {
            return Cow::Owned(source.to_owned());
        }
        self.matching_subscription(state, provider, account)
            .map_or(Cow::Borrowed(provider), |(id, _, _)| Cow::Owned(id))
    }

    fn matching_subscription(
        &self,
        state: &ServerState,
        provider: &str,
        account: &Value,
    ) -> Option<(String, Option<i64>, Value)> {
        if account["workspace_shared"] == true
            || (account.is_null() && provider != "codex-native")
            || !state.backend.modules.active(Module::Team)
        {
            return None;
        }
        let store = self.store.lock().ok()?;
        let store = store.as_ref()?;
        let workspace = store.session.as_ref()?.workspace();
        let shares = store.imported.iter().filter(|(_, record)| {
            record.workspace == workspace && record.account_identity.is_some()
        });
        shares.clone().next()?;
        let auth = if provider == "codex-native" {
            crate::services::accounts::native_auth_document(
                &state.backend.accounts.native_auth_path,
            )?
        } else {
            subscriptions::stored_auth(&state.backend.configuration.vault, account)?
        };
        emp_codex::account_auth_headers(&auth)?;
        let identity = emp_state::accounts::account_auth_identity(&auth);
        let (id, record) = shares
            .filter(|(_, record)| record.account_identity.as_deref() == Some(identity.as_str()))
            .max_by_key(|(_, record)| (record.usable(), record.expires_at))?;
        Some((id.clone(), record.expires_at, auth))
    }

    pub(crate) fn check_model_route(
        &self,
        state: &ServerState,
        route: &emp_core::ResolvedRoute,
        tier: Option<&str>,
    ) -> std::result::Result<(), RequestDenied> {
        if route.provider.value().get("implicit_native") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        let active = state.backend.modules.active(Module::Team);
        let account = route
            .provider
            .value()
            .get("account")
            .unwrap_or(&Value::Null);
        let source = self.subscription_source(state, &route.provider_id, account);
        self.check_route(&source, active, tier)?;
        if active
            && tier.is_some()
            && !self.shared_subscription(&source)
            && let Some(identity) =
                subscriptions::stored_identity(&state.backend.configuration.vault, account)
            && let Ok(store) = self.store.lock()
            && let Some((session, _)) = store
                .as_ref()
                .and_then(|store| store.native_usage_grant(&identity))
        {
            check_tier(&session.identity["member"], tier)?;
        }
        Ok(())
    }

    pub(crate) fn check_route(
        &self,
        provider: &str,
        active: bool,
        tier: Option<&str>,
    ) -> std::result::Result<(), RequestDenied> {
        if !is_team_service_id(provider) {
            return Ok(());
        }
        let store = self
            .store
            .lock()
            .map_err(|_| RequestDenied::ShareUnavailable)?;
        let store = store.as_ref().ok_or(RequestDenied::ShareUnavailable)?;
        let Some(record) = store.imported.get(provider) else {
            return if self.configured_shares.contains(provider) {
                Err(RequestDenied::ShareUnavailable)
            } else {
                Ok(())
            };
        };
        if record.expires_at.is_none() {
            return Err(RequestDenied::ShareUnavailable);
        }
        let session = store
            .session
            .as_ref()
            .filter(|session| active && record.usable() && session.can_sync(record))
            .ok_or(RequestDenied::ShareUnavailable)?;
        check_tier(&session.identity["member"], tier)
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
            .filter(|id| self.check_route(id, active, None).is_err())
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

    pub(crate) fn exclude_shared_migration_providers(&self, config: &mut Value) -> Result<()> {
        let store = self.snapshot()?;
        let shared = |id: &str| {
            store
                .imported
                .get(id)
                .is_some_and(|record| record.expires_at.is_none())
        };
        config["providers"]
            .as_array_mut()
            .expect("normalized providers")
            .retain(|provider| !provider["id"].as_str().is_some_and(shared));
        config["models"]
            .as_array_mut()
            .expect("normalized models")
            .retain(|model| !model["provider"].as_str().is_some_and(shared));
        config["model_groups"]
            .as_array_mut()
            .expect("normalized model groups")
            .retain(|group| {
                !group["sources"]
                    .as_array()
                    .expect("normalized sources")
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|source| source.split_once('/'))
                    .any(|(provider, _)| shared(provider))
            });
        Ok(())
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
                        session.can_sync(record) && can_migrate(&session.identity)
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
                && can_migrate(&session.identity)
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
}

fn check_tier(member: &Value, tier: Option<&str>) -> std::result::Result<(), RequestDenied> {
    match tier {
        Some("ultrafast") if member["ultrafast"] != true => Err(RequestDenied::Ultrafast),
        Some("priority" | "fast") if member["fast"] != true => Err(RequestDenied::Fast),
        _ => Ok(()),
    }
}
