use super::*;

impl UsageLedger {
    /// Select once per caller/thread/group; the caller supplies currently usable account identities.
    pub fn bind_aggregate(
        &self,
        caller: &str,
        thread: &str,
        model: &str,
        candidates: &[String],
        rotate: bool,
    ) -> Result<usize> {
        use rusqlite::OptionalExtension;
        if candidates.is_empty() {
            return Err(UsageError);
        }
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        let prior: Option<String> = tx
            .query_row(
                "SELECT owner FROM aggregate_bindings WHERE caller=? AND thread=? AND model=?",
                params![caller, thread, model],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(index) = prior
            .as_ref()
            .and_then(|owner| candidates.iter().position(|id| id == owner))
        {
            return Ok(index);
        }
        let cursor: i64 = tx
            .query_row(
                "SELECT next FROM aggregate_cursors WHERE model=?",
                [model],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let index = if rotate {
            (cursor % candidates.len() as i64) as usize
        } else {
            0
        };
        if rotate {
            tx.execute("INSERT INTO aggregate_cursors(model,next) VALUES (?,?) ON CONFLICT(model) DO UPDATE SET next=excluded.next",
                params![model,((index + 1) % candidates.len()) as i64])?;
        }
        tx.execute("INSERT INTO aggregate_bindings(caller,thread,model,owner) VALUES (?,?,?,?) ON CONFLICT(caller,thread,model) DO UPDATE SET owner=excluded.owner",
            params![caller,thread,model,&candidates[index]])?;
        tx.commit()?;
        Ok(index)
    }
}
