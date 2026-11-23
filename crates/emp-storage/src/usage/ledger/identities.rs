//! Durable display metadata keyed by the actual upstream account fingerprint.
use super::*;

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_account_labels (
        category TEXT NOT NULL,owner TEXT NOT NULL,local_id TEXT NOT NULL,name TEXT NOT NULL,
        email TEXT NOT NULL,PRIMARY KEY(category,owner));",
    )?;
    Ok(())
}
impl UsageLedger {
    pub fn remember_account(
        &self,
        category: &str,
        owner: &str,
        id: &str,
        name: &str,
        email: &str,
    ) -> Result<()> {
        if !matches!(category, "native" | "subscription") || !owner.starts_with("account:") {
            return Ok(());
        }
        if [id, name, email]
            .iter()
            .any(|value| value.len() > 512 || value.chars().any(char::is_control))
        {
            return Err(UsageError);
        }
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        self.connect()?.execute(
            "INSERT INTO usage_account_labels VALUES (?,?,?,?,?)
          ON CONFLICT(category,owner) DO UPDATE SET local_id=excluded.local_id,name=excluded.name,
          email=CASE WHEN excluded.email!='' THEN excluded.email ELSE usage_account_labels.email END
          WHERE local_id!=excluded.local_id OR name!=excluded.name
             OR (excluded.email!='' AND email!=excluded.email)",
            params![category, owner, id, name, email],
        )?;
        Ok(())
    }
    pub fn account_labels(&self) -> Result<Vec<Value>> {
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        rows(
            &self.connect()?,
            "SELECT category,owner,local_id,name,email FROM usage_account_labels",
            &[],
        )
    }
}
