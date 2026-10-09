//! Subscription observations from Claude Code's events and usage control query.
//! No OAuth extraction or extra inference for quota checks. Only numeric window
//! fields leave this module; CLI identity is kept in memory to separate logins.
use emp_codex::quota_history::QuotaHistoryStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

pub(crate) struct LocalQuota {
    inner: Mutex<Observation>,
    history: QuotaHistoryStore,
    pub(super) refresh_lock: Mutex<()>,
}

#[derive(Default)]
struct Observation {
    owner: Option<String>,
    generation: u64,
    windows: serde_json::Map<String, Value>,
    observed_at: f64,
    refreshed_at: f64,
}

impl LocalQuota {
    pub(crate) fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            inner: Mutex::new(Observation::default()),
            history: QuotaHistoryStore::new(path),
            refresh_lock: Mutex::new(()),
        }
    }
    pub(super) fn identify(&self, identity: String) -> u64 {
        let mut observation = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if observation.owner.as_deref() != Some(&identity) {
            observation.windows.clear();
            observation.observed_at = 0.0;
            observation.refreshed_at = 0.0;
            observation.generation = observation.generation.wrapping_add(1);
            observation.owner = Some(identity);
        }
        observation.generation
    }
    pub(super) fn begin(&self, identity: Option<String>) -> u64 {
        let mut observation = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if identity.is_none() || identity != observation.owner {
            observation.windows.clear();
            observation.observed_at = 0.0;
            observation.refreshed_at = 0.0;
        }
        observation.owner = identity;
        observation.generation = observation.generation.wrapping_add(1);
        observation.generation
    }

    pub(super) fn record(&self, generation: u64, stdout: &[u8], now: f64) -> bool {
        let mut windows = serde_json::Map::new();
        for line in stdout.split(|byte| *byte == b'\n') {
            let Ok(event) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if event.get("type").and_then(Value::as_str) != Some("rate_limit_event") {
                continue;
            }
            let info = &event["rate_limit_info"];
            for (name, kind, minutes) in [
                ("five_hour", "primary", 300),
                ("seven_day", "secondary", 10080),
            ] {
                let bucket = info["unifiedWindows"].get(name).or_else(|| {
                    (info.get("rateLimitType").and_then(Value::as_str) == Some(name))
                        .then_some(info)
                });
                if let Some(bucket) = bucket.and_then(|value| window(value, minutes, now)) {
                    windows.insert(kind.to_owned(), bucket);
                }
            }
        }
        if windows.is_empty() {
            return false;
        }
        let mut observation = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if observation.owner.is_none() || observation.generation != generation {
            return false;
        }
        observation.windows.extend(windows);
        observation.observed_at = now;
        true
    }

    pub(super) fn record_usage(&self, generation: u64, usage: &Value, now: f64) -> bool {
        let mut windows = serde_json::Map::new();
        for (name, kind, minutes) in [
            ("five_hour", "primary", 300),
            ("seven_day", "secondary", 10080),
        ] {
            let bucket = &usage["rate_limits"][name];
            let Some(used) = bucket["utilization"]
                .as_f64()
                .filter(|used| used.is_finite() && *used >= 0.0)
            else {
                continue;
            };
            // get_usage uses percentages and ISO dates; stream events use fractions and epoch seconds.
            let reset = match bucket.get("resets_at") {
                Some(Value::String(date)) => match time::OffsetDateTime::parse(
                    date,
                    &time::format_description::well_known::Rfc3339,
                ) {
                    Ok(date) if date.unix_timestamp() as f64 > now => Some(date.unix_timestamp()),
                    _ => continue,
                },
                None | Some(Value::Null) => None,
                _ => continue,
            };
            windows.insert(
                kind.into(),
                json!({"used_percent":used,"window_minutes":minutes,"resets_at":reset}),
            );
        }
        if windows.is_empty() {
            return false;
        }
        let mut observation = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if observation.generation != generation || observation.owner.is_none() {
            return false;
        }
        observation.windows = windows;
        observation.observed_at = now;
        observation.refreshed_at = now;
        true
    }

    pub(super) fn last_refresh(&self) -> f64 {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .refreshed_at
    }

    pub(super) fn persist(&self) -> Result<(), emp_codex::quota_history::QuotaHistoryError> {
        let observation = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(owner) = &observation.owner {
            self.history.append_snapshot(
                owner,
                &json!({"rate_limits_by_limit_id":{"claude":observation.windows}}),
                observation.observed_at as i64,
            )?;
        }
        Ok(())
    }

    pub(super) fn history(&self, start: i64, end: i64) -> Result<Value, &'static str> {
        let observation = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let owner = observation
            .owner
            .as_deref()
            .ok_or("claude_cli_login_required")?;
        self.history
            .query_period(owner, start, end)
            .map_err(|_| "claude_quota_history_unavailable")
    }

    pub(crate) fn snapshot(&self, now: f64) -> Value {
        let observation = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let windows: serde_json::Map<_, _> = observation
            .windows
            .iter()
            .filter(|(_, window)| window["resets_at"].as_f64().is_none_or(|reset| reset > now))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if windows.is_empty() {
            return Value::Null;
        }
        json!({"observed_at":observation.observed_at,"rate_limits_by_limit_id":{"claude":windows}})
    }
}

fn window(value: &Value, minutes: u32, now: f64) -> Option<Value> {
    let used = value.get("utilization")?.as_f64()? * 100.0;
    let resets_at = value.get("resetsAt")?.as_f64()?;
    if !used.is_finite() || used < 0.0 || !resets_at.is_finite() || resets_at <= now {
        return None;
    }
    Some(json!({"used_percent":used,"window_minutes":minutes,"resets_at":resets_at}))
}

/// These are status fields from the CLI, never read from its credential store.
pub(super) fn identity(stdout: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    let email = value
        .get("email")?
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 512)?;
    let org = value
        .get("orgId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    (org.len() <= 512).then(|| {
        format!(
            "claude:{:x}",
            Sha256::digest(json!([email, org]).to_string().as_bytes())
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_windows_preserve_used_units_ignore_text_and_expire_without_inventing_quota() {
        let directory = tempfile::tempdir().unwrap();
        let quota = LocalQuota::new(directory.path().join("quota.sqlite3"));
        let generation = quota.begin(Some("account-a".into()));
        let stdout = br#"{"type":"assistant","rate_limit_info":{"utilization":0.99,"resetsAt":9999,"rateLimitType":"five_hour"}}
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.24,"resetsAt":500},"seven_day":{"utilization":0.13,"resetsAt":900}},"private":"hidden"}}
{"type":"result","structured_output":{"answer":"done","tool_calls":[]}}
"#;
        assert!(quota.record(generation, stdout, 100.0));
        let snapshot = quota.snapshot(100.0);
        assert_eq!(
            snapshot["rate_limits_by_limit_id"]["claude"]["primary"]["used_percent"],
            24.0
        );
        assert_eq!(
            snapshot["rate_limits_by_limit_id"]["claude"]["secondary"]["used_percent"],
            13.0
        );
        assert!(!snapshot.to_string().contains("hidden"));
        assert!(
            quota.snapshot(500.0)["rate_limits_by_limit_id"]["claude"]
                .get("primary")
                .is_none()
        );
        assert!(quota.snapshot(900.0).is_null());
        assert!(!quota.record(
            generation,
            br#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            101.0
        ));
    }

    #[test]
    fn new_login_and_newer_requests_reject_old_observations() {
        let directory = tempfile::tempdir().unwrap();
        let quota = LocalQuota::new(directory.path().join("quota.sqlite3"));
        let event = br#"{"type":"rate_limit_event","rate_limit_info":{"rateLimitType":"five_hour","utilization":1.1,"resetsAt":500}}"#;
        let old = quota.begin(Some("account-a".into()));
        assert!(quota.record(old, event, 100.0));
        let new = quota.begin(Some("account-b".into()));
        assert!(quota.snapshot(100.0).is_null());
        assert!(!quota.record(old, event, 101.0));
        assert!(quota.record(new, event, 102.0));
        quota.begin(None);
        assert!(quota.snapshot(102.0).is_null());
        assert!(!quota.record(new, event, 103.0));
    }

    #[test]
    fn history_survives_restart_and_stays_with_the_verified_login() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quota.sqlite3");
        let now = crate::util::system_now();
        let quota = LocalQuota::new(&path);
        let generation = quota.begin(Some("account-a".into()));
        assert!(quota.record_usage(
            generation,
            &json!({"rate_limits":{"five_hour":{
                "utilization":24,"resets_at":null
            }}}),
            now
        ));
        quota.persist().unwrap();
        drop(quota);

        let quota = LocalQuota::new(path);
        let start = now as i64 - 60;
        let end = now as i64 + 60;
        assert!(quota.history(start, end).is_err());
        quota.identify("account-b".into());
        assert_eq!(quota.history(start, end).unwrap()["series"], json!([]));
        quota.identify("account-a".into());
        assert_eq!(
            quota.history(start, end).unwrap()["series"][0]["points"][0]["remaining_percent"],
            76.0
        );
    }
}
