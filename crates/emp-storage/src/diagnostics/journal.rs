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
    Event(Record),
    Flush {
        record: Option<Record>,
        rejected: usize,
        receipt: Sender<usize>,
    },
}

#[derive(Default)]
struct QueueState {
    pending_events: usize,
    rejected: usize,
    accepted: usize,
    settled: usize,
    lost_by_writer: usize,
    attached_rejected: usize,
    reported: usize,
}

impl QueueState {
    fn take_losses(&mut self, rejected_at_flush: Option<usize>) -> usize {
        let total = match rejected_at_flush {
            Some(rejected) => rejected + self.lost_by_writer,
            None => self.rejected + self.lost_by_writer + (self.accepted - self.settled),
        };
        let unreported = total - self.reported;
        self.reported = total;
        unreported
    }
}

struct Record {
    timestamp: String,
    event: Event,
}

struct Event {
    level: &'static str,
    name: String,
    fields: Value,
}

impl Event {
    fn new(level: &str, name: &str, fields: &Value) -> Self {
        let name = redact_sensitive_text(name).chars().take(256).collect();
        let fields = sanitize(fields, 0);
        Self {
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
mod event_gates {
    use std::sync::{Arc, Mutex};

    /// Points where tests pause the journal: before an event is enqueued and before it is written.
    #[derive(Default)]
    pub(super) struct EventGates {
        pub(super) before_write: EventGate,
        pub(super) before_enqueue: EventGate,
    }

    pub(super) type SharedEventGates = Arc<EventGates>;

    #[derive(Default)]
    pub(super) struct EventGate(Mutex<Option<ArmedEventGate>>);

    struct ArmedEventGate {
        name: &'static str,
        wait: Box<dyn FnOnce() + Send>,
    }

    impl EventGate {
        pub(super) fn arm(&self, name: &'static str, gate: impl FnOnce() + Send + 'static) {
            assert!(
                self.0
                    .lock()
                    .unwrap()
                    .replace(ArmedEventGate {
                        name,
                        wait: Box::new(gate),
                    })
                    .is_none()
            );
        }

        pub(super) fn run(&self, name: &str) {
            let gate = self.0.lock().unwrap().take_if(|armed| armed.name == name);
            if let Some(ArmedEventGate { wait, .. }) = gate {
                wait();
            }
        }
    }
}

#[cfg(not(any(test, feature = "test-support")))]
mod event_gates {
    #[derive(Clone, Default)]
    pub(super) struct EventGates {
        pub(super) before_write: EventGate,
        pub(super) before_enqueue: EventGate,
    }

    pub(super) type SharedEventGates = EventGates;

    #[derive(Clone, Default)]
    pub(super) struct EventGate {}

    const _: () = assert!(size_of::<EventGates>() == 0);

    impl EventGate {
        #[inline]
        pub(super) fn run(&self, _name: &str) {}
    }
}

use event_gates::SharedEventGates;

pub struct Journal {
    directory: PathBuf,
    run_id: String,
    sender: Sender<Command>,
    queue: Arc<Mutex<QueueState>>,
    flush_lock: Mutex<()>,
    event_gates: SharedEventGates,
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
            event_gates: SharedEventGates::default(),
        };
        (journal, receiver)
    }

    pub fn new(directory: PathBuf) -> Self {
        let (journal, receiver) = Self::channel(directory);
        let mut writer = Writer::new(
            journal.directory.clone(),
            journal.run_id.clone(),
            journal.event_gates.clone(),
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
        self.event_gates.before_enqueue.run(&event.name);
        let mut queue = self.queue.lock().unwrap();
        if queue.pending_events == QUEUE_CAPACITY {
            queue.rejected += 1;
            return;
        }
        let record = Record {
            timestamp: super::timestamp(),
            event,
        };
        if self.enqueue(&mut queue, Action::Event(record), 1) {
            queue.pending_events += 1;
        }
    }

    fn enqueue(&self, queue: &mut QueueState, action: Action, records: usize) -> bool {
        let dropped = queue.rejected - queue.attached_rejected;
        if self.sender.send(Command { dropped, action }).is_ok() {
            queue.accepted += records;
            queue.attached_rejected = queue.rejected;
            true
        } else {
            queue.rejected += records;
            false
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
        let sent = {
            let mut queue = self.queue.lock().unwrap();
            let rejected = queue.rejected;
            let records = usize::from(event.is_some());
            let record = event.map(|event| Record {
                timestamp: super::timestamp(),
                event,
            });
            self.enqueue(
                &mut queue,
                Action::Flush {
                    record,
                    rejected,
                    receipt: done,
                },
                records,
            )
        };
        if sent && let Ok(losses) = receipt.recv() {
            return losses;
        }
        report_losses(&self.queue, None)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn gate_next_event(&self, name: &'static str, gate: impl FnOnce() + Send + 'static) {
        self.event_gates.before_write.arm(name, gate);
    }

    #[cfg(feature = "test-support")]
    pub fn flush_pending_for_test(&self) -> bool {
        self.flush_lock.try_lock().is_err()
    }
}

fn report_losses(queue: &Mutex<QueueState>, rejected_at_flush: Option<usize>) -> usize {
    let count = queue.lock().unwrap().take_losses(rejected_at_flush);
    if count != 0 {
        let _ = writeln!(
            std::io::stderr().lock(),
            "EMP diagnostic journal dropped {count} records"
        );
    }
    count
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
    event_gates: SharedEventGates,
}

impl Writer {
    fn new(directory: PathBuf, run_id: String, event_gates: SharedEventGates) -> Self {
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
            event_gates,
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
        for Command { dropped, action } in receiver {
            let (record, confirmation) = match action {
                Action::Event(record) => {
                    queue.lock().unwrap().pending_events -= 1;
                    (Some(record), None)
                }
                Action::Flush {
                    record,
                    rejected,
                    receipt,
                } => (record, Some((rejected, receipt))),
            };
            if dropped != 0 {
                self.write_event(
                    &super::timestamp(),
                    &Event {
                        level: "warning",
                        name: "journal_records_dropped".into(),
                        fields: json!({"count":dropped}),
                    },
                );
            }
            if let Some(record) = record {
                let lost = !self.write_event(&record.timestamp, &record.event);
                let mut queue = queue.lock().unwrap();
                queue.settled += 1;
                queue.lost_by_writer += usize::from(lost);
            }
            if let Some((rejected, receipt)) = confirmation {
                let _ = receipt.send(report_losses(queue, Some(rejected)));
            }
        }
        report_losses(queue, None);
    }

    fn path(&self, part: u32) -> PathBuf {
        self.directory
            .join(format!("{}-p{part:03}.jsonl", self.basename))
    }
    fn open_part(&self, part: u32) -> std::io::Result<File> {
        let _parts_lock = self.lock_parts()?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(self.path(part))?;
        #[cfg(unix)]
        file.lock()?;
        Ok(file)
    }

    fn lock_parts(&self) -> std::io::Result<File> {
        no_symlinks(&self.directory)?;
        let mut lock_path = self.directory.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        no_symlinks(&lock_path)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(lock_path)?;
        file.lock()?;
        Ok(file)
    }

    fn prune(&mut self) {
        let Ok(_parts_lock) = self.lock_parts() else {
            return;
        };
        let parts = parts(&self.directory);
        self.managed = parts.iter().map(|part| part.3).sum();
        let newest = parts.last().map(|part| part.2.as_path());
        for (_, _, path, size) in &parts {
            if self.managed <= MAX_DIRECTORY {
                break;
            }
            if *path != self.path(self.part)
                && Some(path.as_path()) != newest
                && remove_closed_part(path)
            {
                self.managed -= size;
            }
        }
    }

    fn write_event(&mut self, timestamp: &str, event: &Event) -> bool {
        if self.file.is_none() {
            return false;
        }
        let mut record = json!({"timestamp":timestamp,"sequence":self.sequence+1,"run_id":self.run_id,"level":event.level,"event":event.name,"fields":event.fields});
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
            self.event_gates.before_write.run(&event.name);
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

fn remove_closed_part(path: &Path) -> bool {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    let Ok(file) = options.open(path) else {
        return false;
    };
    #[cfg(unix)]
    if file.try_lock().is_err() {
        return false;
    }
    let removed = std::fs::remove_file(path).is_ok();
    drop(file);
    removed
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
