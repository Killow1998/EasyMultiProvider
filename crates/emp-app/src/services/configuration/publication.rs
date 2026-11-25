use super::ConfigurationRead;
use crate::app::ServerState;
use crate::services::{
    catalog, management_events, observation::operation::OperationObservation, runtime,
};
use emp_modules::Module;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub(crate) enum CommittedChange<'a> {
    Settings {
        before: &'a Value,
        current: ConfigurationRead<'a>,
        receipt: &'a mut OperationObservation,
    },
    ContextPreference(&'a mut OperationObservation),
    AccountSource(&'a str),
    Migration(&'a mut OperationObservation),
    Team,
    Module(Module),
    NativeLogin,
}

pub(crate) fn after_commit(
    state: &ServerState,
    change: CommittedChange<'_>,
) -> Result<Option<PathBuf>, ()> {
    let catalog_path = match change {
        CommittedChange::Settings {
            before,
            current,
            receipt,
        } => {
            for id in changed_account_ids(before, &current) {
                catalog::account_source_changed(state, &id);
            }
            drop(current);
            catalog::request_refresh(state, false);
            receipt.fact("subscription_refresh", "requested");
            if let Ok(id) = receipt.step("queue_usage_scan", || {
                state.backend.usage.queue_scan(state).ok_or(())
            }) {
                receipt.number("usage_scan_command_id", id);
            }
            receipt.check("runtime_catalog_matches_target", None);
            runtime::mark_active_pending(state, "EMP configuration changed");
            None
        }
        CommittedChange::ContextPreference(receipt) => {
            let refreshed = receipt.step("publish_catalog", || catalog::refresh_catalog(state));
            catalog::request_refresh(state, false);
            receipt.fact("subscription_refresh", "requested");
            let (path, _) = refreshed?;
            receipt.check("catalog_published", Some(true));
            receipt.check("runtime_catalog_matches_target", None);
            Some(path)
        }
        CommittedChange::AccountSource(id) => {
            catalog::account_source_changed(state, id);
            None
        }
        CommittedChange::Migration(receipt) => {
            let (path, _) = receipt.step("publish_catalog", || catalog::refresh_catalog(state))?;
            receipt.check("catalog_published", Some(true));
            receipt.check("runtime_catalog_matches_target", None);
            Some(path)
        }
        CommittedChange::Team => {
            let (path, _) = catalog::refresh_catalog(state)?;
            Some(path)
        }
        CommittedChange::Module(Module::Aggregate | Module::Team | Module::Aero) => {
            if let Ok((path, _)) = catalog::refresh_catalog(state) {
                Some(path)
            } else {
                catalog::request_refresh(state, false);
                None
            }
        }
        CommittedChange::Module(_) => None,
        CommittedChange::NativeLogin => {
            runtime::mark_active_pending(state, "Codex login changed; restart Codex Runtime");
            state
                .backend
                .management_events
                .publish(management_events::Change::Integration);
            None
        }
    };
    Ok(catalog_path)
}

pub(super) fn changed_account_ids(before: &Value, after: &Value) -> BTreeSet<String> {
    fn source_identities(config: &Value) -> BTreeMap<String, Vec<(String, bool, String)>> {
        let mut accounts_by_id = BTreeMap::<String, Vec<(String, bool, String)>>::new();
        for account in config
            .get("accounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = account.get("id").and_then(Value::as_str) {
                accounts_by_id.entry(id.to_owned()).or_default().push((
                    account
                        .get("auth_file")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    account.get("enabled") != Some(&Value::Bool(false)),
                    account
                        .get("credential_status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                ));
            }
        }
        accounts_by_id
    }

    let before = source_identities(before);
    let after = source_identities(after);
    before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|id| before.get(id) != after.get(id))
        .collect()
}
