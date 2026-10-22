//! Join accounting with read-only conversation metadata before presenting a report.
use super::UsageState;
use emp_state::usage::ledger::{SessionBillPage, SessionScope, UsageError, UsageSeriesFilter};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;

impl UsageState {
    fn conversation_scope(
        &self,
        home: &Path,
        filter: &UsageSeriesFilter,
    ) -> Result<SessionScope, UsageError> {
        let mut scope = self.ledger.session_scope(filter)?;
        self.sessions.enrich(home, &mut scope);
        Ok(scope)
    }

    pub(crate) fn query_series(
        &self,
        home: &Path,
        filter: &UsageSeriesFilter,
        conversation: bool,
        now: f64,
    ) -> Result<Value, UsageError> {
        let scope = conversation
            .then(|| self.conversation_scope(home, filter))
            .transpose()?;
        self.ledger.query_series(filter, scope.as_ref(), now)
    }

    pub(crate) fn query_sessions(
        &self,
        home: &Path,
        filter: &UsageSeriesFilter,
        mut page: SessionBillPage,
        query: &str,
        now: f64,
    ) -> Result<Value, UsageError> {
        let scope = self.conversation_scope(home, filter)?;
        if !query.is_empty() {
            page.matching_sessions = Some(
                scope
                    .0
                    .iter()
                    .filter(|(id, node)| {
                        id.to_lowercase().contains(query)
                            || node.name.to_lowercase().contains(query)
                    })
                    .filter_map(|(id, _)| scope.root(id).map(str::to_owned))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            );
        }
        let mut payload = self
            .ledger
            .query_sessions(filter, &page, Some(&scope), now)?;
        for row in payload["records"].as_array_mut().into_iter().flatten() {
            if let Some(node) = row["session_id"].as_str().and_then(|id| scope.0.get(id)) {
                row["session_name"] = json!(node.name);
            }
        }
        Ok(payload)
    }
}
