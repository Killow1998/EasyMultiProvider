//! Migration commands own encryption/decryption, account publication and catalog.
//! The HTTP boundary still owns the existing one-use export authorization.
use crate::app::ServerState;
use crate::services::observation::operation::observe;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use emp_state::{ExportGroups, decrypt_migration_bundle, export_migration_bundle_with_summary};
use serde_json::Value;

const MIN_EXPORT_PASSWORD_UTF8_BYTES: usize = 12;

pub(crate) enum MigrationError {
    Invalid {
        message: String,
        code: Option<&'static str>,
    },
    Internal,
}
impl MigrationError {
    fn invalid(error: impl std::fmt::Display) -> Self {
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
                    match ExportGroups::from_list(&values) {
                        Ok(groups) => Some(groups),
                        Err(error) => {
                            return Err(MigrationError::invalid(error));
                        }
                    }
                }
                None => None,
            };
            let config = match state.backend.configuration.read() {
                Ok(config) => config.clone(),
                Err(_) => {
                    return Err(MigrationError::Internal);
                }
            };
            let (bundle, summary) = match receipt.step("encrypt_bundle", || {
                export_migration_bundle_with_summary(
                    &config,
                    password,
                    &state.backend.configuration.vault,
                    groups.as_ref(),
                    Some(&state.backend.accounts.native_auth_path),
                )
            }) {
                Ok(result) => result,
                Err(error) => {
                    return Err(MigrationError::invalid(error));
                }
            };
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
            let bundle = match STANDARD.decode(encoded) {
                Ok(bundle) => bundle,
                Err(_) => {
                    return Err(MigrationError::invalid(
                        "migration bundle is not valid base64",
                    ));
                }
            };
            // Decrypting (scrypt) needs no configuration; keep it outside the locks.
            let decrypted = match receipt.step("decrypt_bundle", || {
                decrypt_migration_bundle(&bundle, password)
            }) {
                Ok(decrypted) => decrypted,
                Err(error) => {
                    return Err(MigrationError::invalid(error));
                }
            };
            // Same-identity accounts in the bundle replace stored credentials; keep
            // quota refreshes, rotated-credential flushes and other configuration
            // writers out while that happens.
            let summary = receipt.step("commit_bundle", || {
                match crate::services::accounts::import_account_bundle(state, decrypted) {
                    Some(Ok(summary)) => Ok(summary),
                    Some(Err(error)) => Err(MigrationError::invalid(error)),
                    None => Err(MigrationError::Internal),
                }
            })?;
            receipt.check("bundle_committed", Some(true));
            let (catalog_path, _) = match receipt.step("publish_catalog", || {
                crate::services::catalog::refresh_catalog(state)
            }) {
                Ok(result) => result,
                Err(()) => {
                    return Err(MigrationError::Internal);
                }
            };
            let catalog_path = catalog_path.to_string_lossy().into_owned();
            let mut body = serde_json::json!({
                "status":"ok",
                "accounts":summary.accounts,
                "providers":summary.providers,
                "models":summary.models,
                "catalog_path":catalog_path,
            });
            if summary.renamed_accounts > 0 {
                body.as_object_mut()
                    .expect("migration response is an object")
                    .insert(
                        "renamed_accounts".to_owned(),
                        Value::from(summary.renamed_accounts),
                    );
            }
            receipt.check("catalog_published", Some(true));
            receipt.check("runtime_catalog_matches_target", None);
            Ok(body)
        },
    )
}
