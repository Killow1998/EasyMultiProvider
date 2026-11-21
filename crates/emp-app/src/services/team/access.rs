use super::{Imported, Result, Session, TeamState, can_migrate, subscriptions, transport::request};
use crate::app::ServerState;
use emp_modules::{Module, team::Operation};
use emp_state::config::is_team_service_id;
use serde_json::{Value, json};
use std::borrow::Cow;

impl Session {
    fn can_sync(&self, record: &Imported) -> bool {
        self.workspace() == record.workspace && self.identity["member"]["sync"] == true
    }
}

impl TeamState {
    pub(crate) fn scope(&self, provider: &str) -> Option<Value> {
        let store = self.store.lock().ok()?;
        let store = store.as_ref()?;
        let source = store
            .imported
            .get(provider)
            .filter(|source| source.usable())?;
        let session = store
            .session
            .as_ref()
            .filter(|session| session.can_sync(source))?;
        Some(json!({
            "scope":session.scope(&store.device),
            "share":source.share,
        }))
    }

    pub(crate) fn subscription_source<'a>(
        &self,
        state: &ServerState,
        provider: &'a str,
        account: &Value,
    ) -> Cow<'a, str> {
        let personal = Cow::Borrowed(provider);
        if account["workspace_shared"] == true
            || (account.is_null() && provider != "codex-native")
            || !state.backend.modules.active(Module::Team)
        {
            return personal;
        }
        let Ok(store) = self.store.lock() else {
            return personal;
        };
        let Some(store) = store.as_ref() else {
            return personal;
        };
        let Some(session) = &store.session else {
            return personal;
        };
        let shares = store.imported.iter().filter(|(_, record)| {
            record.workspace == session.workspace() && record.account_identity.is_some()
        });
        if shares.clone().next().is_none() {
            return personal;
        }
        let identity = if provider == "codex-native" {
            crate::services::accounts::native_auth_document(
                &state.backend.accounts.native_auth_path,
            )
            .filter(|auth| emp_codex::account_auth_headers(auth).is_some())
            .map(|auth| emp_state::accounts::account_auth_identity(&auth))
        } else {
            subscriptions::stored_identity(&state.backend.configuration.vault, account)
        };
        let Some(identity) = identity else {
            return personal;
        };
        shares
            .filter(|(_, record)| record.account_identity.as_deref() == Some(identity.as_str()))
            .max_by_key(|(_, record)| record.usable())
            .map_or(personal, |(id, _)| Cow::Owned(id.clone()))
    }

    pub(crate) fn check_model_route(
        &self,
        state: &ServerState,
        route: &emp_core::ResolvedRoute,
        tier: Option<&str>,
    ) -> std::result::Result<(), &'static str> {
        if route.provider.value().get("implicit_native") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        let source = self.subscription_source(
            state,
            &route.provider_id,
            route
                .provider
                .value()
                .get("account")
                .unwrap_or(&Value::Null),
        );
        self.check_route(&source, state.backend.modules.active(Module::Team), tier)
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
            .filter(|session| active && record.usable() && session.can_sync(record))
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
