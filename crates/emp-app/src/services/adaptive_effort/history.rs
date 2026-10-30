use super::{Decision, Effort};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;

type Key = [u8; 32];

#[derive(Default)]
pub(crate) struct State(Mutex<History>);

#[derive(Default)]
struct History {
    steps: HashMap<Key, Decision>,
    order: VecDeque<Key>,
    contexts: BTreeMap<Key, Value>,
}

pub(super) struct Step {
    scope: Key,
    keys: Vec<Key>,
    boundaries: Vec<Option<Key>>,
    pub context: Value,
    pub previous: Vec<Option<Decision>>,
}

impl State {
    pub(super) fn prepare(&self, scope: &Value, input: &[Value], incremental: bool) -> Step {
        let mut digest = Sha256::new();
        digest.update(serde_json::to_vec(scope).expect("scope JSON"));
        let scope: Key = digest.clone().finalize().into();
        let mut keys = Vec::with_capacity(input.len());
        let mut boundaries = Vec::with_capacity(input.len());
        for item in input {
            let bytes = serde_json::to_vec(item).expect("input JSON");
            boundaries.push(
                matches!(
                    item["type"].as_str(),
                    Some("function_call_output" | "custom_tool_call_output")
                )
                .then(|| {
                    let mut boundary = Sha256::new();
                    boundary.update(scope);
                    boundary.update(b"tool-result");
                    boundary.update(&bytes);
                    boundary.finalize().into()
                }),
            );
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
            keys.push(digest.clone().finalize().into());
        }
        let history = self.0.lock().unwrap();
        let mut context = if incremental {
            history
                .contexts
                .get(&scope)
                .cloned()
                .unwrap_or_else(|| json!({}))
        } else {
            json!({})
        };
        let mut tools = VecDeque::new();
        for item in input {
            match item["type"].as_str() {
                Some("function_call_output" | "custom_tool_call_output") => {
                    tools.push_back(
                        json!({"type":item["type"],"output":excerpt(&item["output"],1600)}),
                    );
                    if tools.len() > 4 {
                        tools.pop_front();
                    }
                }
                Some("message") | None => match item["role"].as_str() {
                    Some("user") => context["user"] = json!(excerpt(&item["content"], 2000)),
                    Some("assistant") => {
                        context["progress"] = json!(excerpt(&item["content"], 1600))
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if !tools.is_empty() || !incremental {
            context["tool_results"] = json!(tools);
        }
        let previous = keys
            .iter()
            .zip(&boundaries)
            .map(|(key, boundary)| {
                history
                    .steps
                    .get(key)
                    .or_else(|| boundary.and_then(|key| history.steps.get(&key)))
                    .cloned()
            })
            .collect();
        Step {
            scope,
            keys,
            boundaries,
            context,
            previous,
        }
    }

    pub(super) fn remember(&self, step: &Step, decision: &Decision) {
        let Some(key) = step.keys.last() else {
            return;
        };
        let mut history = self.0.lock().unwrap();
        for key in [Some(*key), *step.boundaries.last().unwrap()]
            .into_iter()
            .flatten()
        {
            if !history.steps.contains_key(&key) {
                history.order.push_back(key);
            }
            history.steps.insert(key, decision.clone());
        }
        while history.order.len() > 2048 {
            let old = history.order.pop_front().unwrap();
            history.steps.remove(&old);
        }
        if !history.contexts.contains_key(&step.scope) && history.contexts.len() >= 128 {
            history.contexts.pop_first();
        }
        history.contexts.insert(step.scope, step.context.clone());
    }
}

impl Step {
    // Codex saves its own history. Restore EMP updates at matching prefixes or
    // call-ID-bearing tool results when an incremental WS request becomes a full replay.
    pub(super) fn rewrite(
        &self,
        input: Vec<Value>,
        effort: Effort,
        baseline: Option<Effort>,
    ) -> Vec<Value> {
        let mut effective = baseline;
        let length = input.len();
        let mut output = Vec::with_capacity(length + self.previous.iter().flatten().count() + 1);
        for (index, item) in input.into_iter().enumerate() {
            if item["type"] == "configuration_update" {
                effective = serde_json::from_value(item["reasoning"]["effort"].clone()).ok();
            }
            output.push(item);
            let selected = if index + 1 == length {
                Some(effort)
            } else {
                self.previous[index]
                    .as_ref()
                    .map(|decision| decision.effort)
            };
            if let Some(selected) = selected
                && effective != Some(selected)
            {
                output.push(json!({"type":"configuration_update","reasoning":{"effort":selected}}));
                effective = Some(selected);
            }
        }
        output
    }
}

fn excerpt(value: &Value, limit: usize) -> String {
    let text = if let Some(text) = value.as_str() {
        text.to_owned()
    } else if let Some(parts) = value.as_array() {
        parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    if text.chars().count() <= limit {
        return text;
    }
    let head: String = text.chars().take(limit / 2).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(limit / 2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n[…]\n{tail}")
}
