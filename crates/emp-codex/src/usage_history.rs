//! Incremental read-only usage scanning of Codex rollout files.
use emp_state::usage::{
    TOKEN_FIELDS,
    ledger::{UsageError, UsageLedger},
    token_count,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
const COUNTS: [&str; 6] = [
    "input_tokens",
    "output_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "cache_write_1h_tokens",
    "reasoning_output_tokens",
];
type Routes = BTreeMap<String, (String, String)>;
fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or("").to_owned()
}
fn counts(value: &Value) -> Option<Value> {
    value.as_object().map(|value| {
        Value::Object(
            COUNTS
                .into_iter()
                .filter_map(|key| {
                    value
                        .get(key)
                        .and_then(Value::as_i64)
                        .filter(|n| *n >= 0)
                        .map(|n| (key.into(), json!(n)))
                })
                .collect(),
        )
    })
}
fn routes(config: &Value) -> Routes {
    let mut result = Routes::new();
    for account in config["accounts"].as_array().into_iter().flatten() {
        if let Some(prefix) = account["prefix"].as_str().filter(|s| !s.is_empty()) {
            result.insert(format!("{prefix}/"), ("subscription".into(), String::new()));
        }
    }
    for model in config["models"].as_array().into_iter().flatten() {
        if let Some(provider) = config["providers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["id"] == model["provider"])
        {
            let category = match provider["auth_mode"].as_str() {
                Some("account") => "subscription",
                Some("forward" | "native") => "native",
                _ => "external",
            };
            let upstream = model["upstream_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| model["id"].as_str().unwrap_or(""));
            let provider_prefix = format!("{}/", provider["id"].as_str().unwrap_or(""));
            let upstream = upstream.strip_prefix(&provider_prefix).unwrap_or(upstream);
            result.insert(text(model, "id"), (category.into(), upstream.into()));
        }
    }
    result
}
fn parse_record(record: &Value, state: &mut Value, routes: &Routes) -> Option<(Value, f64)> {
    let payload = record.get("payload").filter(|value| value.is_object())?;
    match record["type"].as_str()? {
        "session_meta" => {
            state["thread"] = json!(
                payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
            state["provider"] = json!(text(payload, "model_provider"));
            state["fork"] = json!(text(payload, "forked_from_id"));
            return None;
        }
        "turn_context" => {
            state["model"] = json!(text(payload, "model").chars().take(256).collect::<String>());
            if payload["turn_id"].as_str().is_some_and(|s| !s.is_empty()) {
                state["turn"] = payload["turn_id"].clone();
            }
            state["tier"] = json!(
                payload["service_tier"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("default")
            );
            return None;
        }
        "event_msg" => {}
        _ => return None,
    }
    if payload["type"] == "task_started" {
        state["turn"] = json!(text(payload, "turn_id"));
        return None;
    }
    if payload["type"] != "token_count" {
        return None;
    }
    let info = payload.get("info").filter(|value| value.is_object())?;
    let totals = counts(&info["total_token_usage"]).unwrap_or(Value::Null);
    let previous = state["totals"].clone();
    if totals.as_object().is_some_and(|v| !v.is_empty()) && totals == previous {
        return None;
    }
    state["totals"] = totals.clone();
    let mut usage = counts(&info["last_token_usage"]).unwrap_or(Value::Null);
    let mut issue = Value::Null;
    if usage.as_object().is_none_or(|v| v.is_empty())
        && let Some(totals) = totals.as_object().filter(|v| !v.is_empty())
    {
        if previous.as_object().is_some_and(|v| !v.is_empty())
            && totals.iter().all(|(key, value)| {
                value.as_i64().unwrap_or(0) >= previous[key].as_i64().unwrap_or(0)
            })
        {
            usage = Value::Object(
                totals
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            json!(
                                value.as_i64().unwrap_or(0) - previous[key].as_i64().unwrap_or(0)
                            ),
                        )
                    })
                    .collect(),
            );
        } else if state["fork"].as_str().is_none_or(str::is_empty) {
            usage = Value::Object(totals.clone());
        }
        issue = json!("aggregate_usage");
    }
    if usage.as_object().is_none_or(|v| v.is_empty()) {
        return None;
    }
    let model = text(state, "model");
    let timestamp = record["timestamp"]
        .as_str()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok());
    if model.is_empty() || timestamp.is_none() {
        state["unattributed"] = json!(state["unattributed"].as_u64().unwrap_or(0) + 1);
        return None;
    }
    let mut category = "unknown".to_owned();
    let mut upstream = model.clone();
    if !model.contains('/')
        && state["provider"] == "openai"
        && (model.starts_with("gpt-") || model.starts_with("codex-"))
    {
        category = "native".into();
    } else if let Some((kind, name)) = routes.get(&model) {
        category = kind.clone();
        upstream = name.clone();
    } else if let Some((prefix, tail)) = model.split_once('/')
        && let Some((kind, _)) = routes.get(&format!("{prefix}/"))
    {
        category = kind.clone();
        upstream = tail.into();
    }
    let owner = format!(
        "history:{}",
        model
            .split_once('/')
            .map(|(prefix, _)| prefix)
            .unwrap_or_else(|| state["provider"].as_str().unwrap_or("unknown"))
    );
    let mut event = Map::new();
    for (field, source) in TOKEN_FIELDS.into_iter().zip(COUNTS) {
        if let Some(value) = usage.get(source) {
            event.insert(field.into(), json!(token_count(Some(value))));
        }
    }
    if let (Some(total), Some(input), Some(output)) = (
        info["last_token_usage"]["total_tokens"].as_i64(),
        event.get("input_tokens").and_then(Value::as_i64),
        event.get("output_tokens").and_then(Value::as_i64),
    ) && total != input + output
    {
        issue = json!("inconsistent_usage");
    }
    if event
        .get("reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > event
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    {
        issue = json!("inconsistent_usage");
    }
    let turn = state
        .get("turn")
        .filter(|value| value.as_str().is_some_and(|s| !s.is_empty()))
        .or_else(|| state.get("thread"))
        .cloned()
        .unwrap_or(Value::Null);
    let row_id = format!(
        "history:{}",
        emp_state::usage::identity_hash(&json!([turn, record["timestamp"], model, usage]))
    );
    event.extend(json!({"observation_id":row_id,"origin":"history","usage_turn":text(state,"turn"),"route_model":model,"route":"responses","usage_category":category,"usage_owner":owner,"upstream_model":upstream,"service_tier":state["tier"].as_str().unwrap_or("default"),"usage_issue":issue}).as_object().unwrap().clone());
    Some((
        Value::Object(event),
        timestamp.unwrap().unix_timestamp_nanos() as f64 / 1e9,
    ))
}

pub struct UsageHistoryScanner {
    status: Mutex<Value>,
    scan_lock: Mutex<()>,
    pub stop: AtomicBool,
}
impl Default for UsageHistoryScanner {
    fn default() -> Self {
        Self {
            status: Mutex::new(
                json!({"running":false,"files":0,"updated":0,"errors":0,"last_scan_at":null}),
            ),
            scan_lock: Mutex::new(()),
            stop: AtomicBool::new(false),
        }
    }
}
impl UsageHistoryScanner {
    pub fn status(&self) -> Value {
        self.status.lock().expect("usage history status").clone()
    }
    pub fn queued(&self) {
        self.status.lock().expect("usage history status")["queued"] = json!(true);
    }
    fn increment(&self, key: &str) {
        let mut state = self.status.lock().expect("usage history status");
        state[key] = json!(state[key].as_u64().unwrap_or(0) + 1);
    }
    pub fn scan(&self, ledger: &UsageLedger, home: &Path, config: &Value, now: f64) {
        let Ok(_lock) = self.scan_lock.try_lock() else {
            return;
        };
        {
            let mut status = self.status.lock().expect("usage history status");
            for (key, value) in [
                ("running", json!(true)),
                ("queued", json!(false)),
                ("files", json!(0)),
                ("updated", json!(0)),
                ("errors", json!(0)),
            ] {
                status[key] = value;
            }
        }
        let routes = routes(config);
        let home = emp_state::config::resolve_user_path(home);
        // Compare paths in the same representation. Windows canonicalize()
        // adds the extended-length prefix to rollout paths, which a resolved
        // CODEX_HOME path does not necessarily have.
        let home = home.canonicalize().unwrap_or(home);
        for directory in [home.join("sessions"), home.join("archived_sessions")] {
            self.scan_directory(ledger, &home, &directory, &routes);
        }
        let mut status = self.status.lock().expect("usage history status");
        status["last_scan_at"] = json!(now);
        status["running"] = json!(false);
    }
    fn scan_directory(&self, ledger: &UsageLedger, home: &Path, directory: &Path, routes: &Routes) {
        if directory.is_symlink() || self.stop.load(Ordering::Acquire) {
            return;
        }
        let Ok(entries) = directory.read_dir() else {
            return;
        };
        for entry in entries.flatten() {
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            let path = entry.path();
            if path.is_dir() {
                self.scan_directory(ledger, home, &path, routes);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            self.increment("files");
            if path.is_symlink() || !path.is_file() {
                continue;
            }
            if path
                .canonicalize()
                .is_ok_and(|resolved| resolved.starts_with(home))
            {
                match self.scan_file(ledger, &path, routes) {
                    Ok(true) => self.increment("updated"),
                    Ok(false) => {}
                    Err(_) => self.increment("errors"),
                }
            } else {
                self.increment("errors");
            }
        }
    }
    fn scan_file(
        &self,
        ledger: &UsageLedger,
        path: &Path,
        routes: &Routes,
    ) -> Result<bool, UsageError> {
        let metadata = path.metadata()?;
        let size = metadata.len();
        let mtime = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| UsageError)?
            .as_nanos();
        let saved = ledger.history_checkpoint(&path.to_string_lossy())?;
        let mut stream = BufReader::new(File::open(path)?);
        let prefix = digest(&mut stream, 0, size.min(256))?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            json!([metadata.dev(), metadata.ino(), prefix])
        };
        #[cfg(not(unix))]
        let identity = json!([
            0,
            metadata
                .created()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|time| time.as_nanos().to_string()),
            prefix
        ]);
        let mut offset = 0;
        let mut state = json!({});
        if let Some(saved) = &saved
            && saved["identity"] == identity
            && let Some(position) = saved["offset"].as_u64().filter(|offset| *offset <= size)
            && saved["tail"]
                == digest(&mut stream, position.saturating_sub(256), position.min(256))?
        {
            if saved["size"] == size && saved["mtime"].as_u64().map(u128::from) == Some(mtime) {
                return Ok(false);
            }
            state = saved["state"].clone();
            offset = position;
        }
        stream.seek(SeekFrom::Start(offset))?;
        let mut events = Vec::new();
        let mut line = Vec::new();
        while !self.stop.load(Ordering::Acquire) {
            line.clear();
            if stream.read_until(b'\n', &mut line)? == 0 || !line.ends_with(b"\n") {
                break;
            }
            offset = stream.stream_position()?;
            if ![
                b"\"session_meta\"".as_slice(),
                b"\"turn_context\"",
                b"\"token_count\"",
                b"\"task_started\"",
            ]
            .iter()
            .any(|marker| line.windows(marker.len()).any(|window| window == *marker))
            {
                continue;
            }
            match serde_json::from_slice::<Value>(&line) {
                Ok(record) => {
                    if let Some(event) = parse_record(&record, &mut state, routes) {
                        events.push(event);
                    }
                }
                Err(_) => state["malformed"] = json!(state["malformed"].as_u64().unwrap_or(0) + 1),
            }
            if events.len() >= 200 {
                let checkpoint = json!({"identity":identity,"offset":offset,"size":offset,"mtime":mtime as u64,"state":state,"tail":digest(&mut stream,offset.saturating_sub(256),offset.min(256))?});
                ledger.record_history(&events, &path.to_string_lossy(), &checkpoint)?;
                events.clear();
            }
        }
        let checkpoint = json!({"identity":identity,"offset":offset,"size":size,"mtime":mtime as u64,"state":state,"tail":digest(&mut stream,offset.saturating_sub(256),offset.min(256))?});
        ledger.record_history(&events, &path.to_string_lossy(), &checkpoint)?;
        Ok(true)
    }
}
fn digest(stream: &mut BufReader<File>, start: u64, length: u64) -> std::io::Result<String> {
    let position = stream.stream_position()?;
    stream.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; length as usize];
    stream.read_exact(&mut bytes)?;
    stream.seek(SeekFrom::Start(position))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
