use super::RequestOutcome;
use serde_json::json;
impl RequestOutcome<'_> {
    pub(super) fn persist_call(&self) {
        let now = crate::util::system_now();
        let started = now - self.started.elapsed().as_secs_f64();
        let finished = started + self.event["duration_ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
        self.state.backend.diagnostics.record(&self.event);
        match self
            .state
            .backend
            .usage
            .ledger
            .record_completion(&self.event, now, started, finished)
        {
            Ok(()) => {
                self.state
                    .backend
                    .management_events
                    .publish(crate::services::management_events::Change::Usage);
                if self.event["team_usage"].is_object() {
                    self.state.backend.team.event_ready();
                }
            }
            Err(_) => self.state.backend.diagnostics.journal.event(
                "error",
                "call_record_save_failed",
                &json!({"request_id":self.event["request_id"]}),
            ),
        }
    }
}
