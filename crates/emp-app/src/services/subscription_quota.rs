//! Read subscription capacity without treating absent usage as zero capacity.
use serde_json::Value;

pub(crate) fn headroom(quota: Option<&Value>) -> Option<f64> {
    capacity(quota, None)
}

pub(crate) fn current_headroom(quota: Option<&Value>) -> Option<f64> {
    capacity(quota, Some(crate::util::system_now()))
}

fn capacity(quota: Option<&Value>, now: Option<f64>) -> Option<f64> {
    let quota = quota?.as_object()?;
    if quota
        .get("credits")
        .and_then(|credits| credits.get("spend_control_reached"))
        == Some(&Value::Bool(true))
        || quota
            .get("credits")
            .and_then(|credits| credits.get("individual_limit"))
            .and_then(|limit| limit.get("remaining_percent"))
            .and_then(finite_number)
            .is_some_and(|remaining| remaining <= 0.0)
    {
        return Some(0.0);
    }
    window_headroom(quota.get("rate_limits"), now)
}

pub(crate) fn image_headroom(quota: Option<&Value>) -> Option<f64> {
    let now = Some(crate::util::system_now());
    let general = capacity(quota, now);
    let image = window_headroom(
        quota
            .and_then(|quota| quota.get("rate_limits_by_limit_id"))
            .and_then(|limits| limits.get("image_gen")),
        now,
    );
    match (general, image) {
        (Some(general), Some(image)) => Some(general.min(image)),
        (general, image) => general.or(image),
    }
}

fn window_headroom(limits: Option<&Value>, now: Option<f64>) -> Option<f64> {
    let limits = limits?.as_object()?;
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|name| limits.get(name))
        .filter(|window| {
            !now.is_some_and(|now| {
                window
                    .get("resetsAt")
                    .or_else(|| window.get("resets_at"))
                    .and_then(finite_number)
                    .is_some_and(|reset| reset <= now)
            })
        })
        .filter_map(|window| {
            window
                .get("usedPercent")
                .or_else(|| window.get("used_percent"))
        })
        .filter_map(finite_number)
        .map(|used| (100.0 - used).clamp(0.0, 100.0))
        .reduce(f64::min)
}

fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|value| value.is_finite())
}
