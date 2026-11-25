use crate::services::observation::diagnostics::Diagnostics;
use emp_modules::manifest::HookPhase;
use emp_modules::peer::{CallError, RequestCanceller};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use super::registry::{ActiveModule, ModuleRegistry};

const PHASE_BUDGET: Duration = Duration::from_secs(5);

pub(crate) struct HookOperation {
    modules: Vec<Arc<ActiveModule>>,
    id: Option<String>,
    caller: OnceLock<String>,
    deadline: Option<Instant>,
    diagnostics: Arc<Diagnostics>,
    cancellation: Mutex<Cancellation>,
}

#[derive(Default)]
struct Cancellation {
    cancelled: bool,
    pending: Option<RequestCanceller>,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Reply<T> {
    Claimed { value: T },
    Declined {},
}

enum Outcome<T> {
    Claimed(Option<T>),
    Declined,
    Failed {
        module_id: Arc<str>,
        reason: Failure,
    },
}

#[derive(Debug)]
enum Failure {
    Timeout,
    Exited,
    InvalidReply,
    InvalidFrame,
    Cancelled,
}

impl From<CallError> for Failure {
    fn from(error: CallError) -> Self {
        match error {
            CallError::TimedOut => Self::Timeout,
            CallError::Disconnected => Self::Exited,
            CallError::Remote(_) => Self::InvalidReply,
            CallError::InvalidFrame => Self::InvalidFrame,
            CallError::Cancelled => Self::Cancelled,
        }
    }
}

impl HookOperation {
    pub(crate) fn capture(
        registry: &ModuleRegistry,
        diagnostics: &Arc<Diagnostics>,
        id: Option<&str>,
        deadline: Option<Instant>,
    ) -> Option<Arc<Self>> {
        let modules = registry.hooks();
        (!modules.is_empty()).then(|| {
            Arc::new(Self {
                modules,
                id: id.map(str::to_owned),
                caller: OnceLock::new(),
                deadline,
                diagnostics: diagnostics.clone(),
                cancellation: Mutex::default(),
            })
        })
    }

    pub(crate) fn bind_caller(&self, caller: &str) {
        self.caller.get_or_init(|| caller.to_owned());
    }

    pub(crate) fn cancel(&self) {
        let mut cancellation = self.cancellation.lock().expect("hook cancellation");
        cancellation.cancelled = true;
        if let Some(pending) = cancellation.pending.take() {
            pending.cancel();
        }
    }

    pub(crate) fn dispatch<T: DeserializeOwned, R>(
        &self,
        phase: HookPhase,
        input: impl FnOnce() -> Value,
        mut claim: impl FnMut(&ActiveModule, T) -> Result<Option<R>, ()>,
    ) -> Result<Option<R>, ()> {
        let now = Instant::now();
        let deadline = self
            .deadline
            .unwrap_or(now + PHASE_BUDGET)
            .min(now + PHASE_BUDGET);
        let mut input = Some(input);
        let mut params = Value::Null;
        for module in self
            .modules
            .iter()
            .filter(|module| module.hooks.contains(&phase))
        {
            if let Some(input) = input.take() {
                params =
                    json!({"operation_id":self.id, "caller_id":self.caller.get(), "input":input()});
            }
            let result = if phase == HookPhase::CallFinished {
                module
                    .connection
                    .notify(phase.method(), params.clone())
                    .map(|()| None)
                    .map_err(Failure::from)
            } else {
                let pending = (|| {
                    let mut cancellation = self.cancellation.lock().expect("hook cancellation");
                    if cancellation.cancelled {
                        return Err(Failure::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(Failure::Timeout);
                    }
                    let pending = module
                        .connection
                        .start_request(phase.method(), params.clone())?;
                    cancellation.pending = Some(pending.canceller());
                    Ok(pending)
                })();
                pending.and_then(|pending| {
                    let result = pending.wait(deadline.saturating_duration_since(Instant::now()));
                    self.cancellation.lock().expect("hook cancellation").pending = None;
                    serde_json::from_value(result?)
                        .map(Some)
                        .map_err(|_| Failure::InvalidReply)
                })
            };
            let failed = |reason| Outcome::Failed {
                module_id: module.id.clone(),
                reason,
            };
            let outcome = match result {
                Ok(None) => Outcome::Claimed(None),
                Ok(Some(Reply::Declined {})) => Outcome::Declined,
                Ok(Some(Reply::Claimed { value })) => match claim(module, value) {
                    Ok(value) => Outcome::Claimed(value),
                    Err(()) => failed(Failure::InvalidReply),
                },
                Err(reason) => failed(reason),
            };
            match outcome {
                Outcome::Claimed(value @ Some(_)) => return Ok(value),
                Outcome::Claimed(None) | Outcome::Declined => {}
                Outcome::Failed { module_id, reason } => {
                    self.diagnostics.journal.event(
                        "warning",
                        "module_hook_failed",
                        &json!({
                            "operation_id":self.id, "module_id":module_id.as_ref(),
                            "phase":phase.method(), "reason":format!("{reason:?}")
                        }),
                    );
                    if matches!(phase, HookPhase::ResolveRoute | HookPhase::CheckAccess) {
                        return Err(());
                    }
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn notify_finished(&self, facts: Value) {
        let _ = self.dispatch::<Value, ()>(HookPhase::CallFinished, || facts, |_, _| Ok(None));
    }
}
