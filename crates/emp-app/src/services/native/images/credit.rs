use crate::app::ServerState;
use emp_core::{ResolvedRoute, RouteSource};
use emp_router::native_http::NativeHttpError;
use serde_json::{Value, json};
use time::{Date, OffsetDateTime, PrimitiveDateTime};

pub(crate) fn status(state: &ServerState) -> Result<Value, NativeHttpError> {
    let config =
        state.backend.configuration.read().map_err(|_| {
            NativeHttpError::router(503, "Image routing configuration is unavailable")
        })?;
    let credit = &config["image_routing"]["credit"];
    if credit.is_null() {
        return Ok(Value::Null);
    }
    let now = crate::util::system_now() as i64;
    let (started, ends) = period(credit, now);
    let (spent, unknown, limit) = period_usage(state, credit, started)?;
    Ok(
        json!({"estimated_spent_nanos":spent,"limit_nanos":limit,"unknown_requests":unknown,
        "active":started <= now && now < ends,"cycle_started_at":started,"cycle_ends_at":ends}),
    )
}

pub(super) fn route(
    state: &ServerState,
    config: &Value,
    body: &Value,
) -> Result<ResolvedRoute, NativeHttpError> {
    let credit = &config["image_routing"]["credit"];
    let now = crate::util::system_now() as i64;
    if config["image_routing"]["mode"] != "credit" {
        return Err(NativeHttpError::router(
            409,
            "The OpenAI image credit route is disabled",
        ));
    }
    let (started, expires) = period(credit, now);
    if now < started || now >= expires {
        return Err(NativeHttpError::router(
            409,
            "The OpenAI image credit period is inactive. Check the dates in routing settings.",
        ));
    }
    if body["stream"] == true || body["output_format"] == "svg" {
        return Err(NativeHttpError::router(
            422,
            "The OpenAI credit route supports non-streaming PNG, JPEG, WebP and GIF. Choose another image route for SVG or streaming.",
        ));
    }
    let id = credit["provider"]
        .as_str()
        .expect("normalized credit provider");
    let provider = config["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|provider| {
            provider["id"] == id
                && provider["enabled"] != false
                && provider["auth_mode"] == "api_key"
                && provider["base_url"] == "https://api.openai.com/v1"
        })
        .ok_or_else(|| {
            NativeHttpError::router(
                409,
                "Select an enabled official OpenAI API service for image credits",
            )
        })?;
    let (spent, unknown, limit) = period_usage(state, credit, started)?;
    if unknown > 0 {
        return Err(NativeHttpError::router(
            409,
            "An image request did not report billable usage. Check the project usage before continuing.",
        ));
    }
    if spent >= limit {
        return Err(NativeHttpError::router(
            429,
            "The OpenAI image credit has reached its local spending limit",
        ));
    }
    let key = emp_state::provider_api_key(provider, &state.backend.configuration.vault);
    if key.is_empty() {
        return Err(NativeHttpError::router(
            409,
            "The OpenAI image service has no project key",
        ));
    }
    let mut provider = provider.as_object().expect("configured provider").clone();
    provider.insert("api_key".into(), json!(key));
    provider.insert("image_credit".into(), json!(true));
    let model = credit["model"].as_str().expect("normalized credit model");
    super::routing::image_route(&provider, RouteSource::ForwardProvider, model)
}

fn period(credit: &Value, now: i64) -> (i64, i64) {
    let started = credit["cycle_started_at"]
        .as_i64()
        .expect("normalized credit cycle");
    let expires = credit["expires_at"]
        .as_i64()
        .expect("normalized credit expiry");
    if credit["renew_monthly"] != true || now < started {
        return (started, expires);
    }
    let anchor = OffsetDateTime::from_unix_timestamp(started).expect("normalized credit cycle");
    let current = OffsetDateTime::from_unix_timestamp(now.min(expires - 1))
        .expect("normalized credit period");
    let month_start = |year, month| {
        let date = Date::from_calendar_date(year, month, anchor.day().min(month.length(year)))
            .expect("valid billing calendar");
        PrimitiveDateTime::new(date, anchor.time())
            .assume_utc()
            .unix_timestamp()
    };
    let (mut year, mut month, _) = current.to_calendar_date();
    if month_start(year, month) > current.unix_timestamp() {
        if month == time::Month::January {
            year -= 1;
        }
        month = month.previous();
    }
    let end_year = year + i32::from(month == time::Month::December);
    (
        month_start(year, month),
        month_start(end_year, month.next()).min(expires),
    )
}

fn period_usage(
    state: &ServerState,
    credit: &Value,
    started: i64,
) -> Result<(i64, i64, i64), NativeHttpError> {
    let id = credit["provider"]
        .as_str()
        .expect("normalized credit provider");
    let budget = credit["budget_cents"]
        .as_i64()
        .expect("normalized credit allowance");
    let percent = credit["percent"].as_i64().expect("normalized credit limit");
    let (spent, unknown) = state
        .backend
        .usage
        .ledger
        .image_credit_cost(id, started)
        .map_err(|_| {
            NativeHttpError::router(503, "The local image credit ledger is unavailable")
        })?;
    Ok((spent, unknown, budget * percent * 100_000))
}

pub(super) fn estimated_cost_nanos(response: &Value) -> Option<i64> {
    let usage = &response["usage"];
    let input = usage["input_tokens"].as_u64()?;
    let output = usage["output_tokens"].as_u64()?;
    let details = &usage["input_tokens_details"];
    let text = details["text_tokens"].as_u64().unwrap_or(0).min(input);
    let image = input - text;
    i64::try_from(
        u128::from(text) * 5_000 + u128::from(image) * 8_000 + u128::from(output) * 30_000,
    )
    .ok()
}
