use super::management_events::{Change, ManagementEvents};
use emp_core::{ResolvedRoute, RouteSource};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
mod requests;

pub(crate) const ACTIVITY_RECENT_FOR_SECONDS: u64 = 60;
const ACTIVITY_RETENTION_SECONDS: u64 = 300;
const MAX_ACTIVITY_ROUTES: usize = 512;
const MAX_PUBLIC_ID_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ActivityIdentity {
    pub(crate) model_id: String,
    pub(crate) provider_id: Option<String>,
    pub(crate) account_id: Option<String>,
}

impl ActivityIdentity {
    /// Capture only identifiers that are safe to expose in the management UI.
    pub(crate) fn from_route(route: &ResolvedRoute) -> Option<Self> {
        let model_id = route.requested_model.clone();
        let (provider_id, account_id) = match route.source {
            RouteSource::ImplicitNative => (None, Some("@native".to_owned())),
            RouteSource::SubscriptionAccount => {
                let account_id = route
                    .provider
                    .value()
                    .get("account")
                    .and_then(Value::as_object)
                    .and_then(|account| account.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Some(route.provider_id.clone()));
                (None, account_id)
            }
            RouteSource::ExplicitModel | RouteSource::ForwardProvider => {
                let provider_id =
                    (!route.provider_id.is_empty()).then(|| route.provider_id.clone());
                (provider_id, None)
            }
        };
        Self::new(model_id, provider_id, account_id)
    }

    pub(crate) fn new(
        model_id: String,
        provider_id: Option<String>,
        account_id: Option<String>,
    ) -> Option<Self> {
        if !valid_public_id(&model_id)
            || provider_id
                .as_deref()
                .is_some_and(|id| !valid_public_id(id))
            || account_id.as_deref().is_some_and(|id| !valid_public_id(id))
        {
            return None;
        }
        Some(Self {
            model_id,
            provider_id,
            account_id,
        })
    }
}

fn valid_public_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_PUBLIC_ID_BYTES
}

#[derive(Clone, Copy, Debug)]
struct RouteActivity {
    in_flight: usize,
    last_finished: Option<u64>,
}

#[derive(Default)]
struct ActivityState {
    revision: u64,
    routes: BTreeMap<ActivityIdentity, RouteActivity>,
    requests: requests::Requests,
}

/// Request activity owns its state and publishes a change after each mutation.
#[derive(Default)]
pub(crate) struct ActivityService {
    state: Mutex<ActivityState>,
    events: Arc<ManagementEvents>,
}

pub(crate) struct ActivityGuard<'a> {
    service: &'a ActivityService,
    identity: Option<ActivityIdentity>,
}

impl ActivityService {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub(crate) fn is_idle(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .routes
            .values()
            .all(|activity| activity.in_flight == 0)
    }

    pub(crate) fn new(events: Arc<ManagementEvents>) -> Self {
        Self {
            state: Mutex::default(),
            events,
        }
    }

    /// Begin tracking one dispatched upstream request. The caller owns this
    /// guard through response completion, stream termination, or cancellation.
    pub(crate) fn begin<'a>(&'a self, identity: Option<ActivityIdentity>) -> ActivityGuard<'a> {
        let identity = identity.filter(|identity| {
            valid_public_id(&identity.model_id)
                && identity.provider_id.as_deref().is_none_or(valid_public_id)
                && identity.account_id.as_deref().is_none_or(valid_public_id)
        });
        let mut tracked_identity = None;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = unix_seconds();
        let mut changed = prune_expired(&mut state, now);

        if let Some(identity) = identity {
            if !state.routes.contains_key(&identity) && state.routes.len() >= MAX_ACTIVITY_ROUTES {
                let evictable = state
                    .routes
                    .iter()
                    .filter(|(_, activity)| activity.in_flight == 0)
                    .min_by_key(|(_, activity)| activity.last_finished.unwrap_or(0))
                    .map(|(identity, _)| identity.clone());
                if let Some(evictable) = evictable {
                    state.routes.remove(&evictable);
                    changed = true;
                }
            }

            if state.routes.contains_key(&identity) || state.routes.len() < MAX_ACTIVITY_ROUTES {
                let activity = state
                    .routes
                    .entry(identity.clone())
                    .or_insert(RouteActivity {
                        in_flight: 0,
                        last_finished: None,
                    });
                activity.in_flight = activity.in_flight.saturating_add(1);
                tracked_identity = Some(identity);
                changed = true;
            }
        }

        if changed {
            state.revision = state.revision.wrapping_add(1);
        }
        drop(state);
        if changed {
            self.events.publish(Change::Activity);
        }

        ActivityGuard {
            service: self,
            identity: tracked_identity,
        }
    }

    /// Public snapshot omits finished rows retained only for capacity management.
    pub(crate) fn snapshot(&self, observed_at: u64) -> Value {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let routes = state
            .routes
            .iter()
            .filter(|(_, activity)| {
                activity.in_flight > 0
                    || activity.last_finished.is_some_and(|finished| {
                        observed_at.saturating_sub(finished) <= ACTIVITY_RETENTION_SECONDS
                    })
            })
            .map(|(identity, activity)| {
                json!({
                    "model_id": identity.model_id,
                    "provider_id": identity.provider_id,
                    "account_id": identity.account_id,
                    "in_flight": activity.in_flight,
                    "last_finished": activity.last_finished,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "revision": state.revision,
            "observed_at": observed_at,
            "recent_for_seconds": ACTIVITY_RECENT_FOR_SECONDS,
            "routes": routes,
            "requests": state.requests.snapshot(observed_at),
        })
    }

    pub(crate) fn observe_request(
        &self,
        event: &Value,
        identity: Option<&ActivityIdentity>,
        finished: bool,
    ) {
        self.update_requests(|requests| {
            requests.observe(event, identity, finished, unix_seconds())
        });
    }

    fn update_requests(&self, update: impl FnOnce(&mut requests::Requests) -> bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = update(&mut state.requests);
        if changed {
            state.revision = state.revision.wrapping_add(1);
        }
        drop(state);
        if changed {
            self.events.publish(Change::Activity);
        }
    }

    fn finish(&self, identity: &ActivityIdentity) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(activity) = state.routes.get_mut(identity)
            && activity.in_flight > 0
        {
            activity.in_flight -= 1;
            activity.last_finished = Some(unix_seconds());
            state.revision = state.revision.wrapping_add(1);
            drop(state);
            self.events.publish(Change::Activity);
        }
    }
}

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        if let Some(identity) = &self.identity {
            self.service.finish(identity);
        }
    }
}

fn prune_expired(state: &mut ActivityState, now: u64) -> bool {
    let previous_len = state.routes.len();
    state.routes.retain(|_, activity| {
        activity.in_flight > 0
            || activity
                .last_finished
                .is_none_or(|finished| now.saturating_sub(finished) <= ACTIVITY_RETENTION_SECONDS)
    });
    state.routes.len() != previous_len
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
