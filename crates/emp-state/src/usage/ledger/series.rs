//! Chart groups and totals share the reconciled accounting selection.
use super::*;
use std::collections::BTreeMap;

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

impl UsageLedger {
    pub fn query_series(&self, filter: &UsageSeriesFilter, now: f64) -> Result<Value> {
        if !Self::valid_period(filter.start, filter.end, &filter.category) {
            return Err(UsageError);
        }
        let bucket = ((filter.end - filter.start) / 48.0).max(60.0);
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
        let mut receipt = Vec::new();
        if let Some(session) = &filter.session {
            receipt.push("(session_id = ? OR json_extract(facts,'$.thread_id') = ?)");
            args.extend([
                SqlValue::Text(session.clone()),
                SqlValue::Text(session.clone()),
            ]);
        }
        if let Some(state) = &filter.state {
            receipt.push("state = ?");
            args.push(SqlValue::Text(state.clone()));
        }
        let mut selection = conditions.join(" AND ");
        if filter.session.is_some() || filter.state.is_some() {
            selection.push_str(&format!(
                " AND u.id IN (SELECT json_extract(facts,'$.usage_event_id') FROM {} WHERE {})",
                crate::usage::call_outcome::REPORT_CALLS,
                receipt.join(" AND ")
            ));
        }
        let sums = TOKEN_FIELDS
            .iter()
            .map(|field| format!("SUM(COALESCE(u.{field},0)) AS {field}"))
            .collect::<Vec<_>>()
            .join(",");
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        let mut query_args = vec![SqlValue::Real(filter.start), SqlValue::Real(bucket)];
        query_args.extend(args);
        let series = rows(
            &connection,
            &format!(
                "SELECT CAST((u.observed_at-?)/? AS INTEGER) AS bucket,
             u.category,u.owner,u.model,{sums},COUNT(*) AS requests,
             SUM(u.cost_nanos IS NOT NULL) AS priced_requests,
             SUM(COALESCE(u.cost_nanos,0)) AS cost_nanos
             FROM accounted_usage u WHERE {selection}
             GROUP BY bucket,u.category,u.owner,u.model ORDER BY bucket,u.category,u.owner,u.model"
            ),
            &query_args,
        )?;
        let fields = TOKEN_FIELDS
            .into_iter()
            .chain(["requests", "priced_requests", "cost_nanos"])
            .collect::<Vec<_>>();
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
        Ok(
            json!({"start":filter.start,"end":filter.end,"bucket":bucket,"totals":totals,
            "groups":groups.into_values().collect::<Vec<_>>(),
            "periods":periods.into_values().collect::<Vec<_>>(),"series":series,
            "pricing":self.prices.snapshot(now),"currency":"USD"}),
        )
    }
}
