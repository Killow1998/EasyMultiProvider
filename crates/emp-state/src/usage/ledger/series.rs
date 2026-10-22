//! Chart groups and totals share the reconciled accounting selection.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct UsageSeriesFilter {
    pub start: f64,
    pub end: f64,
    pub category: String,
    pub owner: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub state: Option<String>,
}

const COUNTERS: [&str; 10] = [
    "requests",
    "reported_requests",
    "priced_requests",
    "unpriced_requests",
    "cost_nanos",
    "input_reports",
    "output_reports",
    "cache_reports",
    "cache_write_reports",
    "reasoning_reports",
];
pub(super) fn sum_fields() -> String {
    TOKEN_FIELDS
        .into_iter()
        .chain(COUNTERS)
        .map(|field| format!("COALESCE(SUM(u.{field}),0) AS {field}"))
        .collect::<Vec<_>>()
        .join(",")
}
pub(super) fn aggregate() -> String {
    TOKEN_FIELDS.iter().map(|field| format!("SUM(COALESCE(u.{field},0)) AS {field}")).collect::<Vec<_>>().join(",")
        + ",COUNT(*) AS requests,SUM(u.input_tokens IS NOT NULL AND u.output_tokens IS NOT NULL) AS reported_requests,
           SUM(u.input_tokens IS NOT NULL) AS input_reports,SUM(u.output_tokens IS NOT NULL) AS output_reports,
           SUM(u.cached_input_tokens IS NOT NULL) AS cache_reports,SUM(u.cache_write_tokens IS NOT NULL) AS cache_write_reports,
           SUM(u.reasoning_tokens IS NOT NULL) AS reasoning_reports,
           SUM(u.cost_nanos IS NOT NULL) AS priced_requests,
           SUM(u.cost_nanos IS NULL OR u.price_issue IS NOT NULL) AS unpriced_requests,
           SUM(COALESCE(u.cost_nanos,0)) AS cost_nanos"
}
pub(super) fn selection(
    filter: &UsageSeriesFilter,
    conversation: bool,
) -> Result<(String, Vec<SqlValue>)> {
    if !UsageLedger::valid_period(filter.start, filter.end, &filter.category) {
        return Err(UsageError);
    }
    let mut conditions = vec!["u.observed_at >= ?", "u.observed_at < ?"];
    let mut args = vec![SqlValue::Real(filter.start), SqlValue::Real(filter.end)];
    for (condition, value) in [
        (
            "u.category = ?",
            (filter.category != "all").then_some(&filter.category),
        ),
        ("u.owner = ?", filter.owner.as_ref()),
        (
            "u.category = 'external' AND u.owner = ?",
            filter.provider.as_ref(),
        ),
    ] {
        if let Some(value) = value {
            conditions.push(condition);
            args.push(SqlValue::Text(value.clone()));
        }
    }
    if let Some(model) = &filter.model {
        conditions.push("(u.model = ? OR u.route_model = ?)");
        args.extend([SqlValue::Text(model.clone()), SqlValue::Text(model.clone())]);
    }
    if let Some(session) = &filter.session {
        conditions.push(if conversation {
            "u.conversation_id = ?"
        } else {
            "u.session_id = ?"
        });
        args.push(SqlValue::Text(session.clone()));
    }
    let mut selection = conditions.join(" AND ");
    if let Some(state) = &filter.state {
        selection.push_str(&format!(
            " AND u.id IN (SELECT json_extract(facts,'$.usage_event_id') FROM {} WHERE state=?)",
            crate::usage::call_outcome::REPORT_CALLS
        ));
        args.push(SqlValue::Text(state.clone()));
    }
    Ok((selection, args))
}
impl UsageLedger {
    pub fn query_series(
        &self,
        filter: &UsageSeriesFilter,
        scope: Option<&SessionScope>,
        now: f64,
    ) -> Result<Value> {
        let (selection, args) = selection(filter, scope.is_some())?;
        let bucket = ((filter.end - filter.start) / 48.0).max(60.0);
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        let source = sessions::scoped_source(&connection, scope)?;
        let mut query_args = vec![SqlValue::Real(filter.start), SqlValue::Real(bucket)];
        query_args.extend(args.iter().cloned());
        let series = rows(
            &connection,
            &format!(
                "SELECT CAST((u.observed_at-?)/? AS INTEGER) AS bucket,
             u.category,u.owner,u.model,{}
             FROM {source} u WHERE {selection}
             GROUP BY bucket,u.category,u.owner,u.model ORDER BY bucket,u.category,u.owner,u.model",
                aggregate()
            ),
            &query_args,
        )?;
        let fields = TOKEN_FIELDS.into_iter().chain(COUNTERS).collect::<Vec<_>>();
        let mut totals = json!({});
        for field in &fields {
            totals[field] = json!(0);
        }
        let mut groups = BTreeMap::new();
        let mut periods = BTreeMap::new();
        let mut series = series;
        for row in &mut series {
            let index = row["bucket"].as_i64().ok_or(UsageError)?;
            row["start"] = json!(filter.start + index as f64 * bucket);
            row.as_object_mut().ok_or(UsageError)?.remove("bucket");
            let key = (
                row["category"].as_str().unwrap_or("").to_owned(),
                row["owner"].as_str().unwrap_or("").to_owned(),
                row["model"].as_str().unwrap_or("").to_owned(),
            );
            let group = groups.entry(key).or_insert_with(|| {
                json!({
                    "category":row["category"],"owner":row["owner"],"model":row["model"]
                })
            });
            let period = periods
                .entry(index)
                .or_insert_with(|| json!({"start":row["start"]}));
            for field in &fields {
                let value = row[field].as_i64().unwrap_or(0);
                for aggregate in [&mut *group, &mut *period, &mut totals] {
                    aggregate[field] = json!(
                        aggregate[field]
                            .as_i64()
                            .unwrap_or(0)
                            .checked_add(value)
                            .ok_or(UsageError)?
                    );
                }
            }
        }
        let components = if scope.is_some() {
            rows(&connection, &format!(
                "SELECT u.session_id AS thread_id,u.session_name,u.session_kind,{}
                 FROM {source} u WHERE {selection} GROUP BY u.session_id,u.session_name,u.session_kind
                 ORDER BY (u.session_id=u.conversation_id) DESC,input_tokens+output_tokens DESC,thread_id", aggregate()), &args)?
        } else {
            Vec::new()
        };
        Ok(
            json!({"start":filter.start,"end":filter.end,"bucket":bucket,"totals":totals,
            "groups":groups.into_values().collect::<Vec<_>>(),"components":components,
            "periods":periods.into_values().collect::<Vec<_>>(),"series":series,
            "pricing":self.prices.snapshot(now),"currency":"USD"}),
        )
    }
}
