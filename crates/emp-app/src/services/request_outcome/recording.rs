use super::{RequestActivity, RequestOutcome};
use crate::services::{
    activity::ActivityService,
    auto_review::ReviewState,
    management_events::ManagementEvents,
    memory_routing::{self, MemoryStatus},
    observation::diagnostics::Diagnostics,
    team::TeamState,
    usage::UsageState,
};
use serde_json::json;

pub(super) struct CompletionServices<'a> {
    pub(super) activity: &'a ActivityService,
    pub(super) aggregate: &'a ReviewState,
    pub(super) auto_review: &'a ReviewState,
    pub(super) diagnostics: &'a Diagnostics,
    pub(super) events: &'a ManagementEvents,
    pub(super) memory: &'a MemoryStatus,
    pub(super) team: &'a TeamState,
    pub(super) usage: &'a UsageState,
}

pub(super) fn finish_request(
    services: &CompletionServices<'_>,
    outcome: RequestOutcome,
    route_activity: Option<RequestActivity<'_>>,
) {
    let event = &outcome.event;
    if event["route"] == "prewarm" {
        services.diagnostics.journal.event(
            "info",
            "model_prewarm_finished",
            &json!({"request_id":event["request_id"],"model_id":event["model_id"],
                    "duration_ms":event["duration_ms"],"success":event["success"],
                    "error_class":event["error_class"],"generate":false}),
        );
        if let Some(cancellation) = outcome.prewarm_cancellation {
            services.activity.finish_request(
                &cancellation.event(&event["request_id"]),
                outcome.identity.as_ref(),
            );
        }
    } else {
        record_route_feedback(services, &outcome);
        record_billing(services, &outcome);
        services
            .activity
            .finish_request(event, outcome.identity.as_ref());
        notify_projected_call_finished();
    }
    drop(route_activity);
}

fn record_billing(services: &CompletionServices<'_>, outcome: &RequestOutcome) {
    let event = &outcome.event;
    let now = crate::util::system_now();
    let started = now - outcome.started.elapsed().as_secs_f64();
    let finished = started + event["duration_ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
    services.diagnostics.record(event);
    match services
        .usage
        .record_completion(event, now, started, finished)
    {
        Ok(()) => {
            if event["team_usage"].is_object() {
                services.team.event_ready();
            }
        }
        Err(_) => services.diagnostics.journal.event(
            "error",
            "call_record_save_failed",
            &json!({"request_id":event["request_id"]}),
        ),
    }
}

fn record_route_feedback(services: &CompletionServices<'_>, outcome: &RequestOutcome) {
    let event = &outcome.event;
    let success = event["success"] == true;
    let reason = event["failure_reason"].as_str().unwrap_or_default();
    let class = event["error_class"].as_str().unwrap_or_default();
    if let Some(owner) = &outcome.aggregate_owner {
        services
            .aggregate
            .record_source_result(owner, success, reason, class);
    }
    if matches!(emp_state::usage::call_state(event), "completed" | "failed") {
        services.auto_review.record_result(
            event["client_model"].as_str().unwrap_or_default(),
            event["provider_id"].as_str().unwrap_or_default(),
            success,
            reason,
            class,
        );
    }
    memory_routing::observe(services.memory, services.events, event);
}

fn notify_projected_call_finished() {}
