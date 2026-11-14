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
mod affinity;
mod calls;
pub use calls::CallFilter;
mod identities;
mod query;
mod read_cache;
mod series;
pub use series::UsageSeriesFilter;
mod sessions;
pub use sessions::SessionBillPage;
mod session_scope;
pub use session_scope::{SessionMetadata, SessionScope};

pub const RESOLVED_USAGE: &str = include_str!("../../../../contracts/usage-owners.sql");

type Result<T> = std::result::Result<T, UsageError>;

fn chart_bucket(span: f64) -> f64 {
    if span <= 3600.0 {
        600.0
    } else if span <= 86400.0 {
        3600.0
    } else {
        86400.0
    }
}

pub struct UsageLedger {
    path: PathBuf,
    pub prices: Arc<PriceCatalog>,
    lock: Mutex<read_cache::ReadCache>,
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
    #[cfg(not(unix))]
    let _ = path;
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
            lock: Mutex::new(read_cache::ReadCache::default()),
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
        calls::initialize(&connection)?;
        identities::initialize(&connection)?;
        sessions::initialize(&connection)?;
        calls::restore_memory_usage(self, &mut connection)?;
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
        } else if let Some(request) = event["request_id"].as_str().filter(|s| !s.is_empty()) {
            sha(&json!([category, owner, model, operation, request]))
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
        let operation = event["route"].as_str().unwrap_or("");
        if !(matches!(
            operation,
            "responses"
                | "compact"
                | "image_generation"
                | "image_edit"
                | "agent_task_recovery"
                | "memory_extraction"
                | "memory_consolidation"
                | "decisions"
        ) || crate::diagnostics::MODEL_TEST_OPERATIONS.contains(&operation))
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
            let mut connection = self.connect()?;
            let transaction = connection.transaction()?;
            insert(&transaction, &values, event)?;
            transaction.commit()?;
            Ok(())
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
        for (values, (event, _)) in values.iter().zip(events) {
            insert(&transaction, values, event)?;
        }
        transaction.execute(
            "INSERT OR REPLACE INTO usage_files VALUES (?,?)",
            params![path, python_json(checkpoint, true)],
        )?;
        transaction.commit()?;
        Ok(())
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
                "SELECT * FROM usage_events WHERE price_issue IN ('unknown_model','missing_rate','unknown_tier') AND (cost_nanos IS NULL OR cost_nanos=0) AND input_tokens IS NOT NULL AND output_tokens IS NOT NULL AND price_revision != ? LIMIT 200",
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
                tx.execute("UPDATE usage_events SET cost_nanos=?,price_issue=?,price_key=?,price_fetched_at=?,price_revision=?,rates=? WHERE id=? AND (cost_nanos IS NULL OR price_issue IN ('unknown_model','missing_rate'))",params_from_iter(values))?;
            }
            tx.commit()?;
        }
        Ok(())
    }
}
fn insert(connection: &Connection, values: &[SqlValue], event: &Value) -> Result<()> {
    connection.execute(
        &format!(
            "INSERT OR IGNORE INTO usage_events VALUES ({})",
            vec!["?"; values.len()].join(",")
        ),
        params_from_iter(values),
    )?;
    if let Some(SqlValue::Text(id)) = values.first() {
        sessions::remember(
            connection,
            id,
            text(event, "thread_id", text(event, "session_id", "")),
        )?;
    }
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
