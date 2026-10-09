//! Durable, content-free call facts beside the existing accounting ledger.
//! Usage totals continue to use accounted_usage; this table never reprices them.
use super::*;

pub struct CallFilter {
    pub start: f64,
    pub end: f64,
    pub category: Option<String>,
    pub provider: Option<String>,
    pub account: Option<String>,
    pub model: Option<String>,
    pub models: Vec<String>,
    pub session: Option<String>,
    pub state: Option<String>,
    pub request: Option<String>,
    pub offset: usize,
    pub limit: usize,
    pub models_offset: usize,
    pub models_sort: String,
}

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
        let state = if event["error_class"] == "client_cancelled" {
            "interrupted"
        } else if event["error_class"] == "client_disconnect" {
            "cancelled"
        } else if event["success"] == true {
            "completed"
        } else if event["status"].is_null() {
            "interrupted"
        } else {
            "failed"
        };
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

    pub fn query_calls(&self, filter: &CallFilter) -> Result<Value> {
        if !Self::valid_period(filter.start, filter.end, "all")
            || filter.limit == 0
            || filter.limit > 100
        {
            return Err(UsageError);
        }
        let mut conditions = vec!["started_at >= ?".to_owned(), "started_at <= ?".to_owned()];
        let mut args = vec![SqlValue::Real(filter.start), SqlValue::Real(filter.end)];
        for (column, value) in [
            ("provider_id", &filter.provider),
            ("account_id", &filter.account),
            ("model_id", &filter.model),
            ("session_id", &filter.session),
            ("state", &filter.state),
            ("request_id", &filter.request),
        ] {
            if let Some(value) = value {
                conditions.push(format!("{column}=?"));
                args.push(SqlValue::Text(value.clone()));
            }
        }
        if let Some(category) = &filter.category {
            if !CATEGORIES.contains(&category.as_str()) {
                return Err(UsageError);
            }
            conditions.push("json_extract(facts,'$.usage_category')=?".into());
            args.push(SqlValue::Text(category.clone()));
        }
        if !filter.models.is_empty() {
            if filter.models.len() > 64 {
                return Err(UsageError);
            }
            conditions.push(format!(
                "model_id IN ({})",
                vec!["?"; filter.models.len()].join(",")
            ));
            args.extend(filter.models.iter().cloned().map(SqlValue::Text));
        }
        let predicate = conditions.join(" AND ");
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        let count: i64 = connection.query_row(
            &format!("SELECT COUNT(*) FROM call_records WHERE {predicate}"),
            params_from_iter(&args),
            |row| row.get(0),
        )?;
        let summary = rows(&connection, &format!("SELECT COUNT(*) AS calls,
            SUM(json_extract(facts,'$.input_tokens')) AS input_tokens,
            SUM(json_extract(facts,'$.output_tokens')) AS output_tokens,
            SUM(CASE WHEN json_type(facts,'$.cached_input_tokens')='integer' AND json_extract(facts,'$.input_tokens') >= json_extract(facts,'$.cached_input_tokens') THEN json_extract(facts,'$.cached_input_tokens') END) AS cached_input_tokens,
            SUM(CASE WHEN json_type(facts,'$.cached_input_tokens')='integer' AND json_extract(facts,'$.input_tokens') >= json_extract(facts,'$.cached_input_tokens') THEN json_extract(facts,'$.input_tokens') END) AS cache_input_tokens,
            SUM(state='completed') AS completed,
            AVG(CASE WHEN state='completed' THEN json_extract(facts,'$.duration_ms') END) AS duration_ms,
            AVG(CASE WHEN state='completed' AND json_extract(facts,'$.performance_schema')=4 THEN json_extract(facts,'$.ttft_ms') END) AS ttft_ms,
            COUNT(CASE WHEN state='completed' AND json_extract(facts,'$.performance_schema')=4 THEN json_extract(facts,'$.ttft_ms') END) AS ttft_samples,
            AVG(CASE WHEN state='completed' THEN json_extract(facts,'$.tokens_per_second') END) AS tokens_per_second,
            COUNT(CASE WHEN state='completed' THEN json_extract(facts,'$.tokens_per_second') END) AS tps_samples
            FROM call_records WHERE {predicate}"), &args)?;
        let model_order = match filter.models_sort.as_str() {
            "calls" => "calls DESC",
            "model" => "model_id ASC",
            "duration" => "duration_ms DESC",
            "ttft" => "ttft_ms DESC",
            "tps" => "tokens_per_second DESC",
            _ => return Err(UsageError),
        };
        let grouped = format!("SELECT provider_id, account_id, model_id,
            json_extract(facts,'$.upstream_model') AS upstream_model,
            json_extract(facts,'$.speed_mode') AS speed_mode,
            json_extract(facts,'$.requested_effort') AS requested_effort,
            COUNT(*) AS calls, SUM(state='completed') AS completed,
            AVG(CASE WHEN state='completed' THEN json_extract(facts,'$.duration_ms') END) AS duration_ms,
            AVG(CASE WHEN state='completed' AND json_extract(facts,'$.performance_schema')=4 THEN json_extract(facts,'$.ttft_ms') END) AS ttft_ms,
            AVG(CASE WHEN state='completed' THEN json_extract(facts,'$.tokens_per_second') END) AS tokens_per_second
            FROM call_records WHERE {predicate}
            GROUP BY provider_id,account_id,model_id,upstream_model,speed_mode,requested_effort");
        let models_total: i64 = connection.query_row(
            &format!("SELECT COUNT(*) FROM ({grouped})"),
            params_from_iter(&args),
            |row| row.get(0),
        )?;
        let mut model_args = args.clone();
        model_args.push(SqlValue::Integer(
            filter.models_offset.min(i64::MAX as usize) as i64,
        ));
        let models = rows(
            &connection,
            &format!(
                "{grouped} ORDER BY {model_order},provider_id,account_id,model_id,upstream_model,speed_mode,requested_effort LIMIT 50 OFFSET ?"
            ),
            &model_args,
        )?;
        let bucket = ((filter.end - filter.start) / 48.0).max(60.0);
        let periods = rows(&connection, &format!("SELECT CAST((started_at-{})/{bucket} AS INTEGER)*{bucket}+{} AS start,
            COUNT(*) AS calls, AVG(CASE WHEN state='completed' AND json_extract(facts,'$.performance_schema')=4 THEN json_extract(facts,'$.ttft_ms') END) AS ttft_ms,
            COUNT(CASE WHEN state='completed' AND json_extract(facts,'$.performance_schema')=4 THEN json_extract(facts,'$.ttft_ms') END) AS ttft_samples,
            AVG(CASE WHEN state='completed' THEN json_extract(facts,'$.tokens_per_second') END) AS tokens_per_second,
            COUNT(CASE WHEN state='completed' THEN json_extract(facts,'$.tokens_per_second') END) AS tps_samples
            FROM call_records WHERE {predicate} GROUP BY CAST((started_at-{})/{bucket} AS INTEGER) ORDER BY start",filter.start,filter.start,filter.start), &args)?;
        args.extend([
            SqlValue::Integer(filter.limit as i64),
            SqlValue::Integer(filter.offset.min(i64::MAX as usize) as i64),
        ]);
        let mut statement = connection.prepare(&format!("SELECT facts,delivery,
            (SELECT json_object('cost_nanos',cost_nanos,'price_issue',price_issue,'price_key',price_key)
             FROM accounted_usage WHERE id=json_extract(call_records.facts,'$.usage_event_id') LIMIT 1)
            FROM call_records WHERE {predicate}
            ORDER BY started_at DESC,request_id DESC,operation LIMIT ? OFFSET ?"))?;
        let records = statement
            .query_map(params_from_iter(&args), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .map(|row| {
                let (raw, delivery, cost) = row?;
                let mut value: Value = serde_json::from_str(&raw).map_err(|_| UsageError)?;
                if let Some(delivery) = delivery {
                    value["delivery"] =
                        serde_json::from_str::<Value>(&delivery).map_err(|_| UsageError)?;
                }
                if let Some(cost) = cost {
                    value["pricing"] =
                        serde_json::from_str::<Value>(&cost).map_err(|_| UsageError)?;
                }
                value.as_object_mut().unwrap().remove("usage_event_id");
                Ok(value)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"records":records,"total":count,"offset":filter.offset,"limit":filter.limit,
            "summary":summary.first(),"models":models,"periods":periods,"models_total":models_total,"models_offset":filter.models_offset,"models_limit":50,
            "start":filter.start,"end":filter.end,"metric_schema":4}),
        )
    }
}
