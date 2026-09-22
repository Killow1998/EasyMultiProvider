//! Bounded SQLite history for browser-safe Codex quota projections.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

pub const SAMPLE_INTERVAL_SECONDS: i64 = 5 * 60;
pub const RETENTION_SECONDS: i64 = 15 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaHistoryError {
    message: &'static str,
}

impl QuotaHistoryError {
    fn unavailable() -> Self {
        Self {
            message: "quota history is unavailable",
        }
    }

    fn symlink() -> Self {
        Self {
            message: "quota history path must not be a symlink",
        }
    }

    fn account_required() -> Self {
        Self {
            message: "quota history account is required",
        }
    }

    fn unsupported_range() -> Self {
        Self {
            message: "unsupported quota history range",
        }
    }
}

impl fmt::Display for QuotaHistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for QuotaHistoryError {}

#[derive(Debug)]
pub struct QuotaHistoryStore {
    path: PathBuf,
    lock: Mutex<()>,
}

#[derive(Debug)]
struct SnapshotRow {
    limit_id: String,
    window_kind: &'static str,
    window_minutes: Option<i64>,
    used_percent: f64,
    resets_at: Option<i64>,
}

impl QuotaHistoryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_snapshot(
        &self,
        account_key: &str,
        snapshot: &Value,
        observed_at: i64,
    ) -> Result<usize, QuotaHistoryError> {
        if account_key.is_empty() {
            return Err(QuotaHistoryError::account_required());
        }
        let rows = snapshot_rows(snapshot);
        if rows.is_empty() {
            return Ok(0);
        }
        let plan_type = snapshot
            .as_object()
            .and_then(|snapshot| {
                snapshot
                    .get("plan_type")
                    .or_else(|| snapshot.get("planType"))
            })
            .and_then(Value::as_str)
            .and_then(normalize_plan_type);
        let timestamp = observed_at - observed_at.rem_euclid(SAMPLE_INTERVAL_SECONDS);
        let cutoff = timestamp.saturating_sub(RETENTION_SECONDS);
        let _guard = self
            .lock
            .lock()
            .map_err(|_| QuotaHistoryError::unavailable())?;
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|_| QuotaHistoryError::unavailable())?;
        for row in &rows {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO quota_samples \
                     (account_key, observed_at, limit_id, window_kind, window_minutes, \
                      used_percent, resets_at, plan_type) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        account_key,
                        timestamp,
                        row.limit_id,
                        row.window_kind,
                        row.window_minutes,
                        row.used_percent,
                        row.resets_at,
                        plan_type,
                    ],
                )
                .map_err(|_| QuotaHistoryError::unavailable())?;
        }
        transaction
            .execute(
                "DELETE FROM quota_samples WHERE observed_at < ?1",
                params![cutoff],
            )
            .map_err(|_| QuotaHistoryError::unavailable())?;
        transaction
            .commit()
            .map_err(|_| QuotaHistoryError::unavailable())?;
        Ok(rows.len())
    }

    pub fn query(
        &self,
        account_key: &str,
        range_name: &str,
        now: i64,
    ) -> Result<Value, QuotaHistoryError> {
        let range_seconds =
            range_seconds(range_name).ok_or_else(QuotaHistoryError::unsupported_range)?;
        let cutoff = now.saturating_sub(range_seconds);
        let mut series = Vec::<Value>::new();
        let mut plans = BTreeMap::<i64, String>::new();
        if self.path.exists() {
            let _guard = self
                .lock
                .lock()
                .map_err(|_| QuotaHistoryError::unavailable())?;
            reject_symlink(&self.path)?;
            let connection = self.connect()?;
            let mut statement = connection
                .prepare(
                    "SELECT observed_at, limit_id, window_kind, window_minutes, \
                     used_percent, resets_at, plan_type FROM quota_samples \
                     WHERE account_key = ?1 AND observed_at >= ?2 AND observed_at <= ?3 \
                     ORDER BY observed_at, limit_id, window_kind",
                )
                .map_err(|_| QuotaHistoryError::unavailable())?;
            let mut rows = statement
                .query(params![account_key, cutoff, now])
                .map_err(|_| QuotaHistoryError::unavailable())?;
            while let Some(row) = rows.next().map_err(|_| QuotaHistoryError::unavailable())? {
                let observed_at: i64 = row.get(0).map_err(|_| QuotaHistoryError::unavailable())?;
                let limit_id: String = row.get(1).map_err(|_| QuotaHistoryError::unavailable())?;
                let kind: String = row.get(2).map_err(|_| QuotaHistoryError::unavailable())?;
                let minutes: Option<i64> =
                    row.get(3).map_err(|_| QuotaHistoryError::unavailable())?;
                let used: f64 = row.get(4).map_err(|_| QuotaHistoryError::unavailable())?;
                let resets_at: Option<i64> =
                    row.get(5).map_err(|_| QuotaHistoryError::unavailable())?;
                let plan_type: Option<String> =
                    row.get(6).map_err(|_| QuotaHistoryError::unavailable())?;
                if let Some(plan) = plan_type.as_deref().and_then(normalize_plan_type) {
                    plans.insert(observed_at, plan);
                }
                let position = series.iter().position(|item| {
                    item.get("limit_id").and_then(Value::as_str) == Some(limit_id.as_str())
                        && match minutes {
                            Some(minutes) => {
                                item.get("window_minutes").and_then(Value::as_i64) == Some(minutes)
                            }
                            None => {
                                item.get("window_minutes").is_some_and(Value::is_null)
                                    && item.get("window_kind").and_then(Value::as_str)
                                        == Some(kind.as_str())
                            }
                        }
                });
                let position = position.unwrap_or_else(|| {
                    series.push(json!({
                        "limit_id": limit_id,
                        "window_kind": kind,
                        "window_minutes": minutes,
                        "points": [],
                    }));
                    series.len() - 1
                });
                let remaining = round_two(100.0 - used.clamp(0.0, 100.0));
                series[position]
                    .get_mut("points")
                    .and_then(Value::as_array_mut)
                    .expect("new quota series always contains points")
                    .push(json!({
                        "observed_at": observed_at,
                        "remaining_percent": remaining,
                        "resets_at": resets_at,
                    }));
            }
        }
        Ok(json!({
            "range": range_name,
            "start_at": cutoff,
            "end_at": now,
            "sample_interval_seconds": SAMPLE_INTERVAL_SECONDS,
            "retention_days": RETENTION_SECONDS / (24 * 60 * 60),
            "series": series,
            "plans": plans
                .into_iter()
                .map(|(observed_at, plan_type)| json!({
                    "observed_at": observed_at,
                    "plan_type": plan_type,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    pub fn delete_account(&self, account_key: &str) -> Result<(), QuotaHistoryError> {
        if !self.path.exists() {
            return Ok(());
        }
        let _guard = self
            .lock
            .lock()
            .map_err(|_| QuotaHistoryError::unavailable())?;
        reject_symlink(&self.path)?;
        let connection =
            Connection::open(&self.path).map_err(|_| QuotaHistoryError::unavailable())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| QuotaHistoryError::unavailable())?;
        connection
            .execute(
                "DELETE FROM quota_samples WHERE account_key = ?1",
                params![account_key],
            )
            .map_err(|_| QuotaHistoryError::unavailable())?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection, QuotaHistoryError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        create_private_directory(parent)?;
        reject_symlink(&self.path)?;
        let connection =
            Connection::open(&self.path).map_err(|_| QuotaHistoryError::unavailable())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| QuotaHistoryError::unavailable())?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS quota_samples (
                    account_key TEXT NOT NULL,
                    observed_at INTEGER NOT NULL,
                    limit_id TEXT NOT NULL,
                    window_kind TEXT NOT NULL,
                    window_minutes INTEGER,
                    used_percent REAL NOT NULL,
                    resets_at INTEGER,
                    plan_type TEXT,
                    PRIMARY KEY (account_key, observed_at, limit_id, window_kind)
                );",
            )
            .map_err(|_| QuotaHistoryError::unavailable())?;
        let plan_type_exists = connection
            .query_row(
                "SELECT 1 FROM pragma_table_info('quota_samples') WHERE name = 'plan_type'",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(|_| QuotaHistoryError::unavailable())?
            .is_some();
        if !plan_type_exists {
            connection
                .execute("ALTER TABLE quota_samples ADD COLUMN plan_type TEXT", [])
                .map_err(|_| QuotaHistoryError::unavailable())?;
        }
        connection
            .execute(
                "CREATE INDEX IF NOT EXISTS quota_samples_lookup \
                 ON quota_samples(account_key, observed_at)",
                [],
            )
            .map_err(|_| QuotaHistoryError::unavailable())?;
        set_private_file_permissions(&self.path);
        Ok(connection)
    }
}

fn range_seconds(range_name: &str) -> Option<i64> {
    match range_name {
        "1h" => Some(60 * 60),
        "1d" => Some(24 * 60 * 60),
        "1w" => Some(7 * 24 * 60 * 60),
        "all" => Some(RETENTION_SECONDS),
        _ => None,
    }
}

fn snapshot_rows(snapshot: &Value) -> Vec<SnapshotRow> {
    let Some(snapshot) = snapshot.as_object() else {
        return Vec::new();
    };
    rate_limit_buckets(snapshot)
        .into_iter()
        .flat_map(|(limit_id, bucket)| {
            ["primary", "secondary"]
                .into_iter()
                .filter_map(move |kind| snapshot_row(limit_id.clone(), kind, bucket.get(kind)?))
        })
        .collect()
}

fn rate_limit_buckets(snapshot: &Map<String, Value>) -> Vec<(String, &Map<String, Value>)> {
    if let Some(by_limit) = snapshot
        .get("rate_limits_by_limit_id")
        .or_else(|| snapshot.get("rateLimitsByLimitId"))
        .and_then(Value::as_object)
    {
        let valid = by_limit
            .iter()
            .filter_map(|(limit_id, bucket)| {
                let bucket = bucket.as_object()?;
                (!limit_id.is_empty()).then(|| (limit_id.clone(), bucket))
            })
            .collect::<Vec<_>>();
        if !valid.is_empty() {
            return valid;
        }
    }
    let Some(bucket) = snapshot.get("rate_limits").and_then(Value::as_object) else {
        return Vec::new();
    };
    let limit_id = bucket
        .get("limitId")
        .or_else(|| bucket.get("limit_id"))
        .and_then(nonempty_string)
        .unwrap_or("codex")
        .to_owned();
    vec![(limit_id, bucket)]
}

fn snapshot_row(limit_id: String, window_kind: &'static str, value: &Value) -> Option<SnapshotRow> {
    let window = value.as_object()?;
    let used_percent = finite_number(
        window
            .get("usedPercent")
            .or_else(|| window.get("used_percent"))?,
    )?;
    Some(SnapshotRow {
        limit_id,
        window_kind,
        window_minutes: integer(
            window
                .get("windowDurationMins")
                .or_else(|| window.get("window_duration_mins"))
                .or_else(|| window.get("window_minutes")),
        ),
        used_percent: used_percent.clamp(0.0, 100.0),
        resets_at: integer(window.get("resetsAt").or_else(|| window.get("resets_at"))),
    })
}

fn finite_number(value: &Value) -> Option<f64> {
    if value.is_boolean() {
        return None;
    }
    value.as_f64().filter(|value| value.is_finite())
}

fn integer(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if value.is_boolean() {
        return None;
    }
    if let Some(value) = value.as_i64() {
        return Some(value);
    }
    if let Some(value) = value.as_u64() {
        return i64::try_from(value).ok();
    }
    let value = finite_number(value)?.trunc();
    (value >= i64::MIN as f64 && value <= i64::MAX as f64).then_some(value as i64)
}

fn nonempty_string(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn normalize_plan_type(value: &str) -> Option<String> {
    let collapsed = value
        .trim()
        .to_lowercase()
        .replace(['-', ' '], "_")
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if collapsed.is_empty()
        || collapsed.chars().count() > 32
        || !collapsed
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
    {
        return None;
    }
    Some(match collapsed.as_str() {
        "prolite" | "lite_pro" => "pro_lite".to_owned(),
        _ => collapsed,
    })
}

fn round_two(value: f64) -> f64 {
    format!("{value:.2}").parse().unwrap_or(value)
}

fn reject_symlink(path: &Path) -> Result<(), QuotaHistoryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(QuotaHistoryError::symlink()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(QuotaHistoryError::unavailable()),
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<(), QuotaHistoryError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let existed = path.exists();
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .map_err(|_| QuotaHistoryError::unavailable())?;
    if !existed && let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        let _ = fs::set_permissions(path, permissions);
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<(), QuotaHistoryError> {
    fs::create_dir_all(path).map_err(|_| QuotaHistoryError::unavailable())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) {}
