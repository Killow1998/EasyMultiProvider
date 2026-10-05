//! Request-local compaction: retain the active turn, compact only visible history.
use super::budget::positive_integer;
use super::estimate::{IncrementalEstimate, estimate_json_tokens, json_cost};
use serde_json::{Map, Value};
use summary::{map_reduce, message};
use units::{flatten, split_atomic_units, visible_candidate};

mod summary;
mod units;

/// Upper bound on atomic history units considered for one destination compaction.
const MAX_COMPACTION_UNITS: usize = 50_000;

pub fn compact_with<F>(
    body: &Value,
    model: &Map<String, Value>,
    safe_budget: u64,
    mut summarize: F,
) -> Result<Value, &'static str>
where
    F: FnMut(&Value) -> Result<String, ()>,
{
    if safe_budget == 0 {
        return Err("compaction_budget_invalid");
    }
    let root = body.as_object().ok_or("invalid_history_projection")?;
    let mut projected = root.clone();
    let active_start = projected
        .remove(crate::ACTIVE_INPUT_START)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok());
    // Every body built below replaces `input`; keep the key (and its position)
    // but drop the original array so it is never cloned or re-serialised again.
    let mut source = match projected.get_mut("input").map(Value::take) {
        Some(Value::Array(items)) => items,
        Some(Value::Object(item)) => vec![Value::Object(item)],
        Some(Value::String(text)) => vec![Value::String(text)],
        _ => Vec::new(),
    };
    let suffix = if source
        .last()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("compaction_trigger")
    {
        source.pop().into_iter().collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let (candidate, active) =
        if let Some(index) = active_start.filter(|index| *index <= source.len()) {
            let active = source.split_off(index);
            (source, active)
        } else if !suffix.is_empty() {
            (source, Vec::new())
        } else {
            let mut units = split_atomic_units(&source, false);
            match units.pop() {
                Some(last) => (units.into_iter().flatten().collect(), last),
                None => (Vec::new(), Vec::new()),
            }
        };
    let before = input_view_value(&final_body(
        &projected,
        None,
        std::slice::from_ref(&candidate),
        &active,
        &suffix,
    ));
    if estimate_json_tokens(&before).is_some_and(|estimate| estimate <= safe_budget) {
        projected.insert(
            "input".to_owned(),
            Value::Array([candidate, active, suffix].concat()),
        );
        return Ok(Value::Object(projected));
    }
    drop(before);
    let active_only = input_view_value(&final_body(&projected, None, &[], &active, &suffix));
    if estimate_json_tokens(&active_only).is_none_or(|estimate| estimate > safe_budget) {
        return Err("compaction_unit_too_large");
    }
    let units = split_atomic_units(
        &candidate
            .into_iter()
            .filter(visible_candidate)
            .collect::<Vec<_>>(),
        true,
    );
    if units.len() > MAX_COMPACTION_UNITS {
        return Err("compaction_unit_too_large");
    }
    let configured_output = positive_integer(projected.get("max_output_tokens"))
        .or_else(|| positive_integer(projected.get("max_tokens")))
        .or_else(|| positive_integer(model.get("max_output_tokens")))
        .or_else(|| positive_integer(model.get("output_limit")))
        .unwrap_or(1024);
    let output_limit = configured_output.min((safe_budget / 8).max(1));
    let placeholder = "x".repeat(usize::try_from(output_limit.saturating_mul(2)).unwrap_or(2048));
    // The tentative body is the placeholder checkpoint, the retained units,
    // then the active turn and suffix; units only add their own items.
    let mut tail = IncrementalEstimate::new(
        &input_view_value(&final_body(
            &projected,
            Some(&placeholder),
            &[],
            &active,
            &suffix,
        )),
        1 + active.len() + suffix.len(),
    );
    let mut kept = 0;
    for unit in units.iter().rev() {
        let costs = unit
            .iter()
            .map(|item| json_cost(item, 2))
            .collect::<Option<Vec<_>>>();
        let fits = costs.as_ref().is_some_and(|costs| {
            tail.tokens_with(costs)
                .is_some_and(|estimate| estimate <= safe_budget)
        });
        match costs {
            Some(costs) if fits => {
                tail.extend(&costs);
                kept += 1;
            }
            _ => break,
        }
    }
    let (mapped, retained) = units.split_at(units.len() - kept);
    let summary = if mapped.is_empty() {
        None
    } else {
        Some(map_reduce(
            mapped,
            body.get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            safe_budget,
            output_limit,
            &mut summarize,
        )?)
    };
    let result = final_body(&projected, summary.as_deref(), retained, &active, &suffix);
    if estimate_json_tokens(&input_view_value(&result))
        .is_none_or(|estimate| estimate > safe_budget)
    {
        return Err("compaction_result_over_budget");
    }
    Ok(result)
}

fn final_body(
    root: &Map<String, Value>,
    summary: Option<&str>,
    tail: &[Vec<Value>],
    active: &[Value],
    suffix: &[Value],
) -> Value {
    let mut input = Vec::new();
    if let Some(summary) = summary {
        input.push(message(&format!(
            "{}\n\n{summary}",
            crate::CHECKPOINT_PREFIX
        )));
    }
    input.extend(flatten(tail));
    input.extend_from_slice(active);
    input.extend_from_slice(suffix);
    let mut result = root.clone();
    result.insert("input".to_owned(), Value::Array(input));
    Value::Object(result)
}

fn input_view_value(body: &Value) -> Value {
    let Some(root) = body.as_object() else {
        return Value::Null;
    };
    Value::Object(
        ["input", "instructions", "tools", "text", "response_format"]
            .into_iter()
            .filter_map(|key| root.get(key).cloned().map(|value| (key.to_owned(), value)))
            .collect(),
    )
}

#[cfg(test)]
mod tests;
