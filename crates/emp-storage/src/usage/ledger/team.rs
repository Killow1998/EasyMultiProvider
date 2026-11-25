use super::*;
use emp_domain::MAX_USAGE_MODEL_BYTES;

fn report_model(model: &str) -> &str {
    &model[..model.floor_char_boundary(MAX_USAGE_MODEL_BYTES)]
}

impl UsageLedger {
    pub(super) fn insert_team_event(
        transaction: &rusqlite::Transaction<'_>,
        scope: &str,
        share: &str,
        usage: &UsageEntry,
        event: &Value,
        finished: f64,
    ) -> Result<()> {
        let estimate = usage.quote["price_issue"]
            .is_null()
            .then(|| usage.quote["cost_nanos"].as_i64())
            .flatten();
        let payload = json!({
            "id":usage.id, "share":share,
            "model":report_model(text(event,"response_model",text(event,"upstream_model",""))),
            "finished_at":finished.trunc() as i64,
            "input_tokens":token_count(event.get("input_tokens")),
            "cached_input_tokens":token_count(event.get("cached_input_tokens")),
            "output_tokens":token_count(event.get("output_tokens")),
            "estimated_usd_nanos":estimate,
            "actual_usd_nanos":null,
        });
        transaction.execute(
            "INSERT INTO team_outbox VALUES (?,?,?) ON CONFLICT(scope,event_id) DO NOTHING",
            params![scope, usage.id, payload.to_string()],
        )?;
        Ok(())
    }

    pub fn team_events(&self, scope: &str) -> Result<Vec<(String, Value)>> {
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let db = self.connect()?;
        let mut statement = db.prepare(
            "SELECT event_id,payload FROM team_outbox WHERE scope=? ORDER BY rowid LIMIT 100",
        )?;
        let events = statement.query_map([scope], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        events
            .map(|event| {
                let (id, payload) = event?;
                let mut payload: Value = serde_json::from_str(&payload).map_err(|_| UsageError)?;
                if let Some(model) = payload["model"].as_str()
                    && model.len() > MAX_USAGE_MODEL_BYTES
                {
                    payload["model"] = json!(report_model(model));
                }
                Ok((id, payload))
            })
            .collect()
    }

    pub fn ack_team_events(&self, scope: &str, ids: &[&str]) -> Result<()> {
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let mut db = self.connect()?;
        let transaction = db.transaction()?;
        {
            let mut delete =
                transaction.prepare("DELETE FROM team_outbox WHERE scope=? AND event_id=?")?;
            for id in ids {
                delete.execute(params![scope, id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}
