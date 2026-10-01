//! Bounded JSONL framing, including truncated final lines and frozen file ends.
use super::MAX_ROLLOUT_LINE_BYTES;
use super::source::WalkReport;
use emp_history::HistoryError;
use serde_json::{Map, Value};
use std::fs::File;
#[cfg(test)]
use std::io::SeekFrom;
use std::io::{BufRead, BufReader, Read, Seek};

/// Walk every parsed record in the byte range from the current seek position
/// to `end`, feeding each to `visit`. `Ok(false)` from `visit` stops the walk.
pub(super) fn walk_records(
    file: &mut File,
    end: u64,
    mut visit: impl FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
) -> Result<WalkReport, HistoryError> {
    let start = file
        .stream_position()
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    if start > end {
        return Err(HistoryError::new("invalid_replay_range"));
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(end - start));
    walk_bounded_reader(&mut reader, end - start, "source_unavailable", &mut visit)
}

pub(super) fn walk_bounded_reader(
    reader: &mut impl BufRead,
    end: u64,
    io_error_reason: &str,
    mut visit: impl FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
) -> Result<WalkReport, HistoryError> {
    let mut line = Vec::new();
    let mut bytes_read = 0u64;
    while let Some(terminated) = read_bounded_line_with_reason(reader, &mut line, io_error_reason)?
    {
        bytes_read = bytes_read.saturating_add(line.len() as u64);
        if bytes_read > end {
            return Err(HistoryError::new("invalid_replay_range"));
        }
        if let Some(record) = rollout_json_line(&line, terminated)?
            && !visit(record)?
        {
            return Ok(WalkReport {
                bytes_read,
                stopped_early: true,
            });
        }
    }
    Ok(WalkReport {
        bytes_read,
        stopped_early: false,
    })
}

pub(super) fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
) -> Result<Option<bool>, HistoryError> {
    read_bounded_line_with_reason(reader, line, "source_unavailable")
}

pub(super) fn read_bounded_line_with_reason(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    io_error_reason: &str,
) -> Result<Option<bool>, HistoryError> {
    line.clear();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| HistoryError::new(io_error_reason))?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(false));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let take = newline + 1;
            if line.len().saturating_add(take) > MAX_ROLLOUT_LINE_BYTES {
                return Err(HistoryError::new("history_record_too_large"));
            }
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(Some(true));
        }
        if line.len().saturating_add(available.len()) > MAX_ROLLOUT_LINE_BYTES {
            return Err(HistoryError::new("history_record_too_large"));
        }
        line.extend_from_slice(available);
        let take = available.len();
        reader.consume(take);
    }
}

pub(super) fn rollout_json_line(
    line: &[u8],
    terminated: bool,
) -> Result<Option<Map<String, Value>>, HistoryError> {
    let raw = if terminated {
        &line[..line.len().saturating_sub(1)]
    } else {
        line
    };
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(value)) => Ok(Some(value)),
        Ok(_) => Err(HistoryError::new("invalid_json")),
        Err(error) if !terminated && error.is_eof() => Ok(None),
        Err(_) => Err(HistoryError::new("invalid_json")),
    }
}

#[cfg(test)]
mod walker_boundary_tests {
    use super::*;
    use std::io::Write;

    /// The walker's `end` is an absolute offset captured BEFORE the walk
    /// starts. Records appended after the capture (a concurrent writer) must
    /// never be observed, even when the handle's file grew behind the walk.
    #[test]
    fn walker_absolute_end_ignores_concurrent_append() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        std::fs::write(&path, "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"first\"}}\n").unwrap();
        let mut file = File::open(&path).unwrap();
        let captured_end = file.metadata().unwrap().len();
        // Seek somewhere non-zero like the reverse probe does, then append.
        file.seek(SeekFrom::Start(4)).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                b"{\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"APPENDED\"}}\n",
            )
            .unwrap();
        // Rewind to a non-zero start and walk to the frozen absolute end.
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut seen = Vec::new();
        walk_records(&mut file, captured_end, |record| {
            seen.push(serde_json::to_string(&record).unwrap());
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen.len(), 1, "walker must stop at the frozen end");
        assert!(!seen[0].contains("APPENDED"), "{:?}", seen[0]);
    }
}
