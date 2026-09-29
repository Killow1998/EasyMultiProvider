use super::*;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerState {
    Clear,
    Present,
    Unknown,
}

#[derive(Clone, Copy)]
enum MarkerAction {
    Reset(MarkerState),
    Add,
}

#[derive(Clone, Copy)]
struct MarkerTransform([MarkerState; 3]);

impl MarkerTransform {
    fn identity() -> Self {
        Self([
            MarkerState::Clear,
            MarkerState::Present,
            MarkerState::Unknown,
        ])
    }

    fn apply(self, state: MarkerState) -> MarkerState {
        self.0[state_index(state)]
    }

    fn then(self, action: MarkerAction) -> Self {
        Self(self.0.map(|state| apply_action(action, state)))
    }
}

struct RolloutIndex {
    path: PathBuf,
    mode: String,
    history_base: Option<HistoryBase>,
    base_uncertain: bool,
    pagination_valid: bool,
    prefix: MarkerTransform,
}

struct MarkerScanner<'a> {
    home: &'a Path,
    roots: &'a [PathBuf],
    indexes: HashMap<String, RolloutIndex>,
}

impl<'a> MarkerScanner<'a> {
    fn new(home: &'a Path, roots: &'a [PathBuf]) -> Self {
        Self {
            home,
            roots,
            indexes: HashMap::new(),
        }
    }

    fn index_rollout(
        &mut self,
        info: &ThreadInfo,
        path: PathBuf,
    ) -> Result<RolloutIndex, HistoryRepairError> {
        scan_rollout(info, path, None)
    }

    fn marker_state_for(
        &mut self,
        thread_id: &str,
        bound: Option<HistoryBound>,
    ) -> Result<MarkerState, HistoryRepairError> {
        self.marker_state_at(thread_id, bound, &mut Vec::new(), &mut HashMap::new())
    }

    fn marker_state_at(
        &mut self,
        thread_id: &str,
        bound: Option<HistoryBound>,
        visiting: &mut Vec<String>,
        cache: &mut HashMap<(String, Option<u64>, u64), MarkerState>,
    ) -> Result<MarkerState, HistoryRepairError> {
        let key = (
            thread_id.to_owned(),
            bound.map(|value| value.ordinal_exclusive),
            bound.map_or(0, |value| value.byte_offset),
        );
        if let Some(state) = cache.get(&key) {
            return Ok(*state);
        }
        if visiting.len() >= MAX_LINEAGE_DEPTH || visiting.iter().any(|id| id == thread_id) {
            return Ok(MarkerState::Unknown);
        }
        if !self.indexes.contains_key(thread_id) {
            let path = match find_rollout_for_id(self.home, thread_id, self.roots) {
                Ok(path) => path,
                Err(error) if error.reason() == "lineage_source_missing" => {
                    return Ok(MarkerState::Unknown);
                }
                Err(error) => return Err(error),
            };
            let info = ThreadInfo {
                id: thread_id.to_owned(),
                path: path.clone(),
                mode: None,
            };
            let index = self.index_rollout(&info, path)?;
            self.indexes.insert(thread_id.to_owned(), index);
        }

        visiting.push(thread_id.to_owned());
        let index = self
            .indexes
            .get(thread_id)
            .ok_or_else(|| HistoryRepairError::new("lineage_source_missing"))?;
        let mode = index.mode.clone();
        let base = index.history_base.clone();
        let base_uncertain = index.base_uncertain;
        let local = if let Some(bound) = bound {
            if index.mode != "paginated" || !index.pagination_valid {
                MarkerTransform([MarkerState::Unknown; 3])
            } else {
                let path = index.path.clone();
                let info = ThreadInfo {
                    id: thread_id.to_owned(),
                    path: path.clone(),
                    mode: Some(mode.clone()),
                };
                scan_rollout(&info, path, Some(bound))?.prefix
            }
        } else {
            index.prefix
        };
        let inherited = if base_uncertain {
            MarkerState::Unknown
        } else if let Some(base) = base {
            if mode != "paginated" {
                MarkerState::Unknown
            } else {
                self.marker_state_at(
                    &base.thread_id,
                    Some(HistoryBound {
                        ordinal_exclusive: base.end_ordinal_exclusive,
                        byte_offset: base.end_byte_offset,
                    }),
                    visiting,
                    cache,
                )?
            }
        } else {
            MarkerState::Clear
        };
        visiting.pop();
        let state = local.apply(inherited);
        cache.insert(key, state);
        Ok(state)
    }
}

pub(super) fn select_affected_rollouts(
    home: &Path,
    infos: &[ThreadInfo],
) -> Result<Vec<ThreadInfo>, HistoryRepairError> {
    if infos.len() > MAX_REPAIR_THREADS {
        return Err(HistoryRepairError::new("repair_thread_limit_exceeded"));
    }
    let roots = session_roots(home)?;
    let mut scanner = MarkerScanner::new(home, &roots);
    let mut seen_paths = BTreeSet::new();

    for info in infos {
        let path = resolve_rollout_path(home, info, &roots)?;
        let index = scanner.index_rollout(info, path)?;
        if !seen_paths.insert(index.path.clone()) {
            return Err(HistoryRepairError::new("rollout_path_conflict"));
        }
        if scanner.indexes.insert(info.id.clone(), index).is_some() {
            return Err(HistoryRepairError::new("thread_identity_conflict"));
        }
    }

    let mut affected = Vec::new();
    for info in infos {
        let state = scanner.marker_state_for(&info.id, None)?;
        if state != MarkerState::Clear {
            affected.push(info.clone());
        }
    }

    Ok(affected)
}

fn scan_rollout(
    info: &ThreadInfo,
    path: PathBuf,
    bound: Option<HistoryBound>,
) -> Result<RolloutIndex, HistoryRepairError> {
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| HistoryRepairError::new("rollout_missing"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HistoryRepairError::new("rollout_path_unsafe"));
    }
    let compressed = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"));
    let mut probe =
        File::open(&path).map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    let mut magic = [0u8; 4];
    let magic_len = probe
        .read(&mut magic)
        .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    let has_zstd_magic = magic_len == magic.len() && magic == [0x28, 0xb5, 0x2f, 0xfd];
    if compressed != has_zstd_magic {
        return Err(HistoryRepairError::new("rollout_compression_mismatch"));
    }

    let raw = File::open(&path).map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
    let source: Box<dyn Read> = if compressed {
        Box::new(
            zstd::stream::read::Decoder::new(raw)
                .map_err(|_| HistoryRepairError::new("compressed_rollout_invalid"))?,
        )
    } else {
        Box::new(raw)
    };
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();
    let mut file_bytes = 0u64;
    let mut first = true;
    let mut mode = info.mode.clone().unwrap_or_else(|| "legacy".to_owned());
    let mut mode_conflict = false;
    let mut history_base = None;
    let mut base_uncertain = false;
    let mut pagination_valid = true;
    let mut last_ordinal = None;
    let mut prefix = MarkerTransform::identity();

    loop {
        let Some(terminated) = read_bounded_line(&mut reader, &mut line, &mut file_bytes)? else {
            break;
        };
        if !terminated {
            return Err(HistoryRepairError::new("rollout_incomplete"));
        }
        let end_offset = file_bytes;
        let body = line.strip_suffix(b"\n").unwrap_or(&line);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        if body.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value = serde_json::from_slice::<Value>(body)
            .map_err(|_| HistoryRepairError::new("rollout_json_invalid"))?;
        let Some(record) = value.as_object() else {
            if first {
                return Err(HistoryRepairError::new("session_meta_missing"));
            }
            if mode == "paginated" {
                pagination_valid = false;
            }
            if contains_emp_marker(&value) && bound.is_none() {
                prefix = prefix.then(MarkerAction::Add);
            }
            continue;
        };
        let ordinal = record.get("ordinal").and_then(Value::as_u64);
        if first {
            if token(record.get("type")) != "session_meta"
                || session_meta_id(record).as_deref() != Some(info.id.as_str())
            {
                return Err(HistoryRepairError::new("thread_identity_mismatch"));
            }
            let meta_mode = match session_meta_history_mode(record) {
                Ok(mode) => mode,
                Err(_) => {
                    mode_conflict = true;
                    None
                }
            };
            if let (Some(database_mode), Some(rollout_mode)) = (&info.mode, &meta_mode)
                && database_mode != rollout_mode
            {
                mode_conflict = true;
            }
            mode = info
                .mode
                .clone()
                .or(meta_mode)
                .unwrap_or_else(|| "legacy".to_owned());
            match session_meta_history_base(record) {
                Ok(base) => history_base = base,
                Err(_) => base_uncertain = true,
            }
            if mode == "paginated" {
                update_pagination_state(ordinal, &mut last_ordinal, &mut pagination_valid);
            }
            first = false;
            continue;
        }
        if mode == "paginated" {
            update_pagination_state(ordinal, &mut last_ordinal, &mut pagination_valid);
        }
        let within_bound = bound.is_none_or(|limit| {
            ordinal.is_some_and(|value| value < limit.ordinal_exclusive)
                && (limit.byte_offset == 0 || end_offset <= limit.byte_offset)
        });
        if within_bound && let Some(action) = marker_action(record, &value) {
            prefix = prefix.then(action);
        }
    }
    if first {
        return Err(HistoryRepairError::new("session_meta_missing"));
    }
    if mode_conflict && history_base.is_some() {
        base_uncertain = true;
    }
    Ok(RolloutIndex {
        path,
        mode,
        history_base,
        base_uncertain,
        pagination_valid,
        prefix,
    })
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    file_bytes: &mut u64,
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
        let next_line_len = line.len().saturating_add(take);
        if next_line_len > MAX_ROLLOUT_LINE_BYTES {
            return Err(HistoryRepairError::new("rollout_record_too_large"));
        }
        let take_u64 = take as u64;
        let next_file_bytes = file_bytes
            .checked_add(take_u64)
            .ok_or_else(|| HistoryRepairError::new("rollout_too_large"))?;
        line.extend_from_slice(&available[..take]);
        *file_bytes = next_file_bytes;
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(true));
        }
    }
}

fn marker_action(record: &Map<String, Value>, value: &Value) -> Option<MarkerAction> {
    if token(record.get("type")) == "compacted"
        && let Some(items) = record
            .get("payload")
            .and_then(Value::as_object)
            .unwrap_or(record)
            .get("replacement_history")
            .and_then(Value::as_array)
        && items.iter().all(Value::is_object)
    {
        let has_marker = items.iter().any(contains_emp_marker);
        return Some(MarkerAction::Reset(if has_marker {
            MarkerState::Present
        } else {
            MarkerState::Clear
        }));
    }
    contains_emp_marker(value).then_some(MarkerAction::Add)
}

fn contains_emp_marker(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.get("encrypted_content").is_some_and(|content| {
                content
                    .as_str()
                    .is_some_and(|encoded| encoded.starts_with("emp1:"))
            }) || object.values().any(contains_emp_marker)
        }
        Value::Array(items) => items.iter().any(contains_emp_marker),
        _ => false,
    }
}

fn update_pagination_state(ordinal: Option<u64>, last_ordinal: &mut Option<u64>, valid: &mut bool) {
    match ordinal {
        Some(ordinal) if last_ordinal.is_none_or(|last| ordinal > last) => {
            *last_ordinal = Some(ordinal);
        }
        _ => *valid = false,
    }
}

fn state_index(state: MarkerState) -> usize {
    match state {
        MarkerState::Clear => 0,
        MarkerState::Present => 1,
        MarkerState::Unknown => 2,
    }
}

fn apply_action(action: MarkerAction, state: MarkerState) -> MarkerState {
    match action {
        MarkerAction::Reset(state) => state,
        MarkerAction::Add => match state {
            MarkerState::Clear | MarkerState::Present => MarkerState::Present,
            MarkerState::Unknown => MarkerState::Unknown,
        },
    }
}
