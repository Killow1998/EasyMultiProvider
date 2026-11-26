use super::*;
use emp_domain::{RouteSource, resolved_route_from_parts};
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct PublicationLog(Mutex<Option<Arc<Publications>>>);

impl PublicationLog {
    pub(super) fn record(&self, state: &ActivityState, now: u64) {
        if let Some(log) = self.0.lock().unwrap().as_ref() {
            log.history
                .lock()
                .unwrap()
                .snapshots
                .push(state.snapshot(now));
            log.changed.notify_one();
        }
    }
}

#[derive(Default)]
pub(crate) struct Publications {
    history: Mutex<History>,
    changed: Notify,
}

#[derive(Default)]
struct History {
    snapshots: Vec<Value>,
    delivered: Option<usize>,
}

impl ActivityService {
    pub(crate) fn record_publications(&self) -> Arc<Publications> {
        let log = Arc::new(Publications::default());
        *self.publications.0.lock().unwrap() = Some(log.clone());
        log
    }
}

fn same_route(left: &Value, right: &Value) -> bool {
    ["model_id", "provider_id", "account_id"]
        .into_iter()
        .all(|key| left[key] == right[key])
}

impl Publications {
    pub(crate) fn latest(&self) -> Value {
        self.history
            .lock()
            .unwrap()
            .snapshots
            .last()
            .cloned()
            .unwrap_or_default()
    }

    #[cfg(unix)]
    pub(crate) async fn wait_for_idle_route(&self) {
        loop {
            let route = &self.latest()["routes"][0];
            if route["in_flight"] == 0 && route["last_finished"].is_number() {
                return;
            }
            self.changed.notified().await;
        }
    }

    #[cfg(unix)]
    pub(crate) fn delivered(&self) {
        let mut history = self.history.lock().unwrap();
        history.delivered = Some(history.snapshots.len());
    }

    pub(crate) fn assert_intervals(&self, request_ids: &[&str], check_delivery: bool) {
        let history = self.history.lock().unwrap();
        let mut last_revision = history.snapshots[0]["revision"].as_u64().unwrap() - 1;
        let mut went_idle = false;
        for (index, snapshot) in history.snapshots.iter().enumerate() {
            last_revision += 1;
            assert_eq!(
                snapshot["revision"], last_revision,
                "missing activity publication"
            );
            let routes = snapshot["routes"].as_array().unwrap();
            let requests = snapshot["requests"].as_array().unwrap();
            for request in requests
                .iter()
                .filter(|request| request["state"] == "active")
            {
                assert!(
                    routes.iter().any(|route| same_route(route, request)
                        && route["in_flight"].as_u64().unwrap() >= 1),
                    "active request without a route count: {snapshot}"
                );
            }
            for route in routes {
                if route["in_flight"] != 0 || !route["last_finished"].is_number() {
                    continue;
                }
                went_idle = true;
                for id in request_ids {
                    assert!(
                        requests.iter().any(|request| same_route(route, request)
                            && request["request_id"] == *id
                            && request["finished_at"].is_number()),
                        "{id} missing a terminal receipt when the route became idle: {snapshot}"
                    );
                }
                if check_delivery {
                    assert!(
                        history
                            .delivered
                            .is_some_and(|delivered| delivered <= index),
                        "route became idle before downstream delivery: {snapshot}"
                    );
                }
            }
        }
        assert!(went_idle, "no route release was recorded");
    }

    pub(crate) fn assert_overlap(&self, first: &str, second: &str) {
        let history = self.history.lock().unwrap();
        let second_started = history
            .snapshots
            .iter()
            .find(|snapshot| {
                snapshot["requests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|request| request["request_id"] == second && request["state"] == "active")
            })
            .expect("second request became active");
        assert_eq!(
            second_started["routes"][0]["in_flight"], 2,
            "{second_started}"
        );
        let transition = history
            .snapshots
            .windows(2)
            .find(|pair| {
                pair[0]["routes"][0]["in_flight"] == 2 && pair[1]["routes"][0]["in_flight"] == 1
            })
            .expect("overlapping requests must release one route count at a time");
        for snapshot in transition {
            let requests = snapshot["requests"].as_array().unwrap();
            assert!(
                requests.iter().any(
                    |request| request["request_id"] == first && request["state"] == "completed"
                ),
                "{snapshot}"
            );
            assert!(
                requests
                    .iter()
                    .any(|request| request["request_id"] == second && request["state"] == "active"),
                "{snapshot}"
            );
        }
    }
}

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
    let identity = identity("model-a", Some("account-a"));

    let first = service.begin(Some(identity.clone()));
    let second = service.begin(Some(identity.clone()));
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
    let _guard = service.begin(Some(identity("visible-model", Some("@native"))));
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
    let guards = (0..=MAX_ACTIVITY_ROUTES)
        .map(|index| service.begin(Some(identity(&format!("model-{index}"), None))))
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
    let guard = service.begin(None);
    assert_eq!(
        service.snapshot(unix_seconds())["routes"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    drop(guard);
    assert_eq!(service.snapshot(unix_seconds())["revision"], 0);
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
