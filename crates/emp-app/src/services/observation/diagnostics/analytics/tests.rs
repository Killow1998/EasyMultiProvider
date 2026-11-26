use super::summarize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn record(observed_at: &str, first_content_ms: Value, generation_tps: Value) -> Value {
    json!({
        "route":"responses",
        "model_id":"gpt-6-luna",
        "speed_mode":"standard",
        "status":200,
        "error_class":"none",
        "performance_schema":5,
        "observed_at":observed_at,
        "first_content_ms":first_content_ms,
        "generation_tokens_per_second":generation_tps,
        "generation_ms":1000,
    })
}

#[test]
fn recovery_and_missing_outcomes_are_not_health_failures() {
    let now = OffsetDateTime::now_utc();
    let records = [
        json!({"route":"responses","status":200,"error_class":"none"}),
        json!({"route":"responses","status":502,"error_class":"stream_error"}),
        json!({"route":"responses","status":400,"error_class":"stream_error","error_code":"previous_response_not_found"}),
        json!({"route":"responses","error_class":"client_disconnect"}),
        json!({"route":"responses","error_class":"unknown"}),
    ];
    let health = &summarize(&records, now)["health"];
    assert_eq!(health["sample_count"], 2);
    assert_eq!(health["failure_count"], 1);
    assert_eq!(health["success_rate"], 50.0);
    assert_eq!(health["recovery_request_count"], 1);
    assert_eq!(health["cancelled_count"], 1);
    assert_eq!(health["unknown_count"], 1);
}
#[test]
fn visible_metrics_exclude_legacy_first_output_and_full_request_rates() {
    let now = OffsetDateTime::parse("2026-09-23T12:00:00Z", &Rfc3339).unwrap();
    let stamp = now.format(&Rfc3339).unwrap();
    let result = summarize(
        &[
            json!({"route":"responses","model_id":"gpt-6-luna","speed_mode":"standard",
                "status":200,"error_class":"none","performance_schema":4,
                "observed_at":stamp,"ttft_ms":1,"tokens_per_second":999.0}),
            record(&stamp, json!(100), Value::Null),
            record(&stamp, json!(200), json!(60.0)),
        ],
        now,
    );
    let model = &result["models"][0];
    assert_eq!(model["call_count"], 2);
    assert_eq!(model["ttft_ms"], 150.0);
    assert_eq!(model["ttft_samples"], 2);
    assert_eq!(model["tokens_per_second"], 60.0);
    assert_eq!(model["tps_samples"], 1);
    let mut burst = record(&stamp, json!(11916), json!(11500.0));
    burst["generation_ms"] = json!(6);
    assert_eq!(
        super::metric(&[&burst], "generation_tokens_per_second"),
        (None, 0)
    );
}
