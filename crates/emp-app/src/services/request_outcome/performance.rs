//! Operation timing and rate definitions, independent of accounting and delivery.
use super::RequestOutcome;
use serde_json::{Value, json};

fn request_tokens_per_second(output_tokens: &Value, duration_ms: &Value) -> Option<f64> {
    let tokens = output_tokens
        .as_u64()
        .filter(|tokens| *tokens <= 10_000_000)?;
    let duration = duration_ms
        .as_f64()
        .filter(|duration| duration.is_finite() && *duration > 0.0 && *duration <= 86_400_000.0)?;
    let rate = tokens as f64 * 1000.0 / duration;
    (0.0..=1_000_000.0)
        .contains(&rate)
        .then(|| (rate * 100.0).round_ties_even() / 100.0)
}

fn generation_tokens_per_second(
    output_tokens: &Value,
    reasoning_tokens: &Value,
    duration_ms: &Value,
) -> Option<f64> {
    // A few milliseconds of batched delivery cannot measure generation speed.
    if duration_ms.as_u64()? < 100 {
        return None;
    }
    let output = output_tokens
        .as_u64()
        .filter(|tokens| *tokens <= 10_000_000)?;
    let reasoning = if reasoning_tokens.is_null() {
        0
    } else {
        reasoning_tokens.as_u64()?
    };
    request_tokens_per_second(&json!(output.checked_sub(reasoning)?), duration_ms)
}

impl RequestOutcome<'_> {
    pub(super) fn record_performance(&mut self, duration_ms: u64) {
        if let Some(first) = self.models.first_content {
            self.event["first_content_ms"] =
                json!(first.saturating_duration_since(self.started).as_millis() as u64);
        }
        if let Some(first) = self.first_token {
            self.event["ttft_ms"] = json!(first.duration_since(self.started).as_millis() as u64);
        }
        self.event
            .as_object_mut()
            .expect("route observation is an object")
            .remove("tokens_per_second");
        if self.event["success"] != true {
            return;
        }
        if let Some(rate) =
            request_tokens_per_second(&self.event["output_tokens"], &json!(duration_ms))
        {
            self.event["tokens_per_second"] = json!(rate);
        }
        if self.buffered
            || self.event["attempts"].as_u64().unwrap_or(0) > 1
            || self.event["retries"]
                .as_array()
                .is_some_and(|retries| !retries.is_empty())
        {
            return;
        }
        if let Some((first, last)) = self.models.first_content.zip(self.models.last_content) {
            let generation_ms = last.saturating_duration_since(first).as_millis() as u64;
            self.event["generation_ms"] = json!(generation_ms);
            if let Some(rate) = generation_tokens_per_second(
                &self.event["output_tokens"],
                &self.event["reasoning_tokens"],
                &json!(generation_ms),
            ) {
                self.event["generation_tokens_per_second"] = json!(rate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_tps_respects_observation_boundaries() {
        for (output, duration, expected) in [
            (json!(120), json!(2000), Some(60.0)),
            (json!(123), json!(4567), Some(26.93)),
            (json!(1), json!(86_400_000), Some(0.0)),
            (json!(0), json!(100), Some(0.0)),
            (json!(120), json!(0), None),
            (json!(true), json!(100), None),
            (json!(10_000_001), json!(100), None),
            (json!(120), json!(99), Some(1212.12)),
            (json!(120), json!(86_400_001), None),
            (json!(120), json!(100_000_000), None),
        ] {
            assert_eq!(request_tokens_per_second(&output, &duration), expected);
        }
    }

    #[test]
    fn generation_tps_subtracts_reasoning_and_requires_a_measurable_interval() {
        for (output, reasoning, duration, expected) in [
            (json!(120), json!(90), json!(500), Some(60.0)),
            (json!(120), Value::Null, json!(500), Some(240.0)),
            (json!(120), json!(121), json!(500), None),
            (json!(120), json!(-1), json!(500), None),
            (json!(120), json!(90), json!(0), None),
            (json!(100), json!(31), json!(6), None),
            (json!(120), json!(90), json!(99), None),
            (json!(120), json!(90), json!(100), Some(300.0)),
        ] {
            assert_eq!(
                generation_tokens_per_second(&output, &reasoning, &duration),
                expected
            );
        }
    }
}
