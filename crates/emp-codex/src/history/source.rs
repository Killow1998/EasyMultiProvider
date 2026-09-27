use super::*;

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
        super::walk_records(self.file, self.end, visit)
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
        let report = super::walk_bounded_reader(
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
