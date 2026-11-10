//! A temporary real TUI per user turn. Codex owns history and tool execution.
mod attachments;
mod protocol;
mod session;
mod terminal;
mod transcript;

use super::{Cancellation, ClaudeCliCompletion, ClaudeCliError, cancellation_reason, projection};
use crate::{app::ServerState, services::disconnect::DisconnectMonitor};
use emp_core::{Protocol, ResolvedRoute};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::Duration,
};

type SharedTurn = Arc<Mutex<Turn>>;
#[derive(Default)]
pub(crate) struct TurnPool(Mutex<BTreeMap<(String, String), SharedTurn>>);

enum Reply {
    Dispatched,
    Text(String, String),
    Complete(Value, Result<(), ClaudeCliError>),
}

struct ToolResults {
    input: Vec<Value>,
    tools: Value,
}

struct Turn {
    sender: mpsc::Sender<ToolResults>,
    receiver: mpsc::Receiver<Reply>,
    cancelled: Arc<Cancellation>,
    worker: Option<JoinHandle<()>>,
    pending: Vec<String>,
    turn_id: Option<String>,
    input_len: usize,
    history: u64,
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.cancelled.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    executable: &emp_codex::installed_cli::InstalledClaudeCli,
    mut monitor: Option<&mut DisconnectMonitor>,
    usage: &mut crate::services::request_outcome::RequestOutcome<'_>,
    text: &mut dyn FnMut(&str, &str) -> std::io::Result<()>,
) -> Result<ClaudeCliCompletion, ClaudeCliError> {
    let candidate = route
        .with_protocol(Protocol::AnthropicMessages)
        .map_err(|_| ClaudeCliError::Failure("unsupported_provider_protocol"))?;
    usage.candidate(&candidate);
    let identity = identity(body, incoming);
    let stateless = identity.is_none();
    let (key, turn_id) = identity.unwrap_or_else(|| {
        (
            (
                String::new(),
                crate::util::random_hex(16).expect("request identity"),
            ),
            None,
        )
    });
    let items = body["input"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let after_user = items
        .iter()
        .rposition(|item| item["role"] == "user")
        .map_or(0, |index| index + 1);
    let tool_outputs = items[after_user..]
        .iter()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output" | "tool_search_output")
            )
        })
        .collect::<Vec<_>>();
    let prior = state
        .backend
        .claude_turns
        .0
        .lock()
        .map_err(|_| ClaudeCliError::Failure("claude_cli_capacity_unavailable"))?
        .get(&key)
        .cloned();
    let mut reuse: Option<SharedTurn> = None;
    if let Some(prior) = prior {
        let turn = prior
            .lock()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_process_failed"))?;
        if !turn.cancelled.is_cancelled()
            && !turn.pending.is_empty()
            && turn.input_len <= items.len()
            && turn.history == history(body, &items[..turn.input_len], route)
            && (turn_id.is_none() || turn.turn_id.is_none() || turn_id == turn.turn_id)
            && turn
                .pending
                .iter()
                .all(|id| tool_outputs.iter().any(|item| item["call_id"] == *id))
        {
            reuse = Some(Arc::clone(&prior));
        }
    }
    let turn = match reuse {
        Some(turn) => turn,
        None => {
            let old = state
                .backend
                .claude_turns
                .0
                .lock()
                .map_err(|_| ClaudeCliError::Failure("claude_cli_capacity_unavailable"))?
                .remove(&key);
            drop(old);
            let cancelled = Arc::new(Cancellation::new());
            let (sender, input) = mpsc::channel();
            let (result, receiver) = mpsc::channel();
            let context = session::Context::prepare(
                state,
                &candidate,
                body,
                incoming,
                executable,
                Arc::clone(&cancelled),
            )?;
            let worker = std::thread::Builder::new()
                .name("emp-claude-turn".into())
                .spawn(move || {
                    let mut metrics = transcript::Metrics::default();
                    if let Err(error) = session::run(context, input, &result, &mut metrics) {
                        let _ = result.send(Reply::Complete(
                            metrics.output(json!({"answer":"","tool_calls":[]})),
                            Err(error),
                        ));
                    }
                })
                .map_err(|_| ClaudeCliError::Failure("claude_cli_spawn_failed"))?;
            let turn = Arc::new(Mutex::new(Turn {
                sender,
                receiver,
                cancelled,
                worker: Some(worker),
                pending: Vec::new(),
                turn_id,
                input_len: items.len(),
                history: history(body, items, route),
            }));
            state
                .backend
                .claude_turns
                .0
                .lock()
                .map_err(|_| ClaudeCliError::Failure("claude_cli_capacity_unavailable"))?
                .insert(key.clone(), Arc::clone(&turn));
            turn
        }
    };
    let mut turn = turn
        .lock()
        .map_err(|_| ClaudeCliError::Failure("claude_cli_process_failed"))?;
    if !turn.pending.is_empty() {
        turn.input_len = items.len();
        turn.history = history(body, items, route);
        let selected = tool_outputs
            .into_iter()
            .filter(|item| turn.pending.iter().any(|id| item["call_id"] == *id))
            .cloned()
            .collect::<Vec<_>>();
        turn.sender
            .send(ToolResults {
                input: selected,
                tools: body["tools"].clone(),
            })
            .map_err(|_| ClaudeCliError::Failure("claude_cli_process_failed"))?;
    }
    let mut cancelled = None;
    let (output, completed) = loop {
        if cancelled.is_none()
            && let Some(reason) = cancellation_reason(state, &mut monitor)
        {
            turn.cancelled.cancel();
            cancelled = Some(match reason {
                super::process::CancellationReason::ServerShutdown => ClaudeCliError::ShuttingDown,
                super::process::CancellationReason::Steering => {
                    ClaudeCliError::Failure("claude_cli_interrupted")
                }
                _ => ClaudeCliError::Disconnected,
            });
        }
        match turn.receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Reply::Dispatched) => usage.dispatch(),
            Ok(Reply::Text(id, delta)) => {
                if cancelled.is_none() && text(&id, &delta).is_err() {
                    turn.cancelled.cancel();
                    cancelled = Some(ClaudeCliError::Disconnected);
                }
            }
            Ok(Reply::Complete(output, result)) => {
                break (output, cancelled.take().map_or(result, Err));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break (
                    Value::Null,
                    Err(ClaudeCliError::Failure("claude_cli_process_failed")),
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    let result =
        completed.and_then(|()| projection::response_from_cli(&output, &candidate, body, 200));
    if result.is_err() && output["usage"].is_object() {
        usage.observe_upstream(
            &json!({"usage":emp_protocol::anthropic_projection::anthropic_usage(&output["usage"])}),
        );
    }
    turn.pending = result
        .as_ref()
        .ok()
        .into_iter()
        .flat_map(|response| response.body["output"].as_array().into_iter().flatten())
        .filter_map(|item| item["call_id"].as_str().map(str::to_owned))
        .collect();
    if stateless || turn.pending.is_empty() || result.is_err() {
        state
            .backend
            .claude_turns
            .0
            .lock()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_capacity_unavailable"))?
            .remove(&key);
    }
    result.map(|response| ClaudeCliCompletion {
        response,
        route: candidate,
    })
}

fn history(body: &Value, items: &[Value], route: &ResolvedRoute) -> u64 {
    let mut hash = DefaultHasher::new();
    (
        items,
        &body["instructions"],
        &body["reasoning"],
        &body["max_output_tokens"],
        &body["tool_choice"],
        &route.provider_id,
        &route.upstream_model,
        &route.endpoint_fingerprint,
        &route.provider.value()["api_key"],
    )
        .hash(&mut hash);
    hash.finish()
}

fn identity(
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Option<((String, String), Option<String>)> {
    let header = |name: &str| {
        incoming
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let anchor = body
        .as_object()
        .and_then(|body| emp_history::request_history_anchor(body, incoming).ok());
    let thread = anchor
        .as_ref()
        .and_then(|anchor| anchor.thread_id.as_deref())
        .or_else(|| header("session-id"))?;
    Some((
        (
            header("x-emp-caller-id").unwrap_or("local").into(),
            thread.into(),
        ),
        anchor.and_then(|anchor| anchor.turn_id),
    ))
}

impl TurnPool {
    pub(crate) fn selected(
        &self,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
    ) {
        if super::selected(route)
            || body["generate"] == false
            || route.upstream_model == "codex-auto-review"
            || crate::services::request_facts::memory_kind(body, incoming).is_some()
        {
            return;
        }
        if let Some((key, _)) = identity(body, incoming) {
            let old = self.0.lock().expect("Claude turn pool").remove(&key);
            drop(old);
        }
    }
}
