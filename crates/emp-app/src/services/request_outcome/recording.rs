//! Persistent call receipts share the existing usage database and management feed.
use super::*;
impl RequestOutcome<'_> {
    pub(super) fn prepare_call_facts(&mut self) {
        self.update_model_facts();
        if let Some(identity) = &self.identity {
            self.event["account_id"] = json!(identity.account_id);
        }
        let now = crate::util::system_now();
        let snapshot = self.state.backend.activity.snapshot(now as u64);
        if let Some(receipt) = snapshot["requests"].as_array().and_then(|records| {
            records
                .iter()
                .find(|record| record["request_id"] == self.event["request_id"])
        }) {
            for field in ["attempts", "retries", "attempt_routes"] {
                self.event[field] = receipt[field].clone();
            }
        }
    }
    pub(super) fn persist_call(&self) {
        let now = crate::util::system_now();
        let started = now - self.started.elapsed().as_secs_f64();
        let finished = started + self.event["duration_ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
        match self
            .state
            .backend
            .usage
            .ledger
            .record_call(&self.event, started, finished)
        {
            Ok(()) => self
                .state
                .backend
                .management_events
                .publish(crate::services::management_events::Change::Usage),
            Err(_) => self.state.backend.diagnostics.journal.event(
                "error",
                "call_record_save_failed",
                &json!({"request_id":self.event["request_id"]}),
            ),
        }
    }
}
