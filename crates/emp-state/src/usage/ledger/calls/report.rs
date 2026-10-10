//! Filtered read projection; recording and accounting remain in their owners.
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

impl UsageLedger {
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
        let calls = crate::usage::call_outcome::REPORT_CALLS;
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        let count: i64 = connection.query_row(
            &format!("SELECT COUNT(*) FROM {calls} WHERE {predicate}"),
            params_from_iter(&args),
            |row| row.get(0),
        )?;
        let mut summary = rows(&connection, &format!("SELECT COUNT(*) AS calls,
            SUM(json_extract(facts,'$.input_tokens')) AS input_tokens,
            SUM(json_extract(facts,'$.output_tokens')) AS output_tokens,
            SUM(CASE WHEN json_type(facts,'$.cached_input_tokens')='integer' AND json_extract(facts,'$.input_tokens') >= json_extract(facts,'$.cached_input_tokens') THEN json_extract(facts,'$.cached_input_tokens') END) AS cached_input_tokens,
            SUM(CASE WHEN json_type(facts,'$.cached_input_tokens')='integer' AND json_extract(facts,'$.input_tokens') >= json_extract(facts,'$.cached_input_tokens') THEN json_extract(facts,'$.input_tokens') END) AS cache_input_tokens,
            SUM(state='completed') AS completed,
            SUM(state='failed') AS failed,
            SUM(state='interrupted') AS interrupted,
            SUM(state='cancelled') AS cancelled,
            SUM(state='recovery_required') AS recovery_required,
            SUM(state='unknown') AS unknown,
            SUM(state IN ('completed','failed')) AS success_samples,
            AVG(CASE WHEN state='completed' THEN json_extract(facts,'$.duration_ms') END) AS duration_ms,
            AVG(CASE WHEN state='completed' AND json_extract(facts,'$.performance_schema')=4 THEN json_extract(facts,'$.ttft_ms') END) AS ttft_ms,
            COUNT(CASE WHEN state='completed' AND json_extract(facts,'$.performance_schema')=4 THEN json_extract(facts,'$.ttft_ms') END) AS ttft_samples,
            AVG(CASE WHEN state='completed' THEN json_extract(facts,'$.tokens_per_second') END) AS tokens_per_second,
            COUNT(CASE WHEN state='completed' THEN json_extract(facts,'$.tokens_per_second') END) AS tps_samples
            FROM {calls} WHERE {predicate}"), &args)?;
        // Aggregate the full filtered range, independently of record pagination.
        let failures = rows(&connection, &format!("SELECT
            COALESCE(json_extract(facts,'$.error_origin'),json_extract(delivery,'$.error_origin'),'unknown') AS origin,
            COALESCE(json_extract(facts,'$.error_code'),json_extract(delivery,'$.error_code'),json_extract(facts,'$.failure_reason'),json_extract(facts,'$.error_class'),'unknown') AS code,
            json_extract(facts,'$.status') AS status, COUNT(*) AS count
            FROM {calls} WHERE {predicate} AND state='failed'
            GROUP BY origin,code,status ORDER BY count DESC,origin,code,status LIMIT 12"), &args)?;
        if let Some(summary) = summary.first_mut() {
            summary["failure_breakdown"] = json!(failures);
        }
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
            FROM {calls} WHERE {predicate}
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
            FROM {calls} WHERE {predicate} GROUP BY CAST((started_at-{})/{bucket} AS INTEGER) ORDER BY start",filter.start,filter.start,filter.start), &args)?;
        args.extend([
            SqlValue::Integer(filter.limit as i64),
            SqlValue::Integer(filter.offset.min(i64::MAX as usize) as i64),
        ]);
        let mut statement = connection.prepare(&format!("SELECT facts,delivery,
            (SELECT json_object('cost_nanos',cost_nanos,'price_issue',price_issue,'price_key',price_key)
             FROM accounted_usage WHERE id=json_extract(call_records.facts,'$.usage_event_id') LIMIT 1), state
            FROM {calls} WHERE {predicate}
            ORDER BY started_at DESC,request_id DESC,operation LIMIT ? OFFSET ?"))?;
        let records = statement
            .query_map(params_from_iter(&args), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (raw, delivery, cost, state) = row?;
                let mut value: Value = serde_json::from_str(&raw).map_err(|_| UsageError)?;
                value["state"] = json!(state);
                if let Some(delivery) = delivery {
                    value["delivery"] =
                        serde_json::from_str::<Value>(&delivery).map_err(|_| UsageError)?;
                    if matches!(state.as_str(), "failed" | "recovery_required") {
                        for key in ["error_origin", "error_code"] {
                            if value[key].is_null() {
                                value[key] = value["delivery"][key].clone();
                            }
                        }
                    }
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
