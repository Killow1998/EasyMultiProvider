//! Remove one merged imported service under its credential and configuration locks.
use super::{
    duplicate_accounts, forget_pending_rotation, notify_quota_update, quota_refresh_cancellation,
    quota_refresh_lock,
};
use crate::app::ServerState;
use emp_state::FileTransaction;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

pub(crate) fn delete_account_state(
    request_id: Option<&str>,
    state: &ServerState,
    account_id: &str,
) -> Result<(), String> {
    let merged_ids = |config: &Value| {
        let mut ids = vec![account_id.to_owned()];
        ids.extend(
            duplicate_accounts(
                config,
                &state.backend.configuration.vault,
                &state.backend.accounts.native_auth_path,
            )
            .into_iter()
            .filter_map(|(id, owner)| (owner == account_id).then_some(id)),
        );
        ids.sort();
        ids
    };
    let ids = merged_ids(
        &*state
            .backend
            .configuration
            .read()
            .map_err(|_| "internal server error".to_owned())?,
    );
    let cancellations = ids
        .iter()
        .map(|id| {
            quota_refresh_cancellation(state, id).ok_or_else(|| "internal server error".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    for cancellation in &cancellations {
        cancellation.store(true, Ordering::Release);
    }
    let result = crate::services::observation::operation::observe(
        &state.backend.diagnostics,
        request_id,
        "account_delete",
        |receipt| {
            receipt.subject(account_id);
            let refresh_locks = ids
                .iter()
                .map(|id| {
                    quota_refresh_lock(state, id).ok_or_else(|| "internal server error".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let _refresh_guards = refresh_locks
                .iter()
                .map(|lock| lock.lock().map_err(|_| "internal server error".to_owned()))
                .collect::<Result<Vec<_>, _>>()?;
            // Held until the result is stored; see `import_account_state`.
            let mut config = state
                .backend
                .configuration
                .edit()
                .map_err(|_| "internal server error".to_owned())?;
            let current = config.clone();
            if merged_ids(&current) != ids {
                return Err("account changed during removal; please remove again".to_owned());
            }
            let accounts = current
                .get("accounts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let targets = accounts
                .iter()
                .filter(|account| {
                    account["id"]
                        .as_str()
                        .is_some_and(|id| ids.iter().any(|target| target == id))
                })
                .collect::<Vec<_>>();
            if targets.len() != ids.len() {
                return Err(format!("unknown account: {account_id}"));
            }
            let expected = targets
                .iter()
                .map(|target| {
                    let id = target["id"].as_str().expect("matched account id");
                    let expected = emp_state::account_auth_path(
                        &current,
                        id,
                        &state.backend.configuration.config_path,
                    )
                    .map_err(|error| error.to_string())?;
                    if target["auth_file"].as_str().map(PathBuf::from).as_ref() != Some(&expected) {
                        return Err(
                            "refusing to delete credentials outside the account store".to_owned()
                        );
                    }
                    Ok(expected)
                })
                .collect::<Result<Vec<_>, String>>()?;
            // Legacy rows keyed by this id must reach the identity they belong to,
            // or be dropped, before the id becomes free for a different account.
            receipt
                .step("settle_history", || {
                    for id in &ids {
                        crate::services::quota::settle_legacy_quota_history(state, &config, id)?;
                    }
                    Ok::<_, emp_codex::quota_history::QuotaHistoryError>(())
                })
                .map_err(|_| {
                    "quota history is unavailable; the account was not deleted".to_owned()
                })?;
            let mut updated = current.clone();
            updated["accounts"] = Value::Array(
                accounts
                    .into_iter()
                    .filter(|account| {
                        !account["id"]
                            .as_str()
                            .is_some_and(|id| ids.iter().any(|target| target == id))
                    })
                    .collect(),
            );
            if updated
                .get("subscription_search")
                .and_then(Value::as_object)
                .and_then(|search| search.get("account_id"))
                .and_then(Value::as_str)
                .is_some_and(|id| ids.iter().any(|target| target == id))
                && let Some(search) = updated
                    .get_mut("subscription_search")
                    .and_then(Value::as_object_mut)
            {
                search.insert("enabled".to_owned(), Value::Bool(false));
                search.insert("account_id".to_owned(), Value::String(String::new()));
            }
            receipt.step("commit_account_and_configuration", || {
                let mut transaction = FileTransaction::new();
                let paths = expected
                    .iter()
                    .flat_map(|auth| {
                        [
                            auth.clone(),
                            auth.parent()
                                .unwrap_or_else(|| Path::new("."))
                                .join("config.toml"),
                        ]
                    })
                    .collect::<Vec<_>>();
                for path in &paths {
                    transaction
                        .remember(path)
                        .map_err(|error| error.to_string())?;
                }
                for path in &paths {
                    match std::fs::remove_file(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
                config
                    .commit_files(&updated, transaction)
                    .map_err(|error| error.to_string())
            })?;
            receipt.check("account_and_configuration_committed", Some(true));
            for (id, path) in ids.iter().zip(&expected) {
                forget_pending_rotation(state, path);
                state.catalog_refresh.account_changed(id);
            }
            drop(config);
            for id in &ids {
                notify_quota_update(state, id, None);
            }
            Ok(())
        },
    );
    if result.is_err() {
        for cancellation in &cancellations {
            cancellation.store(false, Ordering::Release);
        }
    }
    result
}
