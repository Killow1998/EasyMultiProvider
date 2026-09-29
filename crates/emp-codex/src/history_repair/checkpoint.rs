//! Checkpoint conversion and bounded JSONL replacement writing.

use super::replay::RolloutSummary;
use super::spool::JsonlSpool;
use super::transaction::file_fingerprint;
use super::{ContextSnapshot, HistoryRepairError};
use crate::history::MAX_ROLLOUT_LINE_BYTES;
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

fn convert_emp_item(
    mut item: Map<String, Value>,
) -> Result<(Map<String, Value>, Option<String>), HistoryRepairError> {
    let mut latest_summary = None;
    if matches!(
        item.get("type").and_then(Value::as_str),
        Some("compaction" | "compaction_summary")
    ) && let Some(encoded) = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .and_then(|value| value.strip_prefix("emp1:"))
    {
        let bytes = URL_SAFE
            .decode(encoded)
            .or_else(|_| {
                use base64::engine::general_purpose::URL_SAFE_NO_PAD;
                URL_SAFE_NO_PAD.decode(encoded)
            })
            .map_err(|_| HistoryRepairError::new("emp_checkpoint_invalid"))?;
        let summary = String::from_utf8(bytes)
            .map_err(|_| HistoryRepairError::new("emp_checkpoint_invalid"))?;
        if summary.trim().is_empty() {
            return Err(HistoryRepairError::new("emp_checkpoint_empty"));
        }
        item.insert("type".to_owned(), json!("message"));
        item.remove("encrypted_content");
        item.insert("role".to_owned(), json!("user"));
        item.insert(
            "content".to_owned(),
            json!([{"type":"input_text","text":summary.clone()}]),
        );
        latest_summary = Some(summary);
    }
    if map_contains_emp1_encrypted_content(&item) {
        return Err(HistoryRepairError::new(
            "unsupported_emp_checkpoint_location",
        ));
    }
    Ok((item, latest_summary))
}

fn contains_emp1_encrypted_content(value: &Value) -> bool {
    match value {
        Value::Object(object) => map_contains_emp1_encrypted_content(object),
        Value::Array(items) => items.iter().any(contains_emp1_encrypted_content),
        _ => false,
    }
}

pub(super) fn map_contains_emp1_encrypted_content(object: &Map<String, Value>) -> bool {
    object.get("encrypted_content").is_some_and(|content| {
        content
            .as_str()
            .is_some_and(|value| value.starts_with("emp1:"))
    }) || object.values().any(contains_emp1_encrypted_content)
}

fn replacement_checkpoint(
    rollout: &RolloutSummary,
    snapshot: &mut ContextSnapshot,
    summary: Option<&str>,
) -> Result<Map<String, Value>, HistoryRepairError> {
    if snapshot.history.count() != snapshot.metadata.count() {
        return Err(HistoryRepairError::new("replacement_metadata_invalid"));
    }
    let mut payload = snapshot.latest_checkpoint.take().unwrap_or_default();
    if let Some(summary) = summary {
        payload.insert("message".to_owned(), json!(summary));
    } else if !payload.contains_key("message") {
        payload.insert("message".to_owned(), json!(""));
    }
    // Keep the checkpoint small; the replacement follows as canonical
    // response_item records immediately after it.
    payload.insert("replacement_history".to_owned(), Value::Array(Vec::new()));
    payload.insert(
        "replacement_history_metadata".to_owned(),
        Value::Array(Vec::new()),
    );
    let previous_window = payload
        .get("window_number")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let next_window = previous_window
        .checked_add(1)
        .ok_or_else(|| HistoryRepairError::new("window_number_overflow"))?;
    let old_window_id = payload
        .get("window_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let first_window_id = payload
        .get("first_window_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| old_window_id.clone());
    let unique_window_id = make_window_id(rollout, next_window);
    payload.insert("window_number".to_owned(), json!(next_window));
    payload.insert(
        "first_window_id".to_owned(),
        json!(first_window_id.unwrap_or_else(|| unique_window_id.clone())),
    );
    payload.insert(
        "previous_window_id".to_owned(),
        old_window_id.map_or(Value::Null, Value::String),
    );
    payload.insert("window_id".to_owned(), json!(unique_window_id));
    payload.insert("compaction_response_id".to_owned(), Value::Null);
    payload.insert(
        "latest_token_usage_record".to_owned(),
        snapshot.latest_token_usage.take().unwrap_or(Value::Null),
    );
    Ok(Map::from_iter([
        ("timestamp".to_owned(), json!(rollout_timestamp()?)),
        ("type".to_owned(), json!("compacted")),
        ("payload".to_owned(), Value::Object(payload)),
    ]))
}

fn rollout_timestamp() -> Result<String, HistoryRepairError> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| HistoryRepairError::new("timestamp_unavailable"))
}

fn make_window_id(rollout: &RolloutSummary, window: u64) -> String {
    let mut digest = Sha256::new();
    digest.update(rollout.thread_id.as_bytes());
    digest.update(window.to_le_bytes());
    digest.update(rollout.record_count.to_le_bytes());
    digest.update(rollout.last_record_end.to_le_bytes());
    if let Some(ordinal) = rollout.ordinal {
        digest.update(ordinal.to_le_bytes());
    }
    let bytes = digest.finalize();
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&bytes[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        uuid[0],
        uuid[1],
        uuid[2],
        uuid[3],
        uuid[4],
        uuid[5],
        uuid[6],
        uuid[7],
        uuid[8],
        uuid[9],
        uuid[10],
        uuid[11],
        uuid[12],
        uuid[13],
        uuid[14],
        uuid[15]
    )
}

pub(super) fn write_replacement_rollout(
    rollout: &RolloutSummary,
    snapshot: &mut ContextSnapshot,
    replacement_path: &Path,
) -> Result<usize, HistoryRepairError> {
    if snapshot
        .latest_checkpoint
        .as_ref()
        .is_some_and(map_contains_emp1_encrypted_content)
        || snapshot
            .latest_token_usage
            .as_ref()
            .is_some_and(contains_emp1_encrypted_content)
    {
        return Err(HistoryRepairError::new(
            "unsupported_emp_checkpoint_location",
        ));
    }

    let converted_history = JsonlSpool::create(snapshot.directory.join("converted-history.jsonl"))?;
    let converted_metadata =
        JsonlSpool::create(snapshot.directory.join("converted-metadata.jsonl"))?;
    let mut history = snapshot.history.reader()?;
    let mut metadata = snapshot.metadata.reader()?;
    let mut converted = 0usize;
    let mut latest_summary = None;
    let mut converted_history = converted_history;
    let mut converted_metadata = converted_metadata;
    loop {
        let Some(item) = history.next::<Map<String, Value>>()? else {
            break;
        };
        let metadata = metadata
            .next::<Map<String, Value>>()?
            .ok_or_else(|| HistoryRepairError::new("replacement_metadata_invalid"))?;
        if map_contains_emp1_encrypted_content(&metadata) {
            return Err(HistoryRepairError::new(
                "unsupported_emp_checkpoint_location",
            ));
        }
        let (item, summary) = convert_emp_item(item)?;
        if summary.is_some() {
            converted = converted.saturating_add(1);
            latest_summary = summary;
        }
        converted_history.append(&item)?;
        converted_metadata.append(&metadata)?;
    }
    if metadata.next::<Map<String, Value>>()?.is_some()
        || converted_history.count() != converted_metadata.count()
    {
        return Err(HistoryRepairError::new("replacement_metadata_invalid"));
    }
    if converted == 0 {
        return Ok(0);
    }

    let mut checkpoint = replacement_checkpoint(rollout, snapshot, latest_summary.as_deref())?;
    let mut output = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(replacement_path)
            .map_err(|_| HistoryRepairError::new("repair_stage_unavailable"))?,
    );
    if rollout.compressed {
        let mut encoder = zstd::stream::write::Encoder::new(output, 3)
            .map_err(|_| HistoryRepairError::new("compressed_rollout_write_failed"))?;
        copy_original_rollout(rollout, &mut encoder)?;
        append_replacement_records(
            rollout,
            &mut snapshot.tail_records,
            converted_history,
            converted_metadata,
            &mut checkpoint,
            &mut encoder,
        )?;
        output = encoder
            .finish()
            .map_err(|_| HistoryRepairError::new("compressed_rollout_write_failed"))?;
    } else {
        copy_original_rollout(rollout, &mut output)?;
        append_replacement_records(
            rollout,
            &mut snapshot.tail_records,
            converted_history,
            converted_metadata,
            &mut checkpoint,
            &mut output,
        )?;
    }
    output
        .flush()
        .and_then(|()| output.get_ref().sync_all())
        .map_err(|_| HistoryRepairError::new("repair_stage_unavailable"))?;
    Ok(converted)
}

fn copy_original_rollout(
    rollout: &RolloutSummary,
    output: &mut impl Write,
) -> Result<(), HistoryRepairError> {
    let before = file_fingerprint(&rollout.path)
        .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    if before != (rollout.original_bytes, rollout.original_sha256.clone()) {
        return Err(HistoryRepairError::new("repair_stale_snapshot"));
    }
    let raw =
        File::open(&rollout.path).map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    if rollout.compressed {
        let mut decoder = zstd::stream::read::Decoder::new(raw)
            .map_err(|_| HistoryRepairError::new("compressed_rollout_invalid"))?;
        std::io::copy(&mut decoder, output)
            .map_err(|_| HistoryRepairError::new("compressed_rollout_write_failed"))?;
    } else {
        let mut raw = raw;
        std::io::copy(&mut raw, output)
            .map_err(|_| HistoryRepairError::new("repair_stage_unavailable"))?;
    }
    let after = file_fingerprint(&rollout.path)
        .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    if after != before {
        return Err(HistoryRepairError::new("repair_stale_snapshot"));
    }
    Ok(())
}

fn append_replacement_records(
    rollout: &RolloutSummary,
    tail_records: &mut JsonlSpool,
    mut history: JsonlSpool,
    mut metadata: JsonlSpool,
    checkpoint: &mut Map<String, Value>,
    output: &mut impl Write,
) -> Result<(), HistoryRepairError> {
    let mut next_ordinal = if rollout.mode == "paginated" {
        Some(
            rollout
                .ordinal
                .ok_or_else(|| HistoryRepairError::new("paginated_ordinal_missing"))?
                .checked_add(1)
                .ok_or_else(|| HistoryRepairError::new("paginated_ordinal_overflow"))?,
        )
    } else {
        None
    };
    append_record(output, checkpoint, &mut next_ordinal)?;
    let mut history_reader = history.reader()?;
    let mut metadata_reader = metadata.reader()?;
    loop {
        let Some(item) = history_reader.next::<Map<String, Value>>()? else {
            break;
        };
        let item_metadata = metadata_reader
            .next::<Map<String, Value>>()?
            .ok_or_else(|| HistoryRepairError::new("replacement_metadata_invalid"))?;
        let mut record = Map::from_iter([
            ("type".to_owned(), json!("response_item")),
            ("payload".to_owned(), Value::Object(item)),
            ("metadata".to_owned(), Value::Object(item_metadata)),
        ]);
        append_record(output, &mut record, &mut next_ordinal)?;
    }
    if metadata_reader.next::<Map<String, Value>>()?.is_some() {
        return Err(HistoryRepairError::new("replacement_metadata_invalid"));
    }
    let mut tail_reader = tail_records.reader()?;
    while let Some(mut record) = tail_reader.next::<Map<String, Value>>()? {
        append_record(output, &mut record, &mut next_ordinal)?;
    }
    Ok(())
}

fn append_record(
    output: &mut impl Write,
    record: &mut Map<String, Value>,
    next_ordinal: &mut Option<u64>,
) -> Result<(), HistoryRepairError> {
    match record.get("timestamp") {
        Some(Value::String(_)) => {}
        Some(_) => return Err(HistoryRepairError::new("rollout_timestamp_invalid")),
        None => {
            record.insert("timestamp".to_owned(), json!(rollout_timestamp()?));
        }
    }
    if let Some(ordinal) = next_ordinal.as_mut() {
        record.insert("ordinal".to_owned(), json!(*ordinal));
        *ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| HistoryRepairError::new("paginated_ordinal_overflow"))?;
    }
    let mut limited = LineLimitedWriter::new(output, MAX_ROLLOUT_LINE_BYTES);
    serde_json::to_writer(&mut limited, &Value::Object(std::mem::take(record))).map_err(
        |error| {
            if limited.too_large {
                HistoryRepairError::new("rollout_record_too_large")
            } else if error.is_io() {
                HistoryRepairError::new("repair_stage_unavailable")
            } else {
                HistoryRepairError::new("checkpoint_serialize_failed")
            }
        },
    )?;
    limited.write_all(b"\n").map_err(|_| {
        if limited.too_large {
            HistoryRepairError::new("rollout_record_too_large")
        } else {
            HistoryRepairError::new("repair_stage_unavailable")
        }
    })
}

struct LineLimitedWriter<'a, W> {
    output: &'a mut W,
    remaining: usize,
    too_large: bool,
}

impl<'a, W> LineLimitedWriter<'a, W> {
    fn new(output: &'a mut W, limit: usize) -> Self {
        Self {
            output,
            remaining: limit,
            too_large: false,
        }
    }
}

impl<W: Write> Write for LineLimitedWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.remaining {
            self.too_large = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "serialized rollout record exceeds the line limit",
            ));
        }
        let written = self.output.write(buffer)?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
