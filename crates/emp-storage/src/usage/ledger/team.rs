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
        usage_id: &str,
        event: &Value,
        finished: f64,
    ) -> Result<()> {
        let model = report_model(text(
            event,
            "response_model",
            text(event, "upstream_model", ""),
        ));
        transaction.execute(
            "INSERT INTO team_outbox (scope,event_id,payload)
             SELECT ?1,id,json_object(
                'id',id,'share',?2,'model',?3,'finished_at',?4,
                'input_tokens',input_tokens,'cached_input_tokens',cached_input_tokens,
                'output_tokens',output_tokens,
                'estimated_usd_nanos',CASE WHEN price_issue IS NULL THEN cost_nanos END,
                'actual_usd_nanos',NULL)
             FROM usage_events WHERE id=?5
             ON CONFLICT(scope,event_id) DO NOTHING",
            params![scope, share, model, finished.trunc() as i64, usage_id],
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
