//! Bounded version observations; raw process output never reaches the UI.
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) fn public(installed: Option<String>, status: &str) -> Value {
    json!({"installed":installed,"status":status,"supported_range":"0.149.x–0.156.x","recommended":"0.156.1"})
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.')
}
fn number(bytes: &[u8], position: &mut usize, limit: usize) -> Option<u32> {
    let start = *position;
    while bytes.get(*position).is_some_and(u8::is_ascii_digit) {
        *position += 1;
    }
    if *position == start || *position - start > limit {
        return None;
    }
    std::str::from_utf8(&bytes[start..*position])
        .ok()?
        .parse()
        .ok()
}
fn suffix(bytes: &[u8], position: &mut usize, prefix: u8) -> Option<String> {
    if bytes.get(*position) != Some(&prefix) {
        return Some(String::new());
    }
    let start = *position;
    *position += 1;
    while bytes
        .get(*position)
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
    {
        *position += 1;
    }
    (*position > start + 1).then(|| String::from_utf8_lossy(&bytes[start..*position]).into_owned())
}
fn parse(bytes: &[u8], start: usize) -> Option<(String, &'static str)> {
    let mut position = start + usize::from(bytes.get(start) == Some(&b'v'));
    let major = number(bytes, &mut position, 4)?;
    if bytes.get(position) != Some(&b'.') {
        return None;
    }
    position += 1;
    let minor = number(bytes, &mut position, 4)?;
    if bytes.get(position) != Some(&b'.') {
        return None;
    }
    position += 1;
    let patch = number(bytes, &mut position, 6)?;
    let prerelease = suffix(bytes, &mut position, b'-')?;
    let build = suffix(bytes, &mut position, b'+')?;
    if bytes
        .get(position)
        .is_some_and(|c| word(*c) || matches!(c, b'+' | b'-'))
    {
        return None;
    }
    let status = if (major, minor) < (0, 149) {
        "unsupported"
    } else if !prerelease.is_empty() || (major, minor) > (0, 156) {
        "unverified"
    } else if (major, minor) == (0, 156) && patch >= 1 {
        "recommended"
    } else {
        "supported"
    };
    Some((
        format!("{major}.{minor}.{patch}{prerelease}{build}"),
        status,
    ))
}
fn classify(output: &str) -> Value {
    let bytes = output.as_bytes();
    for start in 0..bytes.len() {
        if start > 0 && word(bytes[start - 1]) {
            continue;
        }
        if let Some((version, status)) = parse(bytes, start) {
            return public(Some(version), status);
        }
    }
    public(None, "unknown")
}

pub(super) fn observe(path: &Path) -> Value {
    observe_inner(path).unwrap_or_else(|error| {
        public(
            None,
            if error.kind() == std::io::ErrorKind::NotFound {
                "unavailable"
            } else {
                "unknown"
            },
        )
    })
}
fn observe_inner(path: &Path) -> std::io::Result<Value> {
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline
            || output.metadata()?.len() > 2 * 1024 * 1024
            || errors.metadata()?.len() > 32 * 1024
        {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let Some(status) = status else {
        return Ok(public(None, "unknown"));
    };
    output.seek(SeekFrom::Start(0))?;
    errors.seek(SeekFrom::Start(0))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    output.take(2 * 1024 * 1024).read_to_end(&mut stdout)?;
    errors.take(32 * 1024).read_to_end(&mut stderr)?;
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    Ok(if status.success() {
        classify(&stdout)
    } else if status.code() == Some(127)
        || format!("{stdout} {stderr}")
            .to_lowercase()
            .contains("not found")
    {
        public(None, "unavailable")
    } else {
        public(None, "unknown")
    })
}

#[cfg(test)]
mod tests {
    use super::classify;
    use serde_json::json;

    #[test]
    fn codex_0156_and_0157_boundaries_match_python_0119() {
        for (output, installed, status) in [
            ("codex-cli 0.156.0", "0.156.0", "supported"),
            ("codex-cli 0.156.1", "0.156.1", "recommended"),
            ("codex-cli 0.157.0", "0.157.0", "unverified"),
            ("codex-cli 0.157.9", "0.157.9", "unverified"),
        ] {
            assert_eq!(
                classify(output),
                json!({
                    "installed":installed,
                    "status":status,
                    "supported_range":"0.149.x–0.156.x",
                    "recommended":"0.156.1",
                }),
                "classification for {output}"
            );
        }
    }
}
