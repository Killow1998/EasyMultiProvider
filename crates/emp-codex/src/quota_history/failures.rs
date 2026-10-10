//! Failed observations explain holes without inventing quota measurements.
use super::*;
use crate::quota::QuotaError;

impl QuotaHistoryStore {
    pub fn append_failure(
        &self,
        owner: &str,
        error: &QuotaError,
        at: i64,
    ) -> Result<(), QuotaHistoryError> {
        if owner.is_empty() {
            return Err(QuotaHistoryError::account_required());
        }
        let _guard = self
            .lock
            .lock()
            .map_err(|_| QuotaHistoryError::unavailable())?;
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|_| QuotaHistoryError::unavailable())?;
        let timestamp = at - at.rem_euclid(SAMPLE_INTERVAL_SECONDS);
        transaction.execute("INSERT OR REPLACE INTO quota_failures (account_key,observed_at,diagnostics) VALUES (?,?,?)",
            params![owner,timestamp,error.diagnostics().to_string()]).map_err(|_| QuotaHistoryError::unavailable())?;
        transaction
            .execute(
                "DELETE FROM quota_failures WHERE observed_at < ?",
                params![timestamp.saturating_sub(RETENTION_SECONDS)],
            )
            .map_err(|_| QuotaHistoryError::unavailable())?;
        transaction
            .commit()
            .map_err(|_| QuotaHistoryError::unavailable())
    }

    pub(super) fn failures(
        connection: &Connection,
        owner: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<Value>, QuotaHistoryError> {
        let mut statement = connection.prepare("SELECT observed_at,diagnostics FROM quota_failures WHERE account_key=? AND observed_at BETWEEN ? AND ? ORDER BY observed_at")
            .map_err(|_| QuotaHistoryError::unavailable())?;
        let mut rows = statement
            .query(params![owner, start, end])
            .map_err(|_| QuotaHistoryError::unavailable())?;
        let mut groups = Vec::<Value>::new();
        while let Some(row) = rows.next().map_err(|_| QuotaHistoryError::unavailable())? {
            let at: i64 = row.get(0).map_err(|_| QuotaHistoryError::unavailable())?;
            let raw: String = row.get(1).map_err(|_| QuotaHistoryError::unavailable())?;
            let details: Value =
                serde_json::from_str(&raw).map_err(|_| QuotaHistoryError::unavailable())?;
            if let Some(last) = groups.last_mut()
                && last["diagnostics"] == details
                && at - last["end_at"].as_i64().unwrap_or(0) <= 3 * SAMPLE_INTERVAL_SECONDS
            {
                last["end_at"] = json!(at);
                last["count"] = json!(last["count"].as_u64().unwrap_or(0) + 1);
            } else {
                groups.push(json!({"start_at":at,"end_at":at,"count":1,"diagnostics":details}));
            }
        }
        // Retain recent grouped incidents within the same bounded response budget.
        if groups.len() > MAX_POINTS_PER_SERIES as usize {
            groups.drain(..groups.len() - MAX_POINTS_PER_SERIES as usize);
        }
        Ok(groups)
    }
}
