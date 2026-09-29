//! Streaming rollout parsing and lineage replay for legacy history repair.

use super::{
    ContextSnapshot, HistoryBound, HistoryRepairError, MAX_LINEAGE_DEPTH, ThreadInfo,
    file_fingerprint, find_rollout_for_id, resolve_rollout_path, session_roots,
};
use crate::history::{
    HistoryBase, MAX_ROLLOUT_LINE_BYTES, session_meta_history_base, session_meta_history_mode,
    session_meta_id, token,
};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

struct RolloutHeader {
    thread_id: String,
    path: PathBuf,
    compressed: bool,
    mode: String,
    history_base: Option<HistoryBase>,
    original_bytes: u64,
}

pub(super) struct RolloutSummary {
    pub(super) thread_id: String,
    pub(super) path: PathBuf,
    pub(super) compressed: bool,
    pub(super) mode: String,
    pub(super) original_sha256: String,
    pub(super) original_bytes: u64,
    pub(super) record_count: u64,
    pub(super) last_record_end: u64,
    pub(super) ordinal: Option<u64>,
}

fn load_rollout_header(
    home: &Path,
    info: &ThreadInfo,
) -> Result<RolloutHeader, HistoryRepairError> {
    let roots = session_roots(home)?;
    let path = resolve_rollout_path(home, info, &roots)?;
    let (source, compressed, original_bytes) = open_rollout_reader(&path)?;
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();
    let terminated = read_repair_line(&mut reader, &mut line)?
        .ok_or_else(|| HistoryRepairError::new("session_meta_missing"))?;
    if !terminated {
        return Err(HistoryRepairError::new("rollout_incomplete"));
    }
    let record = parse_repair_record(&line)?
        .ok_or_else(|| HistoryRepairError::new("session_meta_missing"))?;
    if token(record.get("type")) != "session_meta"
        || session_meta_id(&record).as_deref() != Some(info.id.as_str())
    {
        return Err(HistoryRepairError::new("thread_identity_mismatch"));
    }
    let meta_mode = session_meta_history_mode(&record)
        .map_err(|_| HistoryRepairError::new("invalid_history_mode"))?;
    if let (Some(database_mode), Some(rollout_mode)) = (&info.mode, &meta_mode)
        && database_mode != rollout_mode
    {
        return Err(HistoryRepairError::new("history_mode_mismatch"));
    }
    let mode = info
        .mode
        .clone()
        .or(meta_mode)
        .unwrap_or_else(|| "legacy".to_owned());
    if !matches!(mode.as_str(), "legacy" | "paginated") {
        return Err(HistoryRepairError::new("invalid_history_mode"));
    }
    let history_base = session_meta_history_base(&record)
        .map_err(|_| HistoryRepairError::new("invalid_history_base"))?;
    Ok(RolloutHeader {
        thread_id: info.id.clone(),
        path,
        compressed,
        mode,
        history_base,
        original_bytes,
    })
}

fn open_rollout_reader(path: &Path) -> Result<(Box<dyn Read>, bool, u64), HistoryRepairError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| HistoryRepairError::new("rollout_missing"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HistoryRepairError::new("rollout_path_unsafe"));
    }
    let compressed = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"));
    let mut probe = File::open(path).map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    let mut magic = [0u8; 4];
    let magic_len = probe
        .read(&mut magic)
        .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    let has_zstd_magic = magic_len == magic.len() && magic == [0x28, 0xb5, 0x2f, 0xfd];
    if compressed != has_zstd_magic {
        return Err(HistoryRepairError::new("rollout_compression_mismatch"));
    }
    let raw = File::open(path).map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    let source: Box<dyn Read> = if compressed {
        Box::new(
            zstd::stream::read::Decoder::new(raw)
                .map_err(|_| HistoryRepairError::new("compressed_rollout_invalid"))?,
        )
    } else {
        Box::new(raw)
    };
    Ok((source, compressed, metadata.len()))
}

fn read_repair_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> Result<Option<bool>, HistoryRepairError> {
    line.clear();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(false))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > MAX_ROLLOUT_LINE_BYTES {
            return Err(HistoryRepairError::new("rollout_record_too_large"));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(true));
        }
    }
}

fn parse_repair_record(line: &[u8]) -> Result<Option<Map<String, Value>>, HistoryRepairError> {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    let body = body.strip_suffix(b"\r").unwrap_or(body);
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value = serde_json::from_slice::<Value>(body)
        .map_err(|_| HistoryRepairError::new("rollout_json_invalid"))?;
    match value {
        Value::Object(record) => Ok(Some(record)),
        _ => Err(HistoryRepairError::new("rollout_record_invalid")),
    }
}

pub(super) fn context_for(
    home: &Path,
    snapshot: &mut ContextSnapshot,
    info: &ThreadInfo,
    bound: Option<HistoryBound>,
    visiting: &mut Vec<String>,
) -> Result<RolloutSummary, HistoryRepairError> {
    let thread_id = info.id.as_str();
    if visiting.len() >= MAX_LINEAGE_DEPTH || visiting.iter().any(|id| id == thread_id) {
        return Err(HistoryRepairError::new("lineage_invalid"));
    }
    visiting.push(thread_id.to_owned());
    let header = load_rollout_header(home, info)?;
    if let Some(base) = &header.history_base {
        if header.mode != "paginated" {
            return Err(HistoryRepairError::new("legacy_lineage_unsupported"));
        }
        let roots = session_roots(home)?;
        let path = find_rollout_for_id(home, &base.thread_id, &roots)?;
        let parent = ThreadInfo {
            id: base.thread_id.clone(),
            path,
            mode: None,
        };
        context_for(
            home,
            snapshot,
            &parent,
            Some(HistoryBound {
                ordinal_exclusive: base.end_ordinal_exclusive,
                byte_offset: base.end_byte_offset,
            }),
            visiting,
        )?;
    }
    let rollout = stream_rollout(&header, snapshot, bound)?;
    if snapshot.turns.has_open_turns()? {
        return Err(HistoryRepairError::new("unfinished_turn_in_history"));
    }
    visiting.pop();
    Ok(rollout)
}

fn stream_rollout(
    header: &RolloutHeader,
    snapshot: &mut ContextSnapshot,
    bound: Option<HistoryBound>,
) -> Result<RolloutSummary, HistoryRepairError> {
    let before = file_fingerprint(&header.path)
        .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    if before.0 != header.original_bytes {
        return Err(HistoryRepairError::new("repair_stale_snapshot"));
    }
    let (source, compressed, original_bytes) = open_rollout_reader(&header.path)?;
    if compressed != header.compressed || original_bytes != header.original_bytes {
        return Err(HistoryRepairError::new("repair_stale_snapshot"));
    }
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();
    let mut first = true;
    let mut file_bytes = 0u64;
    let mut record_count = 0u64;
    let mut last_record_end = 0u64;
    let mut last_ordinal = None;

    loop {
        let Some(terminated) = read_repair_line(&mut reader, &mut line)? else {
            break;
        };
        if !terminated {
            return Err(HistoryRepairError::new("rollout_incomplete"));
        }
        file_bytes = file_bytes
            .checked_add(line.len() as u64)
            .ok_or_else(|| HistoryRepairError::new("rollout_too_large"))?;
        let end_offset = file_bytes;
        let Some(record) = parse_repair_record(&line)? else {
            continue;
        };
        let ordinal = record.get("ordinal").and_then(Value::as_u64);
        if first {
            if token(record.get("type")) != "session_meta"
                || session_meta_id(&record).as_deref() != Some(header.thread_id.as_str())
            {
                return Err(HistoryRepairError::new("thread_identity_mismatch"));
            }
            let meta_mode = session_meta_history_mode(&record)
                .map_err(|_| HistoryRepairError::new("invalid_history_mode"))?;
            if meta_mode.as_deref().is_some_and(|mode| mode != header.mode) {
                return Err(HistoryRepairError::new("history_mode_mismatch"));
            }
            let history_base = session_meta_history_base(&record)
                .map_err(|_| HistoryRepairError::new("invalid_history_base"))?;
            if history_base != header.history_base {
                return Err(HistoryRepairError::new("repair_stale_snapshot"));
            }
            first = false;
        }
        if header.mode == "paginated" {
            let ordinal =
                ordinal.ok_or_else(|| HistoryRepairError::new("paginated_ordinal_missing"))?;
            if last_ordinal.is_some_and(|last| ordinal <= last) {
                return Err(HistoryRepairError::new("paginated_ordinal_not_monotonic"));
            }
            last_ordinal = Some(ordinal);
        } else {
            last_ordinal = ordinal;
        }
        record_count = record_count
            .checked_add(1)
            .ok_or_else(|| HistoryRepairError::new("rollout_too_large"))?;
        last_record_end = end_offset;
        let in_bound = bound.is_none_or(|limit| {
            ordinal.is_some_and(|value| value < limit.ordinal_exclusive)
                && (limit.byte_offset == 0 || end_offset <= limit.byte_offset)
        });
        if in_bound && token(record.get("type")) != "session_meta" {
            process_record(snapshot, record)?;
        }
    }
    if first || record_count == 0 {
        return Err(HistoryRepairError::new("session_meta_missing"));
    }
    let after = file_fingerprint(&header.path)
        .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    if after != before {
        return Err(HistoryRepairError::new("repair_stale_snapshot"));
    }
    Ok(RolloutSummary {
        thread_id: header.thread_id.clone(),
        path: header.path.clone(),
        compressed: header.compressed,
        mode: header.mode.clone(),
        original_sha256: before.1,
        original_bytes: before.0,
        record_count,
        last_record_end,
        ordinal: last_ordinal,
    })
}

fn process_record(
    snapshot: &mut ContextSnapshot,
    mut record: Map<String, Value>,
) -> Result<(), HistoryRepairError> {
    let kind = token(record.get("type"));
    match kind.as_str() {
        "session_meta"
        | "sessionmeta"
        | "turn_context"
        | "turncontext"
        | "world_state"
        | "token_usage_record"
        | "event_msg"
        | "response_item"
        | "compacted"
        | "retained_context"
        | "security_risk_score"
        | "realtime_item"
        | "inter_agent_communication"
        | "inter_agent_communication_metadata" => {}
        _ => return Err(HistoryRepairError::new("unsupported_rollout_record")),
    }
    match kind.as_str() {
        "compacted" => {
            let mut payload = match record.remove("payload") {
                Some(Value::Object(payload)) => payload,
                _ => return Err(HistoryRepairError::new("compacted_payload_invalid")),
            };
            let replacement = payload
                .get("replacement_history")
                .and_then(Value::as_array)
                .ok_or_else(|| HistoryRepairError::new("replacement_history_missing"))?;
            if replacement.iter().any(|item| !item.is_object()) {
                return Err(HistoryRepairError::new("replacement_history_invalid"));
            }
            let metadata = match payload.get("replacement_history_metadata") {
                Some(Value::Array(items))
                    if items.len() == replacement.len() && items.iter().all(Value::is_object) =>
                {
                    Some(items)
                }
                Some(Value::Null) | None => None,
                _ => return Err(HistoryRepairError::new("replacement_metadata_invalid")),
            };
            snapshot.history.reset()?;
            snapshot.metadata.reset()?;
            for (index, item) in replacement.iter().enumerate() {
                let item = item
                    .as_object()
                    .ok_or_else(|| HistoryRepairError::new("replacement_history_invalid"))?;
                snapshot.history.append(item)?;
                if let Some(metadata) = metadata {
                    snapshot.metadata.append(&metadata[index])?;
                } else {
                    snapshot.metadata.append(&Map::<String, Value>::new())?;
                }
            }
            snapshot.tail_records.reset()?;
            payload.remove("replacement_history");
            payload.remove("replacement_history_metadata");
            snapshot.latest_token_usage = payload
                .remove("latest_token_usage_record")
                .filter(|value| !value.is_null());
            snapshot.latest_checkpoint = Some(payload);
        }
        "response_item" => {
            let payload = match record.remove("payload") {
                Some(Value::Object(payload)) => payload,
                _ => return Err(HistoryRepairError::new("response_item_invalid")),
            };
            let metadata = match record.remove("metadata") {
                Some(Value::Object(metadata)) => Value::Object(metadata),
                Some(Value::Null) | None => Value::Object(Map::new()),
                _ => return Err(HistoryRepairError::new("response_item_metadata_invalid")),
            };
            snapshot.history.append(&payload)?;
            snapshot.metadata.append(&metadata)?;
        }
        "token_usage_record" => {
            let payload = match record.remove("payload") {
                Some(Value::Object(payload)) => payload,
                _ => return Err(HistoryRepairError::new("token_usage_record_invalid")),
            };
            snapshot.latest_token_usage = Some(Value::Object(payload));
        }
        "event_msg" => {
            let payload = record
                .get("payload")
                .and_then(Value::as_object)
                .ok_or_else(|| HistoryRepairError::new("event_msg_invalid"))?;
            process_event(snapshot, Some(payload))?;
            snapshot.tail_records.append(&record)?;
        }
        "turn_context"
        | "turncontext"
        | "world_state"
        | "retained_context"
        | "security_risk_score"
        | "realtime_item"
        | "inter_agent_communication"
        | "inter_agent_communication_metadata" => snapshot.tail_records.append(&record)?,
        "session_meta" | "sessionmeta" => {}
        _ => unreachable!(),
    }
    if snapshot.history.count() != snapshot.metadata.count() {
        return Err(HistoryRepairError::new("replacement_metadata_invalid"));
    }
    Ok(())
}

fn process_event(
    snapshot: &mut ContextSnapshot,
    payload: Option<&Map<String, Value>>,
) -> Result<(), HistoryRepairError> {
    let Some(payload) = payload else {
        return Ok(());
    };
    let event = token(payload.get("type"));
    match event.as_str() {
        "task_started" | "turn_started" => {
            let turn = payload
                .get("turn_id")
                .or_else(|| payload.get("turnId"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| HistoryRepairError::new("turn_identity_missing"))?;
            snapshot.turns.start(turn)?;
        }
        "task_complete" | "turn_complete" => {
            if payload.get("error").is_some_and(|value| !value.is_null()) {
                return Err(HistoryRepairError::new("unsuccessful_turn_in_history"));
            }
            let turn = payload
                .get("turn_id")
                .or_else(|| payload.get("turnId"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| HistoryRepairError::new("turn_identity_missing"))?;
            snapshot.turns.complete(turn)?;
        }
        "thread_rolled_back" | "turn_aborted" | "turn_interrupted" => {
            return Err(HistoryRepairError::new("rollback_or_interrupted_turn"));
        }
        _ => {}
    }
    Ok(())
}
