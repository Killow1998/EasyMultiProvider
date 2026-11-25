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
        request_id: &str,
        operation: &str,
        event: &Value,
    ) -> Result<()> {
        let model = report_model(text(
            event,
            "response_model",
            text(event, "upstream_model", ""),
        ));
        let (id, finished, from_usage, facts): (String, f64, bool, String) = transaction.query_row(
            "SELECT json_extract(c.facts,'$.usage_event_id'), c.finished_at, u.id IS NOT NULL,
                CASE WHEN u.id IS NULL THEN c.facts ELSE json_object(
                    'input_tokens',u.input_tokens,'cached_input_tokens',u.cached_input_tokens,
                    'output_tokens',u.output_tokens,
                    'team_estimated_usd_nanos',CASE WHEN u.price_issue IS NULL THEN u.cost_nanos END)
                END
             FROM call_records c LEFT JOIN usage_events u ON u.id=json_extract(c.facts,'$.usage_event_id')
             WHERE c.request_id=? AND c.operation=?",
            params![request_id, operation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let facts: Value = serde_json::from_str(&facts).map_err(|_| UsageError)?;
        let tokens = |field| {
            if from_usage {
                facts[field].clone()
            } else {
                json!(token_count(facts.get(field)))
            }
        };
        let payload = json!({
            "id":id, "share":share, "model":model, "finished_at":finished.trunc() as i64,
            "input_tokens":tokens("input_tokens"),
            "cached_input_tokens":tokens("cached_input_tokens"),
            "output_tokens":tokens("output_tokens"),
            "estimated_usd_nanos":facts["team_estimated_usd_nanos"],
            "actual_usd_nanos":null,
        });
        transaction.execute(
            "INSERT INTO team_outbox (scope,event_id,payload) VALUES (?,?,?)
             ON CONFLICT(scope,event_id) DO NOTHING",
            params![scope, id, payload.to_string()],
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
