//! Migration commands own encryption/decryption, account publication and catalog.
//! The HTTP boundary still owns the existing one-use export authorization.
use crate::app::ServerState;
use crate::services::observation::operation::{OperationObservation, observe};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use emp_state::{ExportGroups, decrypt_migration_bundle, export_migration_bundle_with_summary};
use serde_json::Value;

const MIN_EXPORT_PASSWORD_UTF8_BYTES: usize = 12;
pub(crate) mod lan;
mod preferences;
mod preview;
pub(crate) use preview::{MigrationPreviews, preview_request};

pub(crate) enum MigrationError {
    Invalid {
        message: String,
        code: Option<&'static str>,
    },
    Internal,
}
impl MigrationError {
    pub(super) fn invalid(error: impl std::fmt::Display) -> Self {
        Self::Invalid {
            message: error.to_string(),
            code: None,
        }
    }
}

pub(crate) struct Exported {
    pub(crate) bundle: Vec<u8>,
    pub(crate) summary: Value,
}

pub(crate) fn export(
    request_id: Option<&str>,
    state: &ServerState,
    body: &Value,
) -> Result<Exported, MigrationError> {
    observe(
        &state.backend.diagnostics,
        request_id,
        "migration_export",
        |receipt| {
            let password = body
                .get("password")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if password.len() < MIN_EXPORT_PASSWORD_UTF8_BYTES {
                return Err(MigrationError::Invalid {
                    message: "migration export password must contain at least 12 UTF-8 bytes"
                        .into(),
                    code: Some("migration_password_too_short"),
                });
            }
            let group_values = body.get("groups").and_then(Value::as_array);
            let groups = match group_values {
                Some(values) => {
                    let Some(values) = values.iter().map(Value::as_str).collect::<Option<Vec<_>>>()
                    else {
                        return Err(MigrationError::invalid(
                            "select at least one valid export category",
                        ));
                    };
                    Some(ExportGroups::from_list(&values).map_err(MigrationError::invalid)?)
                }
                None => None,
            };
            let config = state
                .backend
                .configuration
                .snapshot()
                .map_err(|_| MigrationError::Internal)?;
            if let Some(selections) = body.get("selections").and_then(Value::as_array) {
                let account_ids: std::collections::BTreeSet<_> = selections
                    .iter()
                    .filter(|s| s["kind"] == "account")
                    .filter_map(|s| s["id"].as_str())
                    .collect();
                let provider_ids: std::collections::BTreeSet<_> = selections
                    .iter()
                    .filter(|s| s["kind"] == "provider")
                    .filter_map(|s| s["id"].as_str())
                    .collect();
                let mut selected_config = config;
                if let Some(accounts) = selected_config["accounts"].as_array_mut() {
                    accounts.retain(|a| account_ids.contains(a["id"].as_str().unwrap_or_default()));
                }
                if let Some(providers) = selected_config["providers"].as_array_mut() {
                    providers.retain(|p| {
                        provider_ids.contains(p["id"].as_str().unwrap_or_default())
                            || (account_ids.contains("@native") && p["auth_mode"] == "forward")
                    });
                }
                let mut categories = Vec::new();
                if account_ids.contains("@native") {
                    categories.push("native");
                }
                if account_ids.iter().any(|id| *id != "@native") {
                    categories.push("subscriptions");
                }
                if !provider_ids.is_empty() {
                    categories.push("external");
                }
                let selected_groups =
                    ExportGroups::from_list(&categories).map_err(MigrationError::invalid)?;
                let (payload, summary) = emp_state::migration::prepare_migration_export(
                    &selected_config,
                    &state.backend.configuration.vault,
                    Some(&selected_groups),
                    Some(&state.backend.accounts.native_auth_path),
                )
                .map_err(MigrationError::invalid)?;
                let payload = emp_state::migration::select_migration_payload(
                    &payload,
                    selections,
                    body["include_context"].as_bool().unwrap_or(true),
                    body["include_rename"].as_bool().unwrap_or(true),
                )
                .map_err(MigrationError::invalid)?;
                let mut salt = [0; 16];
                getrandom::getrandom(&mut salt).map_err(|_| MigrationError::Internal)?;
                let plaintext =
                    serde_json::to_vec(&payload).map_err(|_| MigrationError::Internal)?;
                let bundle = emp_state::migration::encode_migration(password, &salt, &plaintext)
                    .map_err(MigrationError::invalid)?;
                return Ok(Exported {
                    bundle,
                    summary: serde_json::json!({"accounts":payload["accounts"].as_array().map_or(0,Vec::len),"providers":payload["config"]["providers"].as_array().map_or(0,Vec::len),"models":payload["config"]["models"].as_array().map_or(0,Vec::len),"native_login_included":summary.native_login_included}),
                });
            }
            let (bundle, summary) = receipt
                .step("encrypt_bundle", || {
                    export_migration_bundle_with_summary(
                        &config,
                        password,
                        &state.backend.configuration.vault,
                        groups.as_ref(),
                        Some(&state.backend.accounts.native_auth_path),
                    )
                })
                .map_err(MigrationError::invalid)?;
            let summary = serde_json::json!({
                "accounts":summary.accounts,
                "providers":summary.providers,
                "models":summary.models,
                "groups":summary.groups,
                "native_login_included":summary.native_login_included,
                "native_login_missing":summary.native_login_missing,
            });
            receipt.check("encrypted_bundle_created", Some(true));
            Ok(Exported { bundle, summary })
        },
    )
}

pub(crate) fn import(
    request_id: Option<&str>,
    state: &ServerState,
    body: &Value,
) -> Result<Value, MigrationError> {
    observe(
        &state.backend.diagnostics,
        request_id,
        "migration_import",
        |receipt| {
            let password = body
                .get("password")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let Some(encoded) = body
                .get("bundle")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            else {
                return Err(MigrationError::invalid("migration bundle is required"));
            };
            let bundle = STANDARD
                .decode(encoded)
                .map_err(|_| MigrationError::invalid("migration bundle is not valid base64"))?;
            // Decrypting (scrypt) needs no configuration; keep it outside the locks.
            let decrypted = receipt
                .step("decrypt_bundle", || {
                    decrypt_migration_bundle(&bundle, password)
                })
                .map_err(MigrationError::invalid)?;
            // Account publication serializes credential replacement with refresh and config writes.
            let summary = receipt.step("commit_bundle", || {
                crate::services::accounts::import_account_bundle(state, decrypted)
                    .ok_or(MigrationError::Internal)?
                    .map_err(MigrationError::invalid)
            })?;
            finish_import(state, summary, receipt)
        },
    )
}

fn finish_import(
    state: &ServerState,
    summary: emp_state::migration::MigrationImportSummary,
    receipt: &mut OperationObservation,
) -> Result<Value, MigrationError> {
    receipt.check("bundle_committed", Some(true));
    let (path, _) = receipt.step("publish_catalog", || {
        crate::services::catalog::refresh_catalog(state).map_err(|_| MigrationError::Internal)
    })?;
    receipt.check("catalog_published", Some(true));
    receipt.check("runtime_catalog_matches_target", None);
    let mut result = serde_json::json!({"status":"ok","accounts":summary.accounts,"providers":summary.providers,"models":summary.models,"catalog_path":path.to_string_lossy()});
    if summary.renamed_accounts > 0 {
        result["renamed_accounts"] = summary.renamed_accounts.into();
    }
    Ok(result)
}

fn commit_selected(
    state: &ServerState,
    payload: &Value,
    choices: &Value,
    receipt: &mut OperationObservation,
) -> Result<Value, MigrationError> {
    let summary = receipt.step("commit_bundle", || {
        crate::services::accounts::replacing_account_credentials(state, |config| {
            let resolution = preferences::resolve(state, config, payload, choices, true)?;
            let (mut target, auth, summary) = emp_state::migration::preview_migration_import(
                config,
                &resolution.payload,
                &state.backend.configuration.config_path,
                &state.backend.configuration.vault,
                &std::collections::BTreeMap::new(),
            )
            .map_err(MigrationError::invalid)?;
            preferences::apply(&mut target, &resolution.updates);
            config
                .commit_migration(&target, &auth)
                .map_err(MigrationError::invalid)?;
            Ok(summary)
        })
        .ok_or(MigrationError::Internal)?
    })?;
    finish_import(state, summary, receipt)
}
