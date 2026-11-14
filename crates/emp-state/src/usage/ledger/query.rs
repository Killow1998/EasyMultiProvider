//! Scoped period and owner summaries; callers keep the public UsageLedger API.
use super::{RESOLVED_USAGE, Result, UsageError, UsageLedger, read_cache, rows};
use crate::usage::{CATEGORIES, TOKEN_FIELDS};
use rusqlite::{params, params_from_iter, types::Value as SqlValue};
use serde_json::{Map, Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

type UsageProgress = Box<dyn Fn(i64, i64) -> std::io::Result<()> + Send>;

impl UsageLedger {
    pub fn valid_period(start: f64, end: f64, category: &str) -> bool {
        start.is_finite()
            && end.is_finite()
            && start >= 0.0
            && start < end
            && (category == "all" || CATEGORIES.contains(&category))
    }
    pub fn query(
        &self,
        start: f64,
        end: f64,
        category: &str,
        now: f64,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<Value> {
        self.query_selected(start..end, category, now, None, None, cancelled)
    }
    pub fn query_owner(
        &self,
        start: f64,
        end: f64,
        owner: &str,
        now: f64,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<Value> {
        if owner.is_empty() {
            return Err(UsageError);
        }
        self.query_selected(start..end, "all", now, Some(owner), None, cancelled)
    }
    pub fn query_lifetime_with_progress(
        &self,
        owner: &str,
        now: f64,
        progress: impl Fn(i64, i64) -> std::io::Result<()> + Send + 'static,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<Value> {
        self.query_selected(
            0.0..now,
            "all",
            now,
            Some(owner),
            Some(Box::new(progress)),
            cancelled,
        )
    }
    fn query_selected(
        &self,
        period: std::ops::Range<f64>,
        category: &str,
        now: f64,
        owner: Option<&str>,
        progress: Option<UsageProgress>,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<Value> {
        let (start, end) = (period.start, period.end);
        if !Self::valid_period(start, end, category) {
            return Err(UsageError);
        }
        let mut selection = "observed_at >= ? AND observed_at < ?".to_owned();
        let mut parameters = vec![SqlValue::Real(start), SqlValue::Real(end)];
        if category != "all" {
            selection.push_str(" AND category = ?");
            parameters.push(SqlValue::Text(category.into()));
        }
        if let Some(owner) = owner {
            selection.push_str(" AND owner = ?");
            parameters.push(SqlValue::Text(owner.into()));
        }
        let mut aggregate = TOKEN_FIELDS
            .iter()
            .map(|field| format!("SUM(COALESCE({field},0)) AS {field}"))
            .collect::<Vec<_>>()
            .join(",")
            + ", COUNT(*) AS requests, SUM(input_tokens IS NOT NULL AND output_tokens IS NOT NULL) AS reported_requests, SUM(input_tokens IS NOT NULL) AS input_reports, SUM(output_tokens IS NOT NULL) AS output_reports, SUM(cached_input_tokens IS NOT NULL) AS cache_reports, SUM(reasoning_tokens IS NOT NULL) AS reasoning_reports, SUM(cost_nanos IS NOT NULL AND price_issue IS NULL) AS priced_requests, SUM(cost_nanos IS NULL OR price_issue IS NOT NULL) AS unpriced_requests, SUM(COALESCE(cost_nanos,0)) AS cost_nanos";
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        let connection = read_cache::QueryConnection::new(&connection, cancelled)?;
        connection.execute_batch("PRAGMA temp_store=FILE; PRAGMA temp.cache_size=-2048")?;
        connection.execute(&format!("CREATE TEMP TABLE selected_usage AS SELECT observed_at,category,owner,model,service_tier,origin,usage_turn,route_model,price_issue,cost_nanos,{} FROM {RESOLVED_USAGE} WHERE {selection}",TOKEN_FIELDS.join(",")),params_from_iter(&parameters))?;
        let lifetime = progress.is_some();
        if let Some(progress) = progress {
            let count: i64 =
                connection
                    .query_row("SELECT COUNT(*) FROM selected_usage", [], |row| row.get(0))?;
            progress(0, count)?;
            let completed = std::cell::Cell::new(0);
            let batch = (count / 100).max(1);
            connection.create_scalar_function(
                "usage_progress",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                move |_| {
                    let current = completed.get() + 1;
                    completed.set(current);
                    if current % batch == 0 || current == count {
                        progress(current, count)
                            .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
                    }
                    Ok(1_i64)
                },
            )?;
            aggregate.push_str(", COUNT(usage_progress()) AS read_progress");
        }
        let grouped = |group: &str| {
            rows(
                &connection,
                &format!("SELECT {group},{aggregate} FROM selected_usage GROUP BY {group}"),
                &[],
            )
        };
        let groups = grouped("category,owner,model,service_tier")?;
        if lifetime {
            let first: Option<f64> = connection.query_row(
                "SELECT MIN(observed_at) FROM usage_events WHERE owner = ?",
                params![owner],
                |row| row.get(0),
            )?;
            return Ok(json!({"groups":groups,"first_record_at":first}));
        }
        let sources = grouped("origin")?;
        let periods = rows(
            &connection,
            "SELECT CAST(observed_at/60 AS INTEGER)*60 AS start,SUM(COALESCE(input_tokens,0)) AS input_tokens,SUM(COALESCE(output_tokens,0)) AS output_tokens,SUM(COALESCE(cost_nanos,0)) AS cost_nanos,COUNT(*) AS requests,SUM(cost_nanos IS NOT NULL AND price_issue IS NULL) AS priced_requests, SUM(cost_nanos IS NULL OR price_issue IS NOT NULL) AS unpriced_requests FROM selected_usage GROUP BY start ORDER BY start",
            &[],
        )?;
        let issues = rows(
            &connection,
            "SELECT price_issue,COUNT(*) AS requests FROM selected_usage WHERE price_issue IS NOT NULL GROUP BY price_issue",
            &[],
        )?;
        let unpriced_models = rows(
            &connection,
            "SELECT route_model,COUNT(*) AS requests FROM selected_usage WHERE price_issue='unknown_model' GROUP BY route_model ORDER BY requests DESC LIMIT 50",
            &[],
        )?;
        let first: Option<f64> = match owner {
            Some(owner) => connection.query_row(
                "SELECT MIN(observed_at) FROM usage_events WHERE owner = ?",
                params![owner],
                |row| row.get(0),
            )?,
            None => {
                connection.query_row("SELECT MIN(observed_at) FROM usage_events", [], |row| {
                    row.get(0)
                })?
            }
        };
        let uncorrelated: i64 = connection.query_row(
            "SELECT COUNT(*) FROM selected_usage WHERE origin='realtime' AND usage_turn=''",
            [],
            |row| row.get(0),
        )?;
        let unmatched:i64=connection.query_row("SELECT COUNT(*) FROM selected_usage h WHERE h.origin='history' AND h.usage_turn!='' AND EXISTS(SELECT 1 FROM usage_events r WHERE r.origin='realtime' AND r.usage_turn=h.usage_turn AND r.route_model=h.route_model)",[],|row|row.get(0))?;
        let mut totals = Map::new();
        for field in TOKEN_FIELDS.into_iter().chain([
            "requests",
            "reported_requests",
            "priced_requests",
            "unpriced_requests",
            "cost_nanos",
            "input_reports",
            "output_reports",
        ]) {
            let total = groups
                .iter()
                .try_fold(0_i64, |sum, row| {
                    sum.checked_add(row[field].as_i64().unwrap_or(0))
                })
                .ok_or(UsageError)?;
            totals.insert(field.into(), json!(total));
        }
        Ok(
            json!({"start":start,"end":end,"category":category,"totals":totals,"groups":groups,"periods":periods,"issues":issues,"unpriced_models":unpriced_models,"first_record_at":first,"sources":sources,"unmatched_overlap":unmatched,"uncorrelated_realtime":uncorrelated,"write_error":self.write_error.load(Ordering::Acquire),"pricing":self.prices.snapshot(now),"currency":"USD"}),
        )
    }
}
