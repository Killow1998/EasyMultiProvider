//! Passive receipts for application commands. Only diagnostics are available to
//! this owner: it cannot access configuration, credentials, routing or workers.
//! A completed command is distinct from its observed effects and client delivery.
use emp_state::diagnostics::Diagnostics;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

pub(crate) struct OperationObservation {
    diagnostics: Arc<Diagnostics>,
    started: Instant,
    fields: Value,
}

pub(crate) fn observe<T, E>(
    diagnostics: &Arc<Diagnostics>,
    request_id: Option<&str>,
    operation: &'static str,
    execute: impl FnOnce(&mut OperationObservation) -> Result<T, E>,
) -> Result<T, E> {
    let mut receipt = OperationObservation {
        diagnostics: Arc::clone(diagnostics),
        started: Instant::now(),
        fields: json!({
            "operation_id":crate::util::random_hex(8).ok(), "request_id":request_id, "operation":operation,
            "outcome":"interrupted", "last_stage":"prepare", "stages":[],
            "checks":{}, "facts":{}, "client_effect":"unknown",
        }),
    };
    receipt.emit("operation_started");
    let result = execute(&mut receipt);
    receipt.fields["outcome"] = json!(if result.is_ok() {
        "completed"
    } else {
        "failed"
    });
    result
}

impl OperationObservation {
    fn emit(&self, name: &'static str) {
        let failed = (name == "operation_finished" && self.fields["outcome"] != "completed")
            || (name == "operation_stage_finished"
                && self.fields["stages"]
                    .as_array()
                    .and_then(|stages| stages.last())
                    .is_some_and(|stage| stage["outcome"] == "failed"));
        self.diagnostics
            .journal
            .event(if failed { "warning" } else { "info" }, name, &self.fields);
    }

    /// Preserve the exact result, including errors ignored by existing policy.
    pub(crate) fn step<T, E>(
        &mut self,
        stage: &'static str,
        execute: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        self.fields["last_stage"] = json!(stage);
        self.emit("operation_stage_started");
        let started = Instant::now();
        let result = execute();
        self.fields["stages"].as_array_mut().unwrap().push(json!({
            "stage":stage, "outcome":if result.is_ok() { "completed" } else { "failed" },
            "duration_ms":started.elapsed().as_millis() as u64,
        }));
        self.emit("operation_stage_finished");
        result
    }

    // Fixed application vocabulary only. Never accept a request body, path,
    // upstream error message, configuration snapshot or credential value.
    pub(crate) fn fact(&mut self, name: &'static str, value: &'static str) {
        self.fields["facts"][name] = json!(value);
    }

    /// None means unobserved, not a mismatch. Checks never determine execution.
    pub(crate) fn check(&mut self, name: &'static str, matches: Option<bool>) {
        self.fields["checks"][name] = json!(matches);
    }

    pub(crate) fn number(&mut self, name: &'static str, value: u64) {
        self.fields["facts"][name] = json!(value);
    }

    pub(crate) fn subject(&mut self, id: &str) {
        self.fields["subject"] = json!(self.diagnostics.journal.pseudonym(id));
    }
}

impl Drop for OperationObservation {
    fn drop(&mut self) {
        self.fields["duration_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.fields["handler_panicked"] = json!(std::thread::panicking());
        self.emit("operation_finished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_journal_and_mismatches_never_change_or_repeat_execution() {
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("not-a-directory");
        std::fs::write(&blocked, b"fixture").unwrap();
        let diagnostics = Arc::new(Diagnostics::new(&blocked));
        let mut executions = 0;
        let result: Result<usize, &str> = observe(&diagnostics, None, "fixture", |receipt| {
            receipt.check("expected_matches_actual", Some(false));
            receipt.step("execute_once", || {
                executions += 1;
                Err("private-failure")
            })
        });
        assert_eq!(result, Err("private-failure"));
        assert_eq!(executions, 1);
        let result = observe(&diagnostics, None, "fixture", |receipt| {
            receipt.check("expected_matches_actual", Some(false));
            receipt.step("execute_once", || {
                executions += 1;
                Ok::<_, ()>(42)
            })
        });
        assert_eq!(result, Ok(42));
        assert_eq!(executions, 2);
    }
}
