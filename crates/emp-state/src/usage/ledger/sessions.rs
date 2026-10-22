//! Session attribution indexes existing accounting events without recounting them.
use super::*;

pub struct SessionBillPage {
    pub offset: usize,
    pub limit: usize,
    pub sort: String,
    pub matching_sessions: Option<Vec<String>>,
}

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_sessions (
            event_id TEXT NOT NULL, session_id TEXT NOT NULL,
            PRIMARY KEY(event_id,session_id));
         CREATE INDEX IF NOT EXISTS usage_session ON usage_sessions(session_id,event_id);
         INSERT OR IGNORE INTO usage_sessions
         SELECT json_extract(facts,'$.usage_event_id'),session_id FROM call_records
         WHERE json_type(facts,'$.usage_event_id')='text'
           AND length(json_extract(facts,'$.usage_event_id')) BETWEEN 1 AND 256
           AND length(session_id) BETWEEN 1 AND 256;",
    )?;
    Ok(())
}

pub(super) fn remember(connection: &Connection, id: &str, session: &str) -> Result<()> {
    if !session.is_empty() && session.len() <= 256 && !session.chars().any(char::is_control) {
        connection.execute(
            "INSERT OR IGNORE INTO usage_sessions VALUES (?,?)",
            params![id, session],
        )?;
    }
    Ok(())
}

pub(super) fn source() -> String {
    // Copies and inherited fork records may share an event. Ambiguous evidence
    // stays unassigned rather than billing the same event to multiple sessions.
    format!(
        "(SELECT u.*,COALESCE((SELECT MIN(s.session_id) FROM usage_sessions s
          WHERE s.event_id=u.id HAVING COUNT(*)=1),'') AS session_id
          FROM {RESOLVED_USAGE} u)"
    )
}

impl UsageLedger {
    pub fn session_ids(&self, filter: &UsageSeriesFilter) -> Result<Vec<String>> {
        let (selection, args) = series::selection(filter, false)?;
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let values = rows(
            &self.connect()?,
            &format!(
                "SELECT DISTINCT u.session_id FROM {} u WHERE {selection} AND u.session_id!=''",
                source()
            ),
            &args,
        )?;
        Ok(values
            .into_iter()
            .filter_map(|row| row["session_id"].as_str().map(str::to_owned))
            .collect())
    }

    pub fn query_sessions(
        &self,
        filter: &UsageSeriesFilter,
        page: &SessionBillPage,
        scope: Option<&SessionScope>,
        now: f64,
    ) -> Result<Value> {
        let order = match page.sort.as_str() {
            "recent" => "last_at DESC,session_id",
            "tokens" => "input_tokens+output_tokens DESC,session_id",
            "cost" => "cost_nanos DESC,session_id",
            _ => return Err(UsageError),
        };
        if !(1..=100).contains(&page.limit) || page.offset > i64::MAX as usize {
            return Err(UsageError);
        }
        let (mut selection, mut args) = series::selection(filter, scope.is_some())?;
        let session = if scope.is_some() {
            "conversation_id"
        } else {
            "session_id"
        };
        if let Some(ids) = &page.matching_sessions {
            selection.push_str(&format!(
                " AND u.{session} IN (SELECT value FROM json_each(?))"
            ));
            args.push(SqlValue::Text(json!(ids).to_string()));
        }
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        connection.execute_batch("PRAGMA temp_store=MEMORY")?;
        let source = scoped_source(&connection, scope)?;
        connection.execute(
            &format!(
                "CREATE TEMP TABLE session_bills AS SELECT u.{session} AS session_id,
                MIN(u.observed_at) AS first_at,MAX(u.observed_at) AS last_at,{}
                FROM {source} u WHERE {selection} GROUP BY u.{session}",
                series::aggregate()
            ),
            params_from_iter(&args),
        )?;
        let totals = rows(
            &connection,
            &format!("SELECT {} FROM session_bills u", series::sum_fields()),
            &[],
        )?;
        let unassigned = rows(
            &connection,
            "SELECT * FROM session_bills WHERE session_id=''",
            &[],
        )?;
        let total: i64 = connection.query_row(
            "SELECT COUNT(*) FROM session_bills WHERE session_id!=''",
            [],
            |row| row.get(0),
        )?;
        let records = rows(
            &connection,
            &format!(
                "SELECT * FROM session_bills WHERE session_id!='' ORDER BY {order} LIMIT ? OFFSET ?"
            ),
            &[
                SqlValue::Integer(page.limit as i64),
                SqlValue::Integer(page.offset as i64),
            ],
        )?;
        Ok(
            json!({"start":filter.start,"end":filter.end,"offset":page.offset,"limit":page.limit,
            "total":total,"records":records,"totals":totals.first(),"unassigned":unassigned.first(),
            "pricing":self.prices.snapshot(now),"currency":"USD"}),
        )
    }
}

pub(super) fn scoped_source(
    connection: &Connection,
    scope: Option<&SessionScope>,
) -> Result<String> {
    let Some(scope) = scope else {
        return Ok(source());
    };
    scope.install(connection)?;
    // Fork copies may have several leaf owners, but one agreed conversation.
    Ok(format!(
        "(SELECT s.*,COALESCE((SELECT MIN(m.root_id) FROM usage_sessions e
          LEFT JOIN conversation_scope m ON m.thread_id=e.session_id WHERE e.event_id=s.id
          HAVING COUNT(DISTINCT COALESCE(m.root_id,''))=1),'') AS conversation_id,
          COALESCE(n.name,'') AS session_name,COALESCE(n.kind,'shared') AS session_kind
          FROM {} s LEFT JOIN conversation_scope n ON n.thread_id=s.session_id)",
        source()
    ))
}
