//! Bounded reverse lookup of a self-contained compaction checkpoint.
use super::REVERSE_SCAN_WINDOW_CAP;
use super::lines::{read_bounded_line, rollout_json_line};
use super::records::{payload_resume_metadata, payload_window_number, replacement_history};
use super::visible::{ordinal, record_turn_id, token};
use emp_history::HistoryError;
use serde_json::{Map, Value};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

/// Byte offset where the newest replacement-history compaction record ends.
#[derive(Debug)]
pub(super) struct ReverseBase {
    /// Records replayed instead of the full rollout prefix.
    pub(super) replacement: Vec<Map<String, Value>>,
    /// File offset of the compaction record, used to carry prefix controls.
    pub(super) record_start: u64,
    /// File offset of the first record after the compaction record.
    pub(super) suffix_start: u64,
    /// Explicit turn on the compaction record, if present.
    pub(super) turn: Option<String>,
    /// Ordinal of the compaction record; seeds suffix monotonicity.
    pub(super) ordinal: Option<u64>,
}

/// Offset of the first record boundary at or after `start`.
///
/// When `start` already sits exactly on a record start (offset 0, or the
/// byte before it is `\n`), the record beginning there is inside the window
/// and must stay visible; otherwise the boundary is the first `\n` in the
/// window plus one. Window end is returned when no complete line exists.
fn first_line_boundary(file: &mut File, start: u64, end: u64) -> Result<u64, HistoryError> {
    if start >= end {
        return Ok(end);
    }
    if start == 0 {
        return Ok(0);
    }
    file.seek(SeekFrom::Start(start - 1))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let mut preceding = [0u8; 1];
    file.read_exact(&mut preceding)
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    if preceding[0] == b'\n' {
        return Ok(start);
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(end - start));
    let mut offset = start;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        if available.is_empty() {
            return Ok(end);
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            return Ok(offset + newline as u64 + 1);
        }
        offset += available.len() as u64;
        let take = available.len();
        reader.consume(take);
    }
}

/// Locate the newest replacement-history compaction by scanning backward.
///
/// Windows grow from the tail so large rollouts only parse a bounded suffix.
/// The returned suffix starts at the first record boundary after the
/// compaction record. `None` means reconstruction must replay the whole
/// rollout.
///
/// Selection mirrors Codex: only the newest compaction can bound replay. An
/// older compaction cannot replace the history or window state that a newer
/// one may have changed, so the newest compaction record decides on its own
/// whether the bounded replay applies. A paginated history accepts the newest
/// compaction when it carries a replacement history and window number; other
/// histories additionally require the newer resume metadata contract.
pub(super) fn locate_latest_compaction(
    file: &mut File,
    captured_end: u64,
    history_mode: &str,
) -> Result<Option<ReverseBase>, HistoryError> {
    if captured_end == 0 {
        return Ok(None);
    }
    let mut window = captured_end.clamp(64 * 1024, 16 * 1024 * 1024);
    loop {
        let start = captured_end.saturating_sub(window);
        let line_start = first_line_boundary(file, start, captured_end)?;
        let length = (captured_end - line_start) as usize;
        file.seek(SeekFrom::Start(line_start))
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(length as u64));
        let mut line = Vec::new();
        let mut candidate: Option<ReverseBase> = None;
        let mut offset = line_start;
        loop {
            let Some(terminated) = read_bounded_line(&mut reader, &mut line)? else {
                break;
            };
            let record_start = offset;
            offset += line.len() as u64;
            match rollout_json_line(&line, terminated) {
                Ok(Some(record)) => {
                    if token(record.get("type")).as_str() == "compacted" {
                        // Scanning forward, so each later compaction replaces
                        // the earlier one: the newest encountered record is
                        // the only candidate. Whether it is eligible decides
                        // the whole outcome; never fall back to an older one.
                        let replacement = replacement_history(&record)?;
                        let eligible = replacement.is_some()
                            && payload_window_number(&record).is_some()
                            && (history_mode == "paginated"
                                || payload_resume_metadata(&record).is_some())
                            // An opaque compaction item resolves against the
                            // pre-compaction visible history, so it is not
                            // self-contained; the full scan owns that case.
                            && !replacement.as_ref().is_some_and(|items| {
                                items.iter().any(|item| {
                                    item.get("type").and_then(Value::as_str)
                                        == Some("compaction")
                                        && item
                                            .get("encrypted_content")
                                            .and_then(Value::as_str)
                                            .is_some_and(|value| !value.is_empty())
                                })
                            });
                        let base_ordinal = ordinal(&record);
                        // A paginated base without an ordinal cannot seed the
                        // suffix monotonicity check; the full scan owns it.
                        let has_seed_ordinal =
                            history_mode != "paginated" || base_ordinal.is_some();
                        candidate = (eligible && has_seed_ordinal).then(|| ReverseBase {
                            replacement: replacement.unwrap_or_default(),
                            record_start,
                            suffix_start: offset,
                            turn: record_turn_id(&record),
                            ordinal: base_ordinal,
                        });
                    }
                }
                Ok(None) => {}
                // Malformed complete lines are reported by the full scan.
                Err(HistoryError { .. }) if terminated => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        if candidate.is_some() {
            return Ok(candidate);
        }
        if start == 0 {
            // Whole file scanned without an eligible compaction base.
            return Ok(None);
        }
        if window >= REVERSE_SCAN_WINDOW_CAP {
            return Ok(None);
        }
        window = window.saturating_mul(2);
    }
}
