//! Reusable timestamped report facts, independent of filters and chart buckets.
use super::*;

#[derive(Default)]
pub(super) struct ReadCache {
    pub calls: Option<CachedRows>,
    pub usage: Option<CachedRows>,
}

pub(super) struct CachedRows {
    connection: Connection,
    range: Option<(f64, f64)>,
    version: i64,
}

impl CachedRows {
    pub fn select<'a>(
        slot: &'a mut Option<Self>,
        ledger: &UsageLedger,
        source: &str,
        timestamp: &str,
        range: std::ops::Range<f64>,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<QueryConnection<'a>> {
        let std::ops::Range { start, end } = range;
        if slot.is_none() {
            let connection = ledger.connect()?;
            // Keep one disk-backed window per projection, not a RAM copy per filter.
            connection.execute_batch("PRAGMA temp_store=FILE; PRAGMA temp.cache_size=-2048")?;
            *slot = Some(Self {
                connection,
                range: None,
                version: 0,
            });
        }
        let cached = slot.as_mut().unwrap();
        let connection = QueryConnection::new(&cached.connection, cancelled)?;
        let version = connection.query_row("PRAGMA data_version", [], |row| row.get(0))?;
        let range = cached.range.filter(|_| version == cached.version);
        if let Some((from, until)) = range
            && from <= start
            && end <= until
        {
            return Ok(connection);
        }
        cached.range = None;
        let transaction = connection.unchecked_transaction()?;
        let range = match range.filter(|(from, until)| start <= *until && *from <= end) {
            Some((from, until)) => {
                let width = (until - from).max(end - start);
                let range = if end > until {
                    (end - width, end)
                } else {
                    (start, start + width)
                };
                transaction.execute(
                    &format!("DELETE FROM cached_rows WHERE {timestamp} < ? OR {timestamp} >= ?"),
                    params![range.0, range.1],
                )?;
                transaction.execute(
                    &format!("INSERT INTO cached_rows SELECT * FROM {source} WHERE {timestamp} >= ? AND {timestamp} < ? AND ({timestamp} < ? OR {timestamp} >= ?)"),
                    params![range.0, range.1, from, until],
                )?;
                range
            }
            None => {
                transaction.execute_batch("DROP TABLE IF EXISTS cached_rows")?;
                transaction.execute(
                    &format!("CREATE TEMP TABLE cached_rows AS SELECT * FROM {source} WHERE {timestamp} >= ? AND {timestamp} < ?"),
                    params![start, end],
                )?;
                transaction.execute_batch(&format!(
                    "CREATE INDEX cached_rows_time ON cached_rows({timestamp})"
                ))?;
                (start, end)
            }
        };
        transaction.commit()?;
        cached.range = Some(range);
        cached.version = version;
        Ok(connection)
    }
}

/// The socket owns the cancellation flag; a report only observes it while executing SQL.
pub(super) struct QueryConnection<'a>(&'a Connection);

impl<'a> QueryConnection<'a> {
    pub fn new(connection: &'a Connection, cancelled: Option<Arc<AtomicBool>>) -> Result<Self> {
        if let Some(cancelled) = cancelled {
            if cancelled.load(Ordering::Acquire) {
                return Err(UsageError);
            }
            connection.progress_handler(10_000, Some(move || cancelled.load(Ordering::Acquire)))?;
        }
        Ok(Self(connection))
    }
}

impl std::ops::Deref for QueryConnection<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.0
    }
}

impl Drop for QueryConnection<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_materialization_rolls_back_and_releases_the_cached_connection() {
        let root = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::new(
            root.path().join("usage.db"),
            Arc::new(PriceCatalog::new(root.path().join("prices.json"), 0.0)),
        );
        let connection = Connection::open_in_memory().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = cancelled.clone();
        let observed = reads.clone();
        connection
            .create_scalar_function(
                "read_row",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                move |_| {
                    if observed.fetch_add(1, Ordering::Relaxed) == 50 {
                        stop.store(true, Ordering::Release);
                    }
                    Ok(1)
                },
            )
            .unwrap();
        let mut cache = Some(CachedRows {
            connection,
            range: None,
            version: 0,
        });
        let source = "(WITH RECURSIVE data(at) AS (VALUES(1) UNION ALL SELECT at+1 FROM data WHERE at<100000) SELECT at,read_row() AS value FROM data)";
        assert!(
            CachedRows::select(
                &mut cache,
                &ledger,
                source,
                "at",
                0.0..100001.0,
                Some(cancelled)
            )
            .is_err()
        );
        assert!(
            reads.load(Ordering::Relaxed) < 100000,
            "cancelled queries must stop reading, not just discard their result"
        );
        assert!(cache.as_ref().unwrap().range.is_none());
        let connection =
            CachedRows::select(&mut cache, &ledger, source, "at", 0.0..100001.0, None).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM cached_rows", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            100000
        );
    }

    #[test]
    fn narrower_windows_read_cached_facts_and_extensions_read_only_missing_rows() {
        let root = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::new(
            root.path().join("usage.db"),
            Arc::new(PriceCatalog::new(root.path().join("prices.json"), 0.0)),
        );
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE source(at REAL); INSERT INTO source VALUES (1),(10),(29),(30),(39)",
            )
            .unwrap();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let measured = reads.clone();
        connection
            .create_scalar_function(
                "read_row",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                move |_| {
                    measured.fetch_add(1, Ordering::Relaxed);
                    Ok(1)
                },
            )
            .unwrap();
        let mut cache = Some(CachedRows {
            connection,
            range: None,
            version: 0,
        });
        let source = "(SELECT at,read_row() AS value FROM source)";
        for (start, end, expected) in [
            (0.0, 30.0, 3),
            (23.0, 30.0, 3),
            (29.0, 30.0, 3),
            (29.9, 30.0, 3),
            (29.0, 40.0, 5),
            (0.0, 40.0, 6),
        ] {
            CachedRows::select(&mut cache, &ledger, source, "at", start..end, None).unwrap();
            assert_eq!(reads.load(Ordering::Relaxed), expected);
        }
        let count: i64 = cache
            .as_ref()
            .unwrap()
            .connection
            .query_row("SELECT COUNT(*) FROM cached_rows", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 5);
    }
}
