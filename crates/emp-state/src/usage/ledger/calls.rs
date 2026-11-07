//! Content-free call receipts and recovery of unrecorded memory usage.
use super::*;

mod report;
pub use report::CallFilter;

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS aggregate_bindings (caller TEXT NOT NULL, thread TEXT NOT NULL, model TEXT NOT NULL, owner TEXT NOT NULL, PRIMARY KEY(caller,thread,model));
        CREATE TABLE IF NOT EXISTS aggregate_cursors (model TEXT PRIMARY KEY, next INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS call_records (
        request_id TEXT NOT NULL, operation TEXT NOT NULL, started_at REAL NOT NULL,
        finished_at REAL NOT NULL, provider_id TEXT NOT NULL, account_id TEXT NOT NULL,
        model_id TEXT NOT NULL, session_id TEXT NOT NULL, state TEXT NOT NULL, facts TEXT NOT NULL,
        delivery TEXT, PRIMARY KEY(request_id, operation));
        CREATE INDEX IF NOT EXISTS call_period ON call_records(started_at);
        CREATE INDEX IF NOT EXISTS call_session ON call_records(session_id);",
    )?;
    Ok(())
}

pub(super) fn restore_memory_usage(
    ledger: &UsageLedger,
    connection: &mut Connection,
) -> Result<()> {
    let missing = rows(connection, "SELECT facts,finished_at FROM call_records c
        WHERE operation IN ('memory_extraction','memory_consolidation')
        AND json_type(facts,'$.usage_event_id')='text'
        AND NOT EXISTS(SELECT 1 FROM usage_events u WHERE u.id=json_extract(c.facts,'$.usage_event_id'))", &[])?;
    let transaction = connection.transaction()?;
    for record in missing {
        let mut event: Value = serde_json::from_str(record["facts"].as_str().ok_or(UsageError)?)
            .map_err(|_| UsageError)?;
        // Older receipts retained the turn/model under their request-field names.
        if event["usage_turn"].is_null() {
            event["usage_turn"] = event["turn_id"].clone();
        }
        if event["route_model"].is_null() {
            event["route_model"] = event["model_id"].clone();
        }
        let mut values =
            ledger.values(&event, record["finished_at"].as_f64().ok_or(UsageError)?)?;
        values[0] = sql(&event["usage_event_id"]);
        insert(&transaction, &values, &event)?;
    }
    transaction.commit()?;
    Ok(())
}

// Explicit fields only: no prompts, arbitrary errors, credentials or response bodies.
fn facts(event: &Value) -> Value {
    let mut result = Map::new();
    for field in [
        "request_id",
        "caller_id",
        "caller_name",
        "peer_ip",
        "aggregate_model",
        "aggregate_source",
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
        "session_kind",
        "resolved_protocol",
        "transport",
        "response_model_source",
        "model_name_status",
        "model_declarations",
        "selected_name",
        "provider_name",
        "requested_effort",
        "reported_effort",
        "requested_service_tier",
        "reported_service_tier",
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
        "generation_tokens_per_second",
        "generation_ms",
        "attempts",
        "retry_after_seconds",
        "continuation_recovery_from",
        "output_emitted",
        "tool_activity",
        "retries",
        "attempt_routes",
        "usage_response_id",
        "usage_turn",
        "route_model",
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
    pub fn query_callers(&self, start: f64, end: f64) -> Result<Value> {
        if !Self::valid_period(start, end, "all") {
            return Err(UsageError);
        }
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        let probes = crate::diagnostics::MODEL_TEST_OPERATIONS
            .map(|_| "?")
            .join(",");
        let args = [SqlValue::Real(start), SqlValue::Real(end)]
            .into_iter()
            .chain(
                crate::diagnostics::MODEL_TEST_OPERATIONS.map(|name| SqlValue::Text(name.into())),
            )
            .collect::<Vec<_>>();
        let rows = rows(&connection, &format!("SELECT
            COALESCE(json_extract(c.facts,'$.caller_id'),'unknown') AS caller_id,
            json_extract(c.facts,'$.caller_name') AS caller_name,
            GROUP_CONCAT(DISTINCT json_extract(c.facts,'$.peer_ip')) AS peer_ips,
            c.operation,c.provider_id,c.account_id,c.model_id,
            COUNT(*) AS calls,SUM(c.state='completed') AS completed,
            SUM(json_extract(c.facts,'$.input_tokens')) AS input_tokens,
            SUM(json_extract(c.facts,'$.output_tokens')) AS output_tokens,
            SUM(json_extract(c.facts,'$.cached_input_tokens')) AS cached_input_tokens,
            COUNT(CASE WHEN json_type(c.facts,'$.input_tokens')='integer'
                AND json_type(c.facts,'$.output_tokens')='integer' THEN 1 END) AS reported_calls,
            SUM(json_extract(c.facts,'$.duration_ms')) AS duration_ms,
            SUM(CASE WHEN u.price_issue IS NULL THEN u.cost_nanos END) AS cost_nanos,
            COUNT(CASE WHEN u.price_issue IS NULL THEN u.cost_nanos END) AS priced_calls
            FROM ({}) AS c LEFT JOIN accounted_usage u ON u.id=json_extract(c.facts,'$.usage_event_id')
            WHERE c.started_at>=? AND c.started_at<?
            AND c.operation NOT IN ({probes})
            GROUP BY caller_id,caller_name,c.operation,c.provider_id,c.account_id,c.model_id
            ORDER BY caller_id,c.operation,c.provider_id,c.model_id", crate::usage::call_outcome::REPORT_CALLS),
            &args)?;
        Ok(json!({"start":start,"end":end,"rows":rows}))
    }

    /// The last completed conversation turn identifies its account across restarts.
    /// Auxiliary jobs must never replace the conversation's selected route.
    pub fn latest_conversation_call(&self, thread: &str) -> Result<Option<Value>> {
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        rows(
            &self.connect()?,
            "SELECT facts FROM call_records WHERE session_id=? AND operation='responses'
             AND state='completed' AND COALESCE(json_extract(facts,'$.session_kind'),'')
             NOT IN ('review','memory','memory_extraction','memory_consolidation')
             ORDER BY finished_at DESC,request_id DESC LIMIT 1",
            &[SqlValue::Text(thread.into())],
        )?
        .first()
        .map(|row| {
            serde_json::from_str(row["facts"].as_str().ok_or(UsageError)?).map_err(|_| UsageError)
        })
        .transpose()
    }

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
        let connection = self.connect()?;
        connection.execute("INSERT INTO call_records
            (request_id,operation,started_at,finished_at,provider_id,account_id,model_id,session_id,state,facts)
            VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(request_id,operation) DO UPDATE SET
            started_at=excluded.started_at,finished_at=excluded.finished_at,provider_id=excluded.provider_id,
            account_id=excluded.account_id,model_id=excluded.model_id,session_id=excluded.session_id,
            state=excluded.state,facts=excluded.facts", params![id, text(event,"route","responses"),
            started, finished, text(event,"provider_id",""), text(event,"account_id",""),
            text(event,"model_id",""), &session, state, record.to_string()])?;
        if state == "completed"
            && let Some(prior) = event["continuation_recovery_from"].as_str()
        {
            connection.execute(
                "UPDATE call_records SET facts=json_set(facts,'$.recovery_request_id',?)
                 WHERE request_id=? AND operation=? AND state='recovery_required'",
                params![id, prior, text(event, "route", "responses")],
            )?;
        }
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
