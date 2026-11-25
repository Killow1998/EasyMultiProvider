//! Private rotating JSONL diagnostics; journal failures never stop routing.
pub use emp_domain::redaction::redact_sensitive_text;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex,
    mpsc::{self, Receiver, Sender},
};
mod replay;
#[cfg(test)]
mod tests;
const QUEUE_CAPACITY: usize = 1024;
const MAX_RECORD: usize = 16 * 1024;
const MAX_PART: u64 = 2 * 1024 * 1024;
const MAX_DIRECTORY: u64 = 10 * 1024 * 1024;
static PART: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^emp-\d{8}T\d{6}Z-\d+-[0-9a-f]{16}-p\d+\.jsonl$").unwrap());
static CAMEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
static SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9]+").unwrap());
fn forbidden(key: &str) -> bool {
    let camel = CAMEL.replace_all(key.trim(), "${1}_${2}");
    let normalized = SEPARATOR
        .replace_all(&camel, "_")
        .trim_matches('_')
        .to_ascii_lowercase();
    [
        "authorization",
        "cookie",
        "cookies",
        "headers",
        "body",
        "request",
        "response",
        "prompt",
        "content",
        "input",
        "output",
        "api_key",
        "apikey",
        "token",
        "access_token",
        "refresh_token",
        "id_token",
        "session_token",
        "quota_token",
        "bootstrap",
        "session",
        "password",
        "secret",
        "credential",
        "credentials",
        "auth",
        "set_cookie",
    ]
    .contains(&normalized.as_str())
        || normalized.split('_').any(|part| {
            [
                "auth",
                "authentication",
                "authorization",
                "cookie",
                "cookies",
                "credential",
                "credentials",
                "password",
                "secret",
                "secrets",
            ]
            .contains(&part)
        })
        || ["_api_key", "_apikey", "_token"]
            .iter()
            .any(|suffix| normalized.ends_with(suffix))
}
fn sanitize(value: &Value, depth: usize) -> Value {
    if depth > 6 {
        return json!("<max-depth>");
    }
    match value {
        Value::String(text) => {
            let text = redact_sensitive_text(text);
            let mut limited = text.chars().take(2048).collect::<String>();
            if text.chars().count() > 2048 {
                limited.push_str("<truncated>");
            }
            json!(limited)
        }
        Value::Object(source) => {
            let mut result = serde_json::Map::new();
            for (key, value) in source.iter().take(128) {
                let key = key.chars().take(256).collect::<String>();
                if !forbidden(&key) {
                    result.insert(redact_sensitive_text(&key), sanitize(value, depth + 1));
                }
            }
            if source.len() > 128 {
                result.insert("_truncated_items".into(), json!(source.len() - 128));
            }
            Value::Object(result)
        }
        Value::Array(source) => {
            let mut result = source
                .iter()
                .take(128)
                .map(|value| sanitize(value, depth + 1))
                .collect::<Vec<_>>();
            if source.len() > 128 {
                result.push(json!(format!("<truncated:{}>", source.len() - 128)));
            }
            Value::Array(result)
        }
        other => other.clone(),
    }
}
fn no_symlinks(path: &Path) -> std::io::Result<()> {
    for prefix in path.ancestors() {
        if prefix.is_symlink() {
            return Err(std::io::Error::other("journal symlink"));
        }
    }
    Ok(())
}
fn private(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}
struct Command {
    dropped: usize,
    action: Action,
}

enum Action {
    Event(Event),
    Flush(Option<Event>, Sender<usize>),
}

#[derive(Default)]
struct QueueState {
    pending_events: usize,
    dropped: usize,
}

struct Event {
    timestamp: String,
    level: &'static str,
    name: String,
    fields: Value,
}

impl Event {
    fn new(level: &str, name: &str, fields: &Value) -> Self {
        let name = redact_sensitive_text(name).chars().take(256).collect();
        let fields = sanitize(fields, 0);
        Self {
            timestamp: super::timestamp(),
            level: match level {
                "debug" => "debug",
                "warning" => "warning",
                "error" => "error",
                _ => "info",
            },
            name,
            fields,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
type EventGate = Arc<Mutex<Option<(&'static str, Box<dyn FnOnce() + Send>)>>>;

pub struct Journal {
    directory: PathBuf,
    run_id: String,
    sender: Sender<Command>,
    queue: Arc<Mutex<QueueState>>,
    flush_lock: Mutex<()>,
    #[cfg(any(test, feature = "test-support"))]
    event_gate: EventGate,
}

impl Journal {
    fn channel(directory: PathBuf) -> (Self, Receiver<Command>) {
        let (sender, receiver) = mpsc::channel();
        let journal = Self {
            directory,
            run_id: super::random_id(8),
            sender,
            queue: Arc::default(),
            flush_lock: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            event_gate: Arc::default(),
        };
        (journal, receiver)
    }

    pub fn new(directory: PathBuf) -> Self {
        let (journal, receiver) = Self::channel(directory);
        let mut writer = Writer::new(
            journal.directory.clone(),
            journal.run_id.clone(),
            #[cfg(any(test, feature = "test-support"))]
            journal.event_gate.clone(),
        );
        let queue = Arc::clone(&journal.queue);
        if let Err(error) = std::thread::Builder::new()
            .name("emp-diagnostic-journal".into())
            .spawn(move || writer.run(receiver, &queue))
        {
            let _ = writeln!(
                std::io::stderr().lock(),
                "EMP diagnostic journal unavailable ({:?}); forwarding remains available",
                error.kind()
            );
        }
        journal
    }

    pub fn pseudonym(&self, value: &str) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!("emp-pseudonym:{}:{value}", self.run_id).as_bytes())
        )[..16]
            .to_owned()
    }

    pub fn event(&self, level: &str, name: &str, fields: &Value) {
        let event = Event::new(level, name, fields);
        let mut queue = self.queue.lock().unwrap();
        if queue.pending_events == QUEUE_CAPACITY
            || self
                .sender
                .send(Command {
                    dropped: queue.dropped,
                    action: Action::Event(event),
                })
                .is_err()
        {
            queue.dropped = queue.dropped.saturating_add(1);
        } else {
            queue.pending_events += 1;
            queue.dropped = 0;
        }
    }

    pub fn flush_blocking(&self) {
        self.confirm(None);
    }

    pub fn event_and_flush_blocking(&self, level: &str, name: &str, fields: &Value) {
        self.confirm(Some(Event::new(level, name, fields)));
    }

    fn confirm(&self, event: Option<Event>) -> usize {
        let _flush = self.flush_lock.lock().unwrap();
        let (done, receipt) = mpsc::channel();
        {
            let mut queue = self.queue.lock().unwrap();
            let dropped = std::mem::take(&mut queue.dropped);
            let unsent = dropped.saturating_add(usize::from(event.is_some()));
            if self
                .sender
                .send(Command {
                    dropped,
                    action: Action::Flush(event, done),
                })
                .is_err()
            {
                drop(queue);
                report_dropped(unsent);
                return unsent;
            }
        }
        receipt.recv().unwrap_or_default()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn gate_next_event(&self, name: &'static str, gate: impl FnOnce() + Send + 'static) {
        assert!(
            self.event_gate
                .lock()
                .unwrap()
                .replace((name, Box::new(gate)))
                .is_none()
        );
    }
}

fn report_dropped(count: usize) {
    if count != 0 {
        let _ = writeln!(
            std::io::stderr().lock(),
            "EMP diagnostic journal dropped {count} records"
        );
    }
}

struct Writer {
    directory: PathBuf,
    run_id: String,
    basename: String,
    file: Option<File>,
    part: u32,
    bytes: u64,
    sequence: u64,
    managed: u64,
    #[cfg(any(test, feature = "test-support"))]
    event_gate: EventGate,
}

impl Writer {
    fn new(
        directory: PathBuf,
        run_id: String,
        #[cfg(any(test, feature = "test-support"))] event_gate: EventGate,
    ) -> Self {
        let now = time::OffsetDateTime::now_utc();
        let basename = format!(
            "emp-{:04}{:02}{:02}T{:02}{:02}{:02}Z-{}-{run_id}",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
            std::process::id()
        );
        Self {
            directory,
            run_id,
            basename,
            file: None,
            part: 1,
            bytes: 0,
            sequence: 0,
            managed: 0,
            #[cfg(any(test, feature = "test-support"))]
            event_gate,
        }
    }

    fn run(&mut self, receiver: Receiver<Command>, queue: &Mutex<QueueState>) {
        let opened = (|| -> std::io::Result<()> {
            no_symlinks(&self.directory)?;
            std::fs::create_dir_all(&self.directory)?;
            no_symlinks(&self.directory)?;
            private(&self.directory, 0o700)?;
            self.file = Some(self.open_part(self.part)?);
            self.prune();
            Ok(())
        })();
        if let Err(error) = opened {
            let _ = writeln!(
                std::io::stderr().lock(),
                "EMP diagnostic journal unavailable ({:?}); forwarding remains available",
                error.kind()
            );
        }
        let mut losses = 0usize;
        for Command { dropped, action } in receiver {
            let (event, receipt) = match action {
                Action::Event(event) => {
                    queue.lock().unwrap().pending_events -= 1;
                    (Some(event), None)
                }
                Action::Flush(event, receipt) => (event, Some(receipt)),
            };
            losses = losses.saturating_add(dropped);
            if dropped != 0 {
                self.write_event(&Event {
                    timestamp: super::timestamp(),
                    level: "warning",
                    name: "journal_records_dropped".into(),
                    fields: json!({"count":dropped}),
                });
            }
            if let Some(event) = event
                && !self.write_event(&event)
            {
                losses = losses.saturating_add(1);
            }
            if let Some(receipt) = receipt {
                report_dropped(losses);
                let _ = receipt.send(std::mem::take(&mut losses));
            }
        }
        report_dropped(losses.saturating_add(queue.lock().unwrap().dropped));
    }

    fn path(&self, part: u32) -> PathBuf {
        self.directory
            .join(format!("{}-p{part:03}.jsonl", self.basename))
    }
    fn open_part(&self, part: u32) -> std::io::Result<File> {
        no_symlinks(&self.directory)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options.open(self.path(part))
    }
    fn prune(&mut self) {
        let parts = parts(&self.directory);
        self.managed = parts.iter().map(|part| part.3).sum();
        let newest = parts.last().map(|part| part.2.as_path());
        for (_, _, path, size) in &parts {
            if self.managed <= MAX_DIRECTORY {
                break;
            }
            if *path != self.path(self.part)
                && Some(path.as_path()) != newest
                && std::fs::remove_file(path).is_ok()
            {
                self.managed -= size;
            }
        }
    }

    fn write_event(&mut self, event: &Event) -> bool {
        if self.file.is_none() {
            return false;
        }
        let mut record = json!({"timestamp":event.timestamp,"sequence":self.sequence+1,"run_id":self.run_id,"level":event.level,"event":event.name,"fields":event.fields});
        let mut bytes = serde_json::to_vec(&record).expect("diagnostic JSON");
        if bytes.len() > MAX_RECORD {
            record["fields"] = json!({"_dropped":true,"_event":event.name});
            bytes = serde_json::to_vec(&record).unwrap();
        }
        bytes.push(b'\n');
        let result = (|| -> std::io::Result<()> {
            if self.bytes > 0 && self.bytes + bytes.len() as u64 > MAX_PART {
                self.file = None;
                self.part += 1;
                self.file = Some(self.open_part(self.part)?);
                self.bytes = 0;
                self.prune();
            }
            let file = self.file.as_mut().unwrap();
            #[cfg(any(test, feature = "test-support"))]
            {
                let gate = {
                    let mut armed = self.event_gate.lock().unwrap();
                    armed.take_if(|(name, _)| *name == event.name)
                };
                if let Some((_, wait)) = gate {
                    wait();
                }
            }
            file.write_all(&bytes)?;
            file.flush()?;
            self.sequence += 1;
            self.bytes += bytes.len() as u64;
            self.managed += bytes.len() as u64;
            if self.managed > MAX_DIRECTORY {
                self.prune();
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.file = None;
            let _ = writeln!(
                std::io::stderr().lock(),
                "EMP diagnostic journal stopped ({:?}); forwarding remains available",
                error.kind()
            );
            return false;
        }
        true
    }
}

fn parts(directory: &Path) -> Vec<(std::time::SystemTime, String, PathBuf, u64)> {
    if no_symlinks(directory).is_err() {
        return Vec::new();
    }
    let Ok(entries) = directory.read_dir() else {
        return Vec::new();
    };
    let mut parts = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !PART.is_match(&name) {
                return None;
            }
            let metadata = entry.path().symlink_metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some((
                metadata.modified().ok()?,
                name,
                entry.path(),
                metadata.len(),
            ))
        })
        .collect::<Vec<_>>();
    parts.sort();
    parts
}
