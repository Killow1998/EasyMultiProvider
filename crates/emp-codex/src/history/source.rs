use super::lines::{
    read_bounded_line_with_reason, rollout_json_line, walk_bounded_reader, walk_records,
};
use super::records::{HistoryBase, session_meta_history_base};
use super::visible::token;
use super::{MAX_ROLLOUT_LINE_BYTES, MAX_ROLLOUT_SCAN_BYTES};
use emp_history::HistoryError;
use serde_json::{Map, Value};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug)]
pub(super) struct WalkReport {
    pub(super) bytes_read: u64,
    pub(super) stopped_early: bool,
}

pub(super) trait RecordSource {
    fn walk(
        &mut self,
        visit: &mut dyn FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
    ) -> Result<WalkReport, HistoryError>;
}

pub(super) struct FileRecordSource<'a> {
    file: &'a mut File,
    start: u64,
    end: u64,
}

impl<'a> FileRecordSource<'a> {
    pub(super) fn new(file: &'a mut File, start: u64, end: u64) -> Self {
        Self { file, start, end }
    }
}

impl RecordSource for FileRecordSource<'_> {
    fn walk(
        &mut self,
        visit: &mut dyn FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
    ) -> Result<WalkReport, HistoryError> {
        self.file
            .seek(SeekFrom::Start(self.start))
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        walk_records(self.file, self.end, visit)
    }
}

pub(super) struct ZstdRecordSource<'a> {
    file: &'a mut File,
    compressed_end: u64,
    uncompressed_limit: u64,
    max_uncompressed_bytes: u64,
    frozen_prefix: Option<u64>,
}

impl<'a> ZstdRecordSource<'a> {
    pub(super) fn new(file: &'a mut File, compressed_end: u64, frozen_prefix: Option<u64>) -> Self {
        Self::with_limits(
            file,
            compressed_end,
            frozen_prefix.unwrap_or(MAX_ROLLOUT_SCAN_BYTES.saturating_add(1)),
            MAX_ROLLOUT_SCAN_BYTES,
            frozen_prefix,
        )
    }

    fn with_limits(
        file: &'a mut File,
        compressed_end: u64,
        uncompressed_limit: u64,
        max_uncompressed_bytes: u64,
        frozen_prefix: Option<u64>,
    ) -> Self {
        Self {
            file,
            compressed_end,
            uncompressed_limit,
            max_uncompressed_bytes,
            frozen_prefix,
        }
    }
}

impl RecordSource for ZstdRecordSource<'_> {
    fn walk(
        &mut self,
        visit: &mut dyn FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
    ) -> Result<WalkReport, HistoryError> {
        if self.compressed_end > MAX_ROLLOUT_SCAN_BYTES {
            return Err(HistoryError::new("source_too_large"));
        }
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        let compressed = self.file.by_ref().take(self.compressed_end);
        let decoder = zstd::stream::read::Decoder::new(compressed)
            .map_err(|_| HistoryError::new("invalid_compressed_rollout"))?;
        let limited = decoder.take(self.uncompressed_limit);
        let mut reader = BufReader::with_capacity(64 * 1024, limited);
        let report = walk_bounded_reader(
            &mut reader,
            self.uncompressed_limit,
            "invalid_compressed_rollout",
            visit,
        )?;
        if !report.stopped_early {
            if report.bytes_read > self.max_uncompressed_bytes {
                return Err(HistoryError::new("source_too_large"));
            }
            if self
                .frozen_prefix
                .is_some_and(|expected| report.bytes_read < expected)
            {
                return Err(HistoryError::new("lineage_prefix_truncated"));
            }
        }
        Ok(report)
    }
}

/// Read the first session metadata record through bounded plain or zstd I/O.
pub(super) fn read_session_meta(path: &Path) -> Result<Option<Map<String, Value>>, HistoryError> {
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(HistoryError::new("source_missing"));
    }
    let mut file = File::open(path).map_err(|_| HistoryError::new("source_missing"))?;
    let captured_end = file
        .metadata()
        .map_err(|_| HistoryError::new("source_unavailable"))?
        .len();
    if is_compressed_rollout(path) {
        if captured_end > MAX_ROLLOUT_SCAN_BYTES {
            return Err(HistoryError::new("source_too_large"));
        }
        let compressed = file.by_ref().take(captured_end);
        let decoder = zstd::stream::read::Decoder::new(compressed)
            .map_err(|_| HistoryError::new("invalid_compressed_rollout"))?;
        let mut reader = BufReader::with_capacity(
            64 * 1024,
            decoder.take((MAX_ROLLOUT_LINE_BYTES as u64).saturating_add(1)),
        );
        return read_session_meta_from(&mut reader, "invalid_compressed_rollout");
    }
    let head = captured_end.min((MAX_ROLLOUT_LINE_BYTES as u64).saturating_add(1));
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(head));
    read_session_meta_from(&mut reader, "source_unavailable")
}

fn read_session_meta_from(
    reader: &mut impl BufRead,
    io_error_reason: &str,
) -> Result<Option<Map<String, Value>>, HistoryError> {
    let mut line = Vec::new();
    while let Some(terminated) = read_bounded_line_with_reason(reader, &mut line, io_error_reason)?
    {
        let Some(record) = rollout_json_line(&line, terminated)? else {
            continue;
        };
        if matches!(
            token(record.get("type")).as_str(),
            "session_meta" | "sessionmeta"
        ) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

pub(super) fn read_history_base(path: &Path) -> Result<Option<HistoryBase>, HistoryError> {
    read_session_meta(path)?
        .as_ref()
        .map(session_meta_history_base)
        .transpose()
        .map(Option::flatten)
}

pub(super) fn is_compressed_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zstd_source_enforces_the_uncompressed_byte_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl.zst");
        let bytes = b"{\"type\":\"event_msg\"}\n".repeat(4);
        let compressed = zstd::stream::encode_all(bytes.as_slice(), 0).unwrap();
        std::fs::write(&path, &compressed).unwrap();
        let mut file = File::open(&path).unwrap();
        let mut source =
            ZstdRecordSource::with_limits(&mut file, compressed.len() as u64, 8, 7, None);
        let error = source.walk(&mut |_| Ok(true)).unwrap_err();
        assert_eq!(error.reason(), "source_too_large");
    }
}
