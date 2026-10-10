//! Durable, content-free call facts beside the existing accounting ledger.
//! Usage totals continue to use accounted_usage; this table never reprices them.
use super::*;

mod report;
pub use report::CallFilter;

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS call_records (
        request_id TEXT NOT NULL, operation TEXT NOT NULL, started_at REAL NOT NULL,
        finished_at REAL NOT NULL, provider_id TEXT NOT NULL, account_id TEXT NOT NULL,
        model_id TEXT NOT NULL, session_id TEXT NOT NULL, state TEXT NOT NULL, facts TEXT NOT NULL,
        delivery TEXT, PRIMARY KEY(request_id, operation));
        CREATE INDEX IF NOT EXISTS call_period ON call_records(started_at);",
    )?;
    Ok(())
}

// Explicit fields only: no prompts, arbitrary errors, credentials or response bodies.
fn facts(event: &Value) -> Value {
    let mut result = Map::new();
    for field in [
        "request_id",
        "route",
        "client_model",
        "upstream_model",
        "response_model",
        "provider_id",
        "model_id",
        "account_id",
        "session_id",
        "thread_id",
        "turn_id",
        "parent_thread_id",
        "resolved_protocol",
        "transport",
        "response_model_source",
        "model_name_status",
        "model_declarations",
        "selected_name",
        "provider_name",
        "requested_effort",
        "service_tier",
        "performance_schema",
        "speed_mode",
        "response_status",
        "error_class",
        "error_origin",
        "error_code",
        "failure_reason",
        "status",
        "success",
        "duration_ms",
        "ttft_ms",
        "first_content_ms",
        "tokens_per_second",
        "attempts",
        "retries",
        "attempt_routes",
        "usage_response_id",
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cache_write_tokens",
        "cache_write_1h_tokens",
        "reasoning_tokens",
        "usage_category",
        "usage_owner",
    ] {
        if let Some(value) = event.get(field) {
            let value = match value {
                Value::String(text)
                    if text.len() <= 4096 && !text.chars().any(char::is_control) =>
                {
                    value.clone()
                }
                Value::Number(_) | Value::Bool(_) | Value::Null => value.clone(),
                Value::Array(items)
                    if matches!(field, "model_declarations" | "retries" | "attempt_routes") =>
                {
                    json!(items.iter().take(16).collect::<Vec<_>>())
                }
                _ => continue,
            };
            result.insert(field.into(), value);
        }
    }
    Value::Object(result)
}

impl UsageLedger {
    pub fn record_call(&self, event: &Value, started: f64, finished: f64) -> Result<()> {
        let id = text(event, "request_id", "");
        if id.is_empty() || id.len() > 256 || !started.is_finite() || !finished.is_finite() {
            return Err(UsageError);
        }
        let mut record = facts(event);
        if let Ok(values) = self.values(event, finished)
            && let Some(SqlValue::Text(id)) = values.first()
        {
            record["usage_event_id"] = json!(id);
        }
        let state = crate::usage::call_state(event);
        record["state"] = json!(state);
        record["started_at"] = json!(started);
        record["finished_at"] = json!(finished);
        let session = text(&record, "thread_id", text(&record, "session_id", "")).to_owned();
        record["session_id"] = json!(session);
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        self.connect()?.execute("INSERT INTO call_records
            (request_id,operation,started_at,finished_at,provider_id,account_id,model_id,session_id,state,facts)
            VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(request_id,operation) DO UPDATE SET
            started_at=excluded.started_at,finished_at=excluded.finished_at,provider_id=excluded.provider_id,
            account_id=excluded.account_id,model_id=excluded.model_id,session_id=excluded.session_id,
            state=excluded.state,facts=excluded.facts", params![id, text(event,"route","responses"),
            started, finished, text(event,"provider_id",""), text(event,"account_id",""),
            text(event,"model_id",""), &session, state, record.to_string()])?;
        Ok(())
    }

    pub fn record_call_delivery(&self, id: &str, operation: &str, delivery: &Value) -> Result<()> {
        let delivery: Map<String, Value> = [
            "delivery",
            "downstream_terminal",
            "duration_ms",
            "writes_completed",
            "error_origin",
            "error_code",
            "last_phase",
            "response_status",
        ]
        .into_iter()
        .filter_map(|key| delivery.get(key).map(|value| (key.into(), value.clone())))
        .collect();
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        self.connect()?.execute(
            "UPDATE call_records SET delivery=? WHERE request_id=? AND operation=?",
            params![Value::Object(delivery).to_string(), id, operation],
        )?;
        Ok(())
    }
}
