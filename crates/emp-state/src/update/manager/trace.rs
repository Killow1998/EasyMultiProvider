//! Stage timings share the application's bounded journal, including successful updates.
use super::{UpdateDiagnostic, UpdateManager, json};
use std::time::{Duration, Instant};

pub(super) struct UpdateTrace {
    operation: &'static str,
    started: Instant,
    stage: String,
    stage_started: Instant,
}

impl UpdateManager {
    pub(in crate::update) fn trace_download(
        &self,
        received: u64,
        expected: u64,
        duration: Duration,
        outcome: &str,
    ) {
        (self.0.observe)(
            "update_download",
            &json!({
                "received_bytes": received, "expected_bytes": expected,
                "duration_ms": duration.as_millis(), "outcome": outcome,
                "bytes_per_second": received as u128 * 1000 / duration.as_millis().max(1),
            }),
        );
    }

    pub(super) fn trace_start(&self, operation: &'static str) {
        let now = Instant::now();
        if let Ok(mut trace) = self.0.trace.lock() {
            *trace = Some(UpdateTrace {
                operation,
                started: now,
                stage: String::new(),
                stage_started: now,
            });
        }
        let snapshot = self.snapshot();
        (self.0.observe)(
            "update_started",
            &json!({
                "operation": operation, "current_version": snapshot.current_version,
                "target_version": snapshot.latest_version,
                "proxy_policy": "environment_then_system", "request_timeout_seconds": 600,
            }),
        );
    }

    pub(super) fn trace_stage(&self, stage: &str) {
        let Ok(mut trace) = self.0.trace.lock() else {
            return;
        };
        let Some(trace) = trace.as_mut() else { return };
        if trace.stage == stage {
            return;
        }
        if !trace.stage.is_empty() {
            (self.0.observe)(
                "update_stage_finished",
                &json!({
                    "operation": trace.operation, "stage": trace.stage,
                    "duration_ms": trace.stage_started.elapsed().as_millis(), "outcome": "completed",
                }),
            );
        }
        trace.stage = stage.into();
        trace.stage_started = Instant::now();
        (self.0.observe)(
            "update_stage_started",
            &json!({
                "operation": trace.operation, "stage": stage,
            }),
        );
    }

    pub(super) fn trace_retry(&self, diagnostic: &UpdateDiagnostic, count: u8) {
        if let Ok(mut trace) = self.0.trace.lock()
            && let Some(trace) = trace.as_mut()
            && !trace.stage.is_empty()
        {
            (self.0.observe)(
                "update_stage_finished",
                &json!({
                    "operation": trace.operation, "stage": trace.stage,
                    "duration_ms": trace.stage_started.elapsed().as_millis(), "outcome": "retrying",
                }),
            );
            trace.stage.clear();
        }
        (self.0.observe)(
            "update_retry",
            &json!({
                "stage": diagnostic.stage, "reason": diagnostic.reason,
                "retry_count": count, "retry_limit": super::retry::MAX_RETRIES,
                "http_status": diagnostic.http_status, "os_error": diagnostic.os_error,
            }),
        );
    }

    pub(super) fn trace_finish(&self, outcome: &str) {
        let trace = self.0.trace.lock().ok().and_then(|mut trace| trace.take());
        let Some(trace) = trace else { return };
        if !trace.stage.is_empty() {
            (self.0.observe)(
                "update_stage_finished",
                &json!({
                    "operation": trace.operation, "stage": trace.stage,
                    "duration_ms": trace.stage_started.elapsed().as_millis(), "outcome": outcome,
                }),
            );
        }
        let snapshot = self.snapshot();
        (self.0.observe)(
            "update_finished",
            &json!({
                "operation": trace.operation, "duration_ms": trace.started.elapsed().as_millis(),
                "outcome": outcome, "retry_count": snapshot.retry_count,
                "current_version": snapshot.current_version, "target_version": snapshot.latest_version,
            }),
        );
    }
}
