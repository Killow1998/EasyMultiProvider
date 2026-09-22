//! Python-compatible SQLite accounting and history/realtime reconciliation.
use super::pricing::PriceCatalog;
use super::{CATEGORIES, TOKEN_FIELDS, python_json, token_count};
use rusqlite::{
    Connection, params, params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Debug)]
pub struct UsageError;
impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Usage history is unavailable")
    }
}
impl std::error::Error for UsageError {}
impl From<rusqlite::Error> for UsageError {
    fn from(_: rusqlite::Error) -> Self {
        Self
    }
}
impl From<std::io::Error> for UsageError {
    fn from(_: std::io::Error) -> Self {
        Self
    }
}
type Result<T> = std::result::Result<T, UsageError>;
pub struct UsageLedger {
    path: PathBuf,
    pub prices: Arc<PriceCatalog>,
    lock: Mutex<()>,
    initialized: AtomicBool,
    write_error: AtomicBool,
}
const CREATE:&str="CREATE TABLE IF NOT EXISTS usage_events (
    id TEXT PRIMARY KEY, observed_at REAL NOT NULL, category TEXT NOT NULL, owner TEXT NOT NULL, model TEXT NOT NULL,
    service_tier TEXT NOT NULL, operation TEXT NOT NULL, input_tokens INTEGER, output_tokens INTEGER, cached_input_tokens INTEGER,
    cache_write_tokens INTEGER, cache_write_1h_tokens INTEGER, reasoning_tokens INTEGER, cost_nanos INTEGER, price_issue TEXT,
    price_key TEXT, price_fetched_at REAL, price_revision TEXT, rates TEXT NOT NULL);
    CREATE INDEX IF NOT EXISTS usage_period ON usage_events(observed_at);";
const INDEXES:&str="CREATE INDEX IF NOT EXISTS usage_match ON usage_events(match_key,origin);
    CREATE INDEX IF NOT EXISTS usage_turn ON usage_events(usage_turn,route_model,origin);
    CREATE TABLE IF NOT EXISTS usage_files(path TEXT PRIMARY KEY,checkpoint TEXT NOT NULL);
    DROP VIEW IF EXISTS accounted_usage;
    CREATE VIEW accounted_usage AS SELECT h.* FROM usage_events h WHERE h.origin='realtime' OR h.match_key=''
    OR NOT EXISTS(SELECT 1 FROM usage_events r WHERE r.origin='realtime' AND r.match_key=h.match_key)
    OR (SELECT COUNT(*) FROM usage_events r WHERE r.origin='realtime' AND r.match_key=h.match_key)
     < (SELECT COUNT(*) FROM usage_events p WHERE p.origin='history' AND p.match_key=h.match_key
        AND (p.observed_at<h.observed_at OR (p.observed_at=h.observed_at AND p.id<=h.id)));";
fn private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
fn sql(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Number(value) => value
            .as_i64()
            .map(SqlValue::Integer)
            .unwrap_or_else(|| SqlValue::Real(value.as_f64().unwrap_or(0.0))),
        Value::String(value) => SqlValue::Text(value.clone()),
        _ => SqlValue::Text(value.to_string()),
    }
}
fn text<'a>(event: &'a Value, key: &str, default: &'a str) -> &'a str {
    event[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
}
fn sha(value: &Value) -> String {
    format!("{:x}", Sha256::digest(python_json(value, true).as_bytes()))
}
impl UsageLedger {
    pub fn new(path: PathBuf, prices: Arc<PriceCatalog>) -> Self {
        Self {
            path,
            prices,
            lock: Mutex::new(()),
            initialized: AtomicBool::new(false),
            write_error: AtomicBool::new(false),
        }
    }
    fn connect(&self) -> Result<Connection> {
        let parent = self.path.parent().ok_or(UsageError)?;
        let existed = parent.exists();
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(not(unix))]
        let _ = existed;
        if self.path.is_symlink() {
            return Err(UsageError);
        }
        let mut connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        if self.initialized.load(Ordering::Acquire) {
            return Ok(connection);
        }
        connection.execute_batch(CREATE)?;
        let columns = rows(&connection, "PRAGMA table_info(usage_events)", &[])?;
        if !columns.iter().any(|row| row["name"] == "origin") {
            if connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM usage_events LIMIT 1)",
                [],
                |row| row.get::<_, bool>(0),
            )? {
                let backup = self.path.with_extension("pre-v011.sqlite3");
                if !backup.exists() {
                    connection.backup(rusqlite::MAIN_DB, &backup, None)?;
                    private(&backup)?;
                }
            }
            let tx = connection.transaction()?;
            for (name, default) in [
                ("origin", "realtime"),
                ("usage_turn", ""),
                ("route_model", ""),
                ("match_key", ""),
            ] {
                tx.execute_batch(&format!(
                    "ALTER TABLE usage_events ADD COLUMN {name} TEXT NOT NULL DEFAULT '{default}'"
                ))?;
            }
            tx.commit()?;
        }
        connection.execute_batch(INDEXES)?;
        private(&self.path)?;
        self.initialized.store(true, Ordering::Release);
        Ok(connection)
    }
    fn values(&self, event: &Value, observed: f64) -> Result<Vec<SqlValue>> {
        let response = event
            .get("usage_response_id")
            .filter(|value| value.as_str().is_some_and(|s| !s.is_empty()));
        let category = text(event, "usage_category", "");
        let owner = text(event, "usage_owner", "");
        let model = text(event, "upstream_model", "");
        let operation = text(event, "route", "");
        let id = if let Some(response) = response {
            sha(&json!([category, owner, model, operation, response]))
        } else if let Some(id) = event["observation_id"].as_str().filter(|s| !s.is_empty()) {
            id.to_owned()
        } else {
            let mut bytes = [0; 16];
            getrandom::getrandom(&mut bytes).map_err(|_| UsageError)?;
            bytes.iter().map(|byte| format!("{byte:02x}")).collect()
        };
        let mut quote = self.prices.quote(event).map_err(|_| UsageError)?;
        if event["usage_issue"].as_str().is_some_and(|s| !s.is_empty()) {
            quote["cost_nanos"] = Value::Null;
            quote["price_issue"] = event["usage_issue"].clone();
            quote["rates"] = json!({});
        }
        let mut values = vec![
            SqlValue::Text(id),
            SqlValue::Real(observed),
            SqlValue::Text(category.into()),
            SqlValue::Text(owner.into()),
            SqlValue::Text(model.into()),
            SqlValue::Text(text(event, "service_tier", "default").into()),
            SqlValue::Text(operation.into()),
        ];
        for field in TOKEN_FIELDS {
            values.push(
                token_count(event.get(field))
                    .map_or(SqlValue::Null, |value| SqlValue::Integer(value as i64)),
            );
        }
        for field in [
            "cost_nanos",
            "price_issue",
            "price_key",
            "price_fetched_at",
            "price_revision",
        ] {
            values.push(sql(&quote[field]));
        }
        values.push(SqlValue::Text(python_json(&quote["rates"], true)));
        let turn = text(event, "usage_turn", "");
        let route_model = text(event, "route_model", "");
        let counts = ["input_tokens", "output_tokens", "cached_input_tokens"]
            .map(|key| token_count(event.get(key)));
        let match_key =
            if !turn.is_empty() && !route_model.is_empty() && counts.iter().all(Option::is_some) {
                sha(&json!([turn, route_model, counts]))
            } else {
                String::new()
            };
        values.extend([
            SqlValue::Text(text(event, "origin", "realtime").into()),
            SqlValue::Text(turn.into()),
            SqlValue::Text(route_model.into()),
            SqlValue::Text(match_key),
        ]);
        Ok(values)
    }
    pub fn record(&self, event: &Value, observed: f64) {
        if !matches!(event["route"].as_str(), Some("responses" | "compact"))
            || !event["usage_category"]
                .as_str()
                .is_some_and(|value| CATEGORIES.contains(&value))
            || event["recovery_mode"] == "native_http_fallback"
        {
            return;
        }
        let result = (|| -> Result<()> {
            let values = self.values(event, observed)?;
            let _guard = self.lock.lock().map_err(|_| UsageError)?;
            insert(&self.connect()?, &values)
        })();
        if result.is_err() {
            self.write_error.store(true, Ordering::Release);
        }
    }
    pub fn history_checkpoint(&self, path: &str) -> Result<Option<Value>> {
        use rusqlite::OptionalExtension;
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let raw: Option<String> = self
            .connect()?
            .query_row(
                "SELECT checkpoint FROM usage_files WHERE path=?",
                [path],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(|_| UsageError))
            .transpose()
    }
    pub fn record_history(
        &self,
        events: &[(Value, f64)],
        path: &str,
        checkpoint: &Value,
    ) -> Result<()> {
        let values = events
            .iter()
            .map(|(event, stamp)| self.values(event, *stamp))
            .collect::<Result<Vec<_>>>()?;
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        for values in values {
            insert(&transaction, &values)?;
        }
        transaction.execute(
            "INSERT OR REPLACE INTO usage_files VALUES (?,?)",
            params![path, python_json(checkpoint, true)],
        )?;
        transaction.commit()?;
        Ok(())
    }
    pub fn valid_period(start: f64, end: f64, category: &str) -> bool {
        start.is_finite()
            && end.is_finite()
            && start >= 0.0
            && start < end
            && (category == "all" || CATEGORIES.contains(&category))
    }
    pub fn query(&self, start: f64, end: f64, category: &str, now: f64) -> Result<Value> {
        if !Self::valid_period(start, end, category) {
            return Err(UsageError);
        }
        let mut selection = "observed_at >= ? AND observed_at < ?".to_owned();
        let mut parameters = vec![SqlValue::Real(start), SqlValue::Real(end)];
        if category != "all" {
            selection.push_str(" AND category = ?");
            parameters.push(SqlValue::Text(category.into()));
        }
        let aggregate = TOKEN_FIELDS
            .iter()
            .map(|field| format!("SUM(COALESCE({field},0)) AS {field}"))
            .collect::<Vec<_>>()
            .join(",")
            + ", COUNT(*) AS requests, SUM(input_tokens IS NOT NULL AND output_tokens IS NOT NULL) AS reported_requests, SUM(input_tokens IS NOT NULL) AS input_reports, SUM(output_tokens IS NOT NULL) AS output_reports, SUM(cached_input_tokens IS NOT NULL) AS cache_reports, SUM(reasoning_tokens IS NOT NULL) AS reasoning_reports, SUM(cost_nanos IS NOT NULL) AS priced_requests, SUM(COALESCE(cost_nanos,0)) AS cost_nanos";
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        connection.execute_batch("PRAGMA temp_store=MEMORY")?;
        connection.execute(&format!("CREATE TEMP TABLE selected_usage AS SELECT observed_at,category,owner,model,service_tier,origin,usage_turn,route_model,price_issue,cost_nanos,{} FROM accounted_usage WHERE {selection}",TOKEN_FIELDS.join(",")),params_from_iter(&parameters))?;
        let grouped = |group: &str| {
            rows(
                &connection,
                &format!(
                    "SELECT {group},{aggregate} FROM selected_usage WHERE {selection} GROUP BY {group}"
                ),
                &parameters,
            )
        };
        let groups = grouped("category,owner,model,service_tier")?;
        let sources = grouped("origin")?;
        let periods = rows(
            &connection,
            &format!(
                "SELECT CAST(observed_at/60 AS INTEGER)*60 AS start,SUM(COALESCE(input_tokens,0)) AS input_tokens,SUM(COALESCE(output_tokens,0)) AS output_tokens,SUM(COALESCE(cost_nanos,0)) AS cost_nanos,COUNT(*) AS requests,SUM(cost_nanos IS NOT NULL) AS priced_requests FROM selected_usage WHERE {selection} GROUP BY start ORDER BY start"
            ),
            &parameters,
        )?;
        let issues = rows(
            &connection,
            &format!(
                "SELECT price_issue,COUNT(*) AS requests FROM selected_usage WHERE {selection} AND price_issue IS NOT NULL GROUP BY price_issue"
            ),
            &parameters,
        )?;
        let first: Option<f64> =
            connection.query_row("SELECT MIN(observed_at) FROM usage_events", [], |row| {
                row.get(0)
            })?;
        let uncorrelated: i64 = connection.query_row(
            "SELECT COUNT(*) FROM selected_usage WHERE origin='realtime' AND usage_turn=''",
            [],
            |row| row.get(0),
        )?;
        let unmatched:i64=connection.query_row("SELECT COUNT(*) FROM selected_usage h WHERE h.origin='history' AND h.observed_at>=? AND h.observed_at<? AND h.usage_turn!='' AND EXISTS(SELECT 1 FROM usage_events r WHERE r.origin='realtime' AND r.usage_turn=h.usage_turn AND r.route_model=h.route_model)",params![start,end],|row|row.get(0))?;
        let mut totals = Map::new();
        for field in TOKEN_FIELDS.into_iter().chain([
            "requests",
            "reported_requests",
            "priced_requests",
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
            json!({"start":start,"end":end,"category":category,"totals":totals,"groups":groups,"periods":periods,"issues":issues,"first_record_at":first,"sources":sources,"unmatched_overlap":unmatched,"uncorrelated_realtime":uncorrelated,"write_error":self.write_error.load(Ordering::Acquire),"pricing":self.prices.snapshot(now),"currency":"USD"}),
        )
    }
    pub fn price_pending(&self, stop: &AtomicBool) {
        if self.price_pending_inner(stop).is_err() {
            self.write_error.store(true, Ordering::Release);
        }
    }
    fn price_pending_inner(&self, stop: &AtomicBool) -> Result<()> {
        while !stop.load(Ordering::Acquire) {
            let _guard = self.lock.lock().map_err(|_| UsageError)?;
            let mut connection = self.connect()?;
            let pending = rows(
                &connection,
                "SELECT * FROM usage_events WHERE cost_nanos IS NULL AND price_issue IN ('unknown_model','missing_rate','unknown_tier') AND input_tokens IS NOT NULL AND output_tokens IS NOT NULL AND price_revision != ? LIMIT 200",
                &[SqlValue::Text(self.prices.revision())],
            )?;
            if pending.is_empty() {
                return Ok(());
            }
            let tx = connection.transaction()?;
            for mut event in pending {
                event["upstream_model"] = event["model"].clone();
                for field in [
                    "cache_write_tokens",
                    "cache_write_1h_tokens",
                    "reasoning_tokens",
                ] {
                    if event[field].is_null() {
                        event.as_object_mut().unwrap().remove(field);
                    }
                }
                let quote = self.prices.quote(&event).map_err(|_| UsageError)?;
                let mut values = [
                    "cost_nanos",
                    "price_issue",
                    "price_key",
                    "price_fetched_at",
                    "price_revision",
                ]
                .map(|key| sql(&quote[key]))
                .to_vec();
                values.push(SqlValue::Text(python_json(&quote["rates"], true)));
                values.push(sql(&event["id"]));
                tx.execute("UPDATE usage_events SET cost_nanos=?,price_issue=?,price_key=?,price_fetched_at=?,price_revision=?,rates=? WHERE id=? AND cost_nanos IS NULL",params_from_iter(values))?;
            }
            tx.commit()?;
        }
        Ok(())
    }
}
fn insert(connection: &Connection, values: &[SqlValue]) -> Result<()> {
    connection.execute(
        &format!(
            "INSERT OR IGNORE INTO usage_events VALUES ({})",
            vec!["?"; values.len()].join(",")
        ),
        params_from_iter(values),
    )?;
    Ok(())
}
fn rows(connection: &Connection, query: &str, parameters: &[SqlValue]) -> Result<Vec<Value>> {
    let mut statement = connection.prepare(query)?;
    let names = statement
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    let rows = statement.query_map(params_from_iter(parameters), |row| {
        let mut value = Map::new();
        for (index, name) in names.iter().enumerate() {
            let item = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Real(value) => json!(value),
                ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
                ValueRef::Blob(_) => Value::Null,
            };
            value.insert(name.clone(), item);
        }
        Ok(Value::Object(value))
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}
