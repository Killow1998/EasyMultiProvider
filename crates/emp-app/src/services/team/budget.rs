use super::{Failure, RequestDenied, TeamState, transport::request};
use crate::app::ServerState;
use emp_domain::{ResolvedRoute, RouteSource};
use emp_modules::{
    Module,
    team::{BudgetStatuses, Operation},
};

impl TeamState {
    pub(crate) fn check_route_budget(
        &self,
        state: &ServerState,
        route: &ResolvedRoute,
    ) -> Result<(), RequestDenied> {
        if !state.backend.modules.active(Module::Team) {
            return Ok(());
        }
        match route.source {
            RouteSource::ImplicitNative => self.check_native_budget(state),
            RouteSource::SubscriptionAccount => {
                let account = &route.provider.value()["account"];
                let source = self.subscription_source(state, &route.provider_id, account);
                self.check_subscription_budget(state, &source, || {
                    super::subscriptions::stored_identity(
                        &state.backend.configuration.vault,
                        account,
                    )
                })
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn check_native_budget(&self, state: &ServerState) -> Result<(), RequestDenied> {
        self.check_subscription_budget(state, "codex-native", || {
            let auth =
                crate::services::accounts::native_auth_document(&state.backend.codex.auth_path)?;
            emp_codex::account_auth_headers(&auth)?;
            Some(emp_state::accounts::account_auth_identity(&auth))
        })
    }

    pub(crate) fn check_subscription_budget(
        &self,
        state: &ServerState,
        provider: &str,
        identity: impl FnOnce() -> Option<String>,
    ) -> Result<(), RequestDenied> {
        let active = state.backend.modules.active(Module::Team);
        if self.shared_subscription(provider) {
            self.check_route(provider, active, None)?;
        } else {
            let granted = active
                && identity().is_some_and(|identity| {
                    self.store.lock().is_ok_and(|store| {
                        store
                            .as_ref()
                            .is_some_and(|store| store.native_usage_grant(&identity).is_some())
                    })
                });
            if !granted {
                return Ok(());
            }
        }
        self.check_budget(state)
    }

    fn check_budget(&self, state: &ServerState) -> Result<(), RequestDenied> {
        let unavailable = |Failure(status, _)| match status {
            401 | 403 => RequestDenied::ShareUnavailable,
            _ => RequestDenied::BudgetUnavailable,
        };
        let (session, scope) = self
            .store
            .lock()
            .ok()
            .and_then(|store| {
                let store = store.as_ref()?;
                let session = store.session.as_ref()?;
                Some((session.clone(), session.scope(&store.device)))
            })
            .ok_or(RequestDenied::BudgetUnavailable)?;
        while self
            .flush_batch(state, &session, &scope)
            .map_err(unavailable)?
        {}
        let reply = request(
            state,
            &session.base,
            Some(&session.token),
            Operation::BudgetStatus,
            None,
        )
        .map_err(unavailable)?;
        let budgets: BudgetStatuses =
            serde_json::from_value(reply).map_err(|_| RequestDenied::BudgetUnavailable)?;
        let active = budgets.weekly.as_ref().unwrap_or(&budgets.daily);
        let reached = active
            .limit
            .is_some_and(|limit| active.used.unwrap_or(0) >= limit);
        let kind = active.kind;
        self.observe_budget(state, &session, budgets);
        if reached {
            return Err(RequestDenied::BudgetReached(kind));
        }
        Ok(())
    }
}
