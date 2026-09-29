use super::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};

pub(super) struct JsonlSpool {
    path: PathBuf,
    writer: BufWriter<File>,
    count: u64,
}

impl JsonlSpool {
    pub(super) fn create(path: PathBuf) -> Result<Self, HistoryRepairError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            count: 0,
        })
    }

    pub(super) fn append<T: Serialize>(&mut self, value: &T) -> Result<(), HistoryRepairError> {
        serde_json::to_writer(&mut self.writer, value)
            .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
        self.writer
            .write_all(b"\n")
            .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| HistoryRepairError::new("repair_spool_unavailable"))?;
        Ok(())
    }

    pub(super) fn reset(&mut self) -> Result<(), HistoryRepairError> {
        self.writer
            .flush()
            .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
        let file = self.writer.get_mut();
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
        self.count = 0;
        Ok(())
    }

    pub(super) fn count(&self) -> u64 {
        self.count
    }

    pub(super) fn reader(&mut self) -> Result<JsonlSpoolReader, HistoryRepairError> {
        self.writer
            .flush()
            .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
        let file = File::open(&self.path)
            .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
        Ok(JsonlSpoolReader {
            reader: BufReader::new(file),
            line: Vec::new(),
        })
    }
}

pub(super) struct JsonlSpoolReader {
    reader: BufReader<File>,
    line: Vec<u8>,
}

impl JsonlSpoolReader {
    pub(super) fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, HistoryRepairError> {
        self.line.clear();
        loop {
            let available = self
                .reader
                .fill_buf()
                .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
            if available.is_empty() {
                if self.line.is_empty() {
                    return Ok(None);
                }
                return Err(HistoryRepairError::new("repair_spool_invalid"));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |position| position + 1);
            if self.line.len().saturating_add(take) > MAX_ROLLOUT_LINE_BYTES {
                return Err(HistoryRepairError::new("rollout_record_too_large"));
            }
            self.line.extend_from_slice(&available[..take]);
            self.reader.consume(take);
            if newline.is_some() {
                break;
            }
        }
        serde_json::from_slice(&self.line)
            .map(Some)
            .map_err(|_| HistoryRepairError::new("repair_spool_invalid"))
    }
}

pub(super) struct TurnStore {
    connection: Connection,
}

impl TurnStore {
    pub(super) fn open(path: &Path) -> Result<Self, HistoryRepairError> {
        let connection = Connection::open(path)
            .map_err(|_| HistoryRepairError::new("turn_lifecycle_store_unavailable"))?;
        connection
            .execute_batch(
                "PRAGMA cache_size=-2048;
                 PRAGMA mmap_size=0;
                 PRAGMA temp_store=FILE;
                 PRAGMA journal_mode=OFF;
                 PRAGMA synchronous=OFF;
                 CREATE TABLE turns (id TEXT PRIMARY KEY, state INTEGER NOT NULL);",
            )
            .map_err(|_| HistoryRepairError::new("turn_lifecycle_store_unavailable"))?;
        Ok(Self { connection })
    }

    pub(super) fn start(&mut self, turn: &str) -> Result<(), HistoryRepairError> {
        let state = self
            .connection
            .query_row("SELECT state FROM turns WHERE id = ?1", [turn], |row| {
                row.get::<_, i64>(0)
            })
            .optional()
            .map_err(|_| HistoryRepairError::new("turn_lifecycle_store_unavailable"))?;
        match state {
            Some(2) => Ok(()),
            Some(_) => Err(HistoryRepairError::new("turn_lifecycle_invalid")),
            None => {
                self.connection
                    .execute("INSERT INTO turns (id, state) VALUES (?1, 1)", [turn])
                    .map_err(|_| HistoryRepairError::new("turn_lifecycle_store_unavailable"))?;
                Ok(())
            }
        }
    }

    pub(super) fn complete(&mut self, turn: &str) -> Result<(), HistoryRepairError> {
        let changed = self
            .connection
            .execute(
                "UPDATE turns SET state = 2 WHERE id = ?1 AND state = 1",
                [turn],
            )
            .map_err(|_| HistoryRepairError::new("turn_lifecycle_store_unavailable"))?;
        if changed != 0 {
            return Ok(());
        }
        let completed = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM turns WHERE id = ?1 AND state = 2)",
                [turn],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|_| HistoryRepairError::new("turn_lifecycle_store_unavailable"))?;
        if completed {
            Ok(())
        } else {
            Err(HistoryRepairError::new("turn_lifecycle_invalid"))
        }
    }

    pub(super) fn has_open_turns(&self) -> Result<bool, HistoryRepairError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM turns WHERE state = 1)",
                params![],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|_| HistoryRepairError::new("turn_lifecycle_store_unavailable"))
    }
}
