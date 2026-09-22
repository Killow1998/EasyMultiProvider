//! Private, rotating Python-compatible JSONL journal. Failures never stop routing.
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
const MAX_RECORD: usize = 16 * 1024;
const MAX_PART: u64 = 2 * 1024 * 1024;
const MAX_DIRECTORY: u64 = 10 * 1024 * 1024;
static PART: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^emp-\d{8}T\d{6}Z-\d+-[0-9a-f]{16}-p\d+\.jsonl$").unwrap());
static REDACTIONS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
    r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{6,}", r"(?i)\bsk-[A-Za-z0-9_-]{10,}\b",
    r"\bAIza[0-9A-Za-z_-]{20,}\b", r"(?i)\bgh[pousr]_[A-Za-z0-9]{20,}\b",
    r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}\b",
    r#"(?i)\b(api[-_ ]?key|access[-_ ]?token|refresh[-_ ]?token|token|password|secret|set[-_ ]?cookie|cookie|authorization|bootstrap|session)\s*[=:]\s*(?:bearer\s+)?["']?[^\s"',;&{}]{4,}"#,
].into_iter().map(|pattern| Regex::new(pattern).unwrap()).collect()
});
static CAMEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
static SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9]+").unwrap());
fn redact(text: &str) -> String {
    REDACTIONS.iter().fold(text.to_owned(), |text, pattern| {
        pattern.replace_all(&text, "<redacted>").into_owned()
    })
}
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
            let text = redact(text);
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
                    result.insert(redact(&key), sanitize(value, depth + 1));
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
struct Writer {
    file: Option<File>,
    part: u32,
    bytes: u64,
    sequence: u64,
    managed: u64,
}
pub struct Journal {
    directory: PathBuf,
    run_id: String,
    basename: String,
    writer: Mutex<Writer>,
}
impl Journal {
    pub fn new(directory: PathBuf) -> Self {
        let now = time::OffsetDateTime::now_utc();
        let run_id = super::random_id(8);
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
        let journal = Self {
            directory,
            run_id,
            basename,
            writer: Mutex::new(Writer {
                file: None,
                part: 1,
                bytes: 0,
                sequence: 0,
                managed: 0,
            }),
        };
        let mut writer = journal.writer.lock().unwrap();
        let opened = (|| -> std::io::Result<()> {
            no_symlinks(&journal.directory)?;
            std::fs::create_dir_all(&journal.directory)?;
            no_symlinks(&journal.directory)?;
            private(&journal.directory, 0o700)?;
            writer.file = Some(journal.open_part(1)?);
            journal.prune(&mut writer);
            Ok(())
        })();
        if opened.is_err() {
            writer.file = None;
        }
        drop(writer);
        journal
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
    pub fn pseudonym(&self, value: &str) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!("emp-pseudonym:{}:{value}", self.run_id).as_bytes())
        )[..16]
            .to_owned()
    }
    fn parts(&self) -> Vec<(std::time::SystemTime, String, PathBuf, u64)> {
        if no_symlinks(&self.directory).is_err() {
            return Vec::new();
        }
        let Ok(entries) = self.directory.read_dir() else {
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
    fn prune(&self, writer: &mut Writer) {
        let parts = self.parts();
        writer.managed = parts.iter().map(|part| part.3).sum();
        let newest = parts.last().map(|part| part.2.as_path());
        for (_, _, path, size) in &parts {
            if writer.managed <= MAX_DIRECTORY {
                break;
            }
            if *path != self.path(writer.part)
                && Some(path.as_path()) != newest
                && std::fs::remove_file(path).is_ok()
            {
                writer.managed -= size;
            }
        }
    }
    pub fn event(&self, level: &str, name: &str, fields: &Value) {
        let mut writer = self.writer.lock().expect("diagnostic journal");
        if writer.file.is_none() {
            return;
        }
        let level = if ["debug", "info", "warning", "error"].contains(&level) {
            level
        } else {
            "info"
        };
        let name = redact(name).chars().take(256).collect::<String>();
        let mut record = json!({"timestamp":super::timestamp(),"sequence":writer.sequence+1,"run_id":self.run_id,"level":level,"event":name,"fields":sanitize(fields,0)});
        let mut bytes = serde_json::to_vec(&record).expect("diagnostic JSON");
        if bytes.len() > MAX_RECORD {
            record["fields"] = json!({"_dropped":true,"_event":name});
            bytes = serde_json::to_vec(&record).unwrap();
        }
        bytes.push(b'\n');
        let result = (|| -> std::io::Result<()> {
            if writer.bytes > 0 && writer.bytes + bytes.len() as u64 > MAX_PART {
                writer.file = None;
                writer.part += 1;
                writer.file = Some(self.open_part(writer.part)?);
                writer.bytes = 0;
                self.prune(&mut writer);
            }
            writer.file.as_mut().unwrap().write_all(&bytes)?;
            writer.file.as_mut().unwrap().flush()?;
            writer.sequence += 1;
            writer.bytes += bytes.len() as u64;
            writer.managed += bytes.len() as u64;
            if writer.managed > MAX_DIRECTORY {
                self.prune(&mut writer);
            }
            Ok(())
        })();
        if result.is_err() {
            writer.file = None;
        }
    }
    pub fn read_routes(&self, limit: usize) -> Vec<Value> {
        let mut recent = Vec::new();
        let mut order = 0;
        for (_, _, path, _) in self.parts() {
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let Ok(file) = options.open(path) else {
                continue;
            };
            let mut reader = BufReader::new(file);
            let mut line = Vec::new();
            let mut oversized = false;
            loop {
                let Ok(buffer) = reader.fill_buf() else {
                    break;
                };
                if buffer.is_empty() {
                    break;
                }
                let length = buffer
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(buffer.len(), |i| i + 1);
                let ended = buffer[length - 1] == b'\n';
                if line.len() + length > MAX_RECORD + 1 {
                    oversized = true;
                }
                if !oversized {
                    line.extend_from_slice(&buffer[..length]);
                }
                reader.consume(length);
                if !ended {
                    continue;
                }
                if !oversized
                    && let Ok(record) = serde_json::from_slice::<Value>(&line)
                    && record["event"] == "route_observation"
                    && record["fields"].is_object()
                {
                    recent.push((
                        record["timestamp"].as_str().unwrap_or("").to_owned(),
                        order,
                        record["fields"].clone(),
                    ));
                    order += 1;
                    recent.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
                    if recent.len() > limit {
                        recent.remove(0);
                    }
                }
                line.clear();
                oversized = false;
            }
        }
        recent.into_iter().map(|(_, _, fields)| fields).collect()
    }
}
