//! Bounded, process-local request activity for management views.

use emp_core::{ResolvedRoute, RouteSource};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

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
}

/// Activity is deliberately separate from quota revisions: SSE subscribers
/// share the wake condition, but an activity change never changes quota data.
#[derive(Default)]
pub(crate) struct ActivityService {
    state: Mutex<ActivityState>,
}

pub(crate) struct ActivityGuard<'a> {
    service: &'a ActivityService,
    identity: Option<ActivityIdentity>,
    wake_revision: &'a Mutex<u64>,
    wake_condition: &'a Condvar,
}

impl ActivityService {
    /// Begin tracking one dispatched upstream request. The caller owns this
    /// guard through response completion, stream termination, or cancellation.
    pub(crate) fn begin<'a>(
        &'a self,
        identity: Option<ActivityIdentity>,
        wake_revision: &'a Mutex<u64>,
        wake_condition: &'a Condvar,
    ) -> ActivityGuard<'a> {
        let identity = identity.filter(|identity| {
            valid_public_id(&identity.model_id)
                && identity.provider_id.as_deref().is_none_or(valid_public_id)
                && identity.account_id.as_deref().is_none_or(valid_public_id)
        });
        let mut tracked_identity = None;
        let wake_guard = wake_revision.lock().unwrap_or_else(PoisonError::into_inner);
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
        drop(wake_guard);
        if changed {
            wake_condition.notify_all();
        }

        ActivityGuard {
            service: self,
            identity: tracked_identity,
            wake_revision,
            wake_condition,
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .revision
    }

    /// Return a compact public snapshot. Old finished rows are retained only
    /// for bounded capacity management, then omitted from the snapshot.
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
        })
    }

    fn finish(
        &self,
        identity: &ActivityIdentity,
        wake_revision: &Mutex<u64>,
        wake_condition: &Condvar,
    ) {
        let wake_guard = wake_revision.lock().unwrap_or_else(PoisonError::into_inner);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(activity) = state.routes.get_mut(identity)
            && activity.in_flight > 0
        {
            activity.in_flight -= 1;
            activity.last_finished = Some(unix_seconds());
            state.revision = state.revision.wrapping_add(1);
            drop(state);
            drop(wake_guard);
            wake_condition.notify_all();
        }
    }
}

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        if let Some(identity) = &self.identity {
            self.service
                .finish(identity, self.wake_revision, self.wake_condition);
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
mod tests {
    use super::*;
    use emp_core::{RouteSource, resolved_route_from_parts};

    fn identity(model_id: &str, account_id: Option<&str>) -> ActivityIdentity {
        ActivityIdentity::new(
            model_id.to_owned(),
            Some("provider-a".to_owned()),
            account_id.map(str::to_owned),
        )
        .expect("valid public identity")
    }

    #[test]
    fn guard_counts_concurrent_requests_and_records_finish() {
        let service = ActivityService::default();
        let wake_revision = Mutex::new(0);
        let wake_condition = Condvar::new();
        let identity = identity("model-a", Some("account-a"));

        let first = service.begin(Some(identity.clone()), &wake_revision, &wake_condition);
        let second = service.begin(Some(identity.clone()), &wake_revision, &wake_condition);
        let active = service.snapshot(unix_seconds());
        assert_eq!(active["routes"][0]["in_flight"], 2);
        assert_eq!(active["routes"][0]["account_id"], "account-a");

        drop(first);
        assert_eq!(
            service.snapshot(unix_seconds())["routes"][0]["in_flight"],
            1
        );
        drop(second);
        let finished = service.snapshot(unix_seconds());
        assert_eq!(finished["routes"][0]["in_flight"], 0);
        assert!(finished["routes"][0]["last_finished"].as_u64().is_some());
    }

    #[test]
    fn active_snapshot_keeps_only_public_fields() {
        let service = ActivityService::default();
        let wake_revision = Mutex::new(0);
        let wake_condition = Condvar::new();
        let _guard = service.begin(
            Some(identity("visible-model", Some("@native"))),
            &wake_revision,
            &wake_condition,
        );
        let snapshot = service.snapshot(unix_seconds());
        assert_eq!(snapshot["recent_for_seconds"], ACTIVITY_RECENT_FOR_SECONDS);
        assert_eq!(snapshot["routes"][0]["model_id"], "visible-model");
        assert_eq!(snapshot["routes"][0]["account_id"], "@native");
        assert!(snapshot["routes"][0].get("endpoint").is_none());
        assert!(snapshot["routes"][0].get("prompt").is_none());
    }

    #[test]
    fn snapshot_omits_finished_routes_past_retention() {
        let service = ActivityService::default();
        let stale = identity("stale-model", Some("account-a"));
        service.state.lock().unwrap().routes.insert(
            stale,
            RouteActivity {
                in_flight: 0,
                last_finished: Some(10),
            },
        );

        let snapshot = service.snapshot(10 + ACTIVITY_RETENTION_SECONDS + 1);
        assert_eq!(snapshot["routes"], json!([]));
    }

    #[test]
    fn route_storage_stays_bounded_when_every_tracked_route_is_active() {
        let service = ActivityService::default();
        let wake_revision = Mutex::new(0);
        let wake_condition = Condvar::new();
        let guards = (0..=MAX_ACTIVITY_ROUTES)
            .map(|index| {
                service.begin(
                    Some(identity(&format!("model-{index}"), None)),
                    &wake_revision,
                    &wake_condition,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            service.snapshot(unix_seconds())["routes"]
                .as_array()
                .unwrap()
                .len(),
            MAX_ACTIVITY_ROUTES
        );
        drop(guards);
    }

    #[test]
    fn invalid_or_oversized_public_ids_are_not_tracked() {
        assert!(ActivityIdentity::new(String::new(), None, None).is_none());
        assert!(ActivityIdentity::new("x".repeat(MAX_PUBLIC_ID_BYTES + 1), None, None).is_none());

        let service = ActivityService::default();
        let wake_revision = Mutex::new(0);
        let wake_condition = Condvar::new();
        let guard = service.begin(None, &wake_revision, &wake_condition);
        assert_eq!(
            service.snapshot(unix_seconds())["routes"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        drop(guard);
        assert_eq!(service.revision(), 0);
    }

    #[test]
    fn resolved_routes_keep_provider_and_subscription_identity_separate() {
        let account_route = resolved_route_from_parts(
            "team/model",
            json!({
                "id":"team", "name":"Team", "base_url":"https://codex.invalid/v1",
                "protocol":"responses", "auth_mode":"account", "account":{"id":"team"}
            })
            .as_object()
            .unwrap()
            .clone(),
            json!({"id":"team/model","provider":"team"})
                .as_object()
                .unwrap()
                .clone(),
            RouteSource::SubscriptionAccount,
        )
        .expect("subscription route");
        let native_route = resolved_route_from_parts(
            "gpt-demo",
            json!({
                "id":"codex-native", "name":"Native", "base_url":"https://codex.invalid/v1",
                "protocol":"responses", "auth_mode":"forward", "implicit_native":true
            })
            .as_object()
            .unwrap()
            .clone(),
            json!({"id":"gpt-demo"}).as_object().unwrap().clone(),
            RouteSource::ImplicitNative,
        )
        .expect("native route");
        let provider_route = resolved_route_from_parts(
            "demo/model",
            json!({
                "id":"demo", "name":"Demo", "base_url":"https://provider.invalid/v1",
                "protocol":"responses", "auth_mode":"api_key"
            })
            .as_object()
            .unwrap()
            .clone(),
            json!({"id":"demo/model","provider":"demo"})
                .as_object()
                .unwrap()
                .clone(),
            RouteSource::ExplicitModel,
        )
        .expect("provider route");

        let account_identity = ActivityIdentity::from_route(&account_route).unwrap();
        assert_eq!(account_identity.model_id, "team/model");
        assert_eq!(account_identity.provider_id, None);
        assert_eq!(account_identity.account_id.as_deref(), Some("team"));

        let native_identity = ActivityIdentity::from_route(&native_route).unwrap();
        assert_eq!(native_identity.model_id, "gpt-demo");
        assert_eq!(native_identity.provider_id, None);
        assert_eq!(native_identity.account_id.as_deref(), Some("@native"));

        let provider_identity = ActivityIdentity::from_route(&provider_route).unwrap();
        assert_eq!(provider_identity.model_id, "demo/model");
        assert_eq!(provider_identity.provider_id.as_deref(), Some("demo"));
        assert_eq!(provider_identity.account_id, None);
    }
}
