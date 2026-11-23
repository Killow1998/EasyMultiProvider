use super::budget::positive_integer;
#[cfg(test)]
use super::estimate::estimate_json_tokens;
use super::estimate::{
    IncrementalEstimate, estimate_protocol_payload_tokens, json_cost, payload_view,
};
use serde_json::{Map, Value};
use summary::{map_reduce, message};
use units::{flatten, split_atomic_units, visible_candidate};

mod cache;
pub use cache::SummaryCache;
mod summary;
mod units;

const MAX_COMPACTION_UNITS: usize = 50_000;

pub fn compact_with<F>(
    body: &Value,
    model: &Map<String, Value>,
    safe_budget: u64,
    summarize: F,
) -> Result<Value, &'static str>
where
    F: FnMut(&Value) -> Result<String, ()>,
{
    compact_with_cache(body, model, safe_budget, None, summarize)
}

pub fn compact_with_cache<F>(
    body: &Value,
    model: &Map<String, Value>,
    safe_budget: u64,
    cache: Option<(&SummaryCache, &[&str])>,
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
            let cut = units::active_turn_boundary(&source, index);
            let active = source.split_off(cut);
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
    let fits_budget = |request: &Value| {
        estimate_protocol_payload_tokens(request, "responses")
            .is_some_and(|tokens| tokens <= safe_budget)
    };
    if fits_budget(&final_body(
        &projected,
        None,
        std::slice::from_ref(&candidate),
        &active,
        &suffix,
    )) {
        projected.insert(
            "input".to_owned(),
            Value::Array([candidate, active, suffix].concat()),
        );
        return Ok(Value::Object(projected));
    }
    if !fits_budget(&final_body(&projected, None, &[], &active, &suffix)) {
        return Err("compaction_unit_too_large");
    }
    let mut units = split_atomic_units(
        &candidate
            .into_iter()
            .filter(visible_candidate)
            .collect::<Vec<_>>(),
        true,
    );
    if units.len() > MAX_COMPACTION_UNITS {
        return Err("compaction_unit_too_large");
    }
    let prefixes = cache.map(|(cache, scope)| (cache, cache::prefixes(scope, safe_budget, &units)));
    if let Some((cache, keys)) = &prefixes
        && let Some((covered, summary)) = cache.longest_match(keys)
    {
        let remaining = units.split_off(covered);
        let resumed = final_body(&projected, Some(&summary), &remaining, &active, &suffix);
        if fits_budget(&resumed) {
            return Ok(resumed);
        }
        units = vec![vec![checkpoint(&summary)]];
        units.extend(remaining);
    }
    let configured_output = positive_integer(projected.get("max_output_tokens"))
        .or_else(|| positive_integer(projected.get("max_tokens")))
        .or_else(|| positive_integer(model.get("max_output_tokens")))
        .or_else(|| positive_integer(model.get("output_limit")))
        .unwrap_or(1024);
    let output_limit = configured_output.min((safe_budget / 8).max(1));
    let placeholder = "x".repeat(usize::try_from(output_limit.saturating_mul(2)).unwrap_or(2048));
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
    if !fits_budget(&result) {
        return Err("compaction_result_over_budget");
    }
    if let (Some((cache, keys)), Some(summary)) = (prefixes, summary) {
        let covered = keys.len() - kept;
        cache.insert(keys[covered - 1], covered, summary);
    }
    Ok(result)
}

fn checkpoint(summary: &str) -> Value {
    message(&format!("{}\n\n{summary}", crate::CHECKPOINT_PREFIX))
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
        input.push(checkpoint(summary));
    }
    input.extend(flatten(tail));
    input.extend_from_slice(active);
    input.extend_from_slice(suffix);
    let mut result = root.clone();
    result.insert("input".to_owned(), Value::Array(input));
    Value::Object(result)
}

fn input_view_value(body: &Value) -> Value {
    payload_view(body, "responses").expect("Responses request object")
}

#[cfg(test)]
mod tests;
