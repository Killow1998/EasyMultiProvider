use crate::app::ServerState;
use emp_state::accounts::account_auth_identity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(in crate::services) enum Selection {
    Kept {
        account_id: String,
        identity: String,
    },
    Removed,
}

pub(in crate::services) fn path(state: &ServerState) -> PathBuf {
    state
        .backend
        .configuration
        .config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("state/local-key-subscription.json")
}

impl Selection {
    pub(in crate::services) fn for_account(account_id: &str, auth: &Value) -> Self {
        Self::Kept {
            account_id: account_id.into(),
            identity: account_auth_identity(auth),
        }
    }

    pub(in crate::services) fn load(state: &ServerState) -> io::Result<Option<Self>> {
        match emp_state::filesystem::read_file_limited(&path(state), 4096) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(in crate::services) fn save(
        &self,
        state: &ServerState,
    ) -> Result<(), emp_state::FilesystemError> {
        emp_state::atomic_write_private_state(
            &path(state),
            &serde_json::to_vec(self).expect("retained account reference"),
        )
    }
}

pub(super) fn import_native(state: &ServerState, auth: &Value) -> Result<Selection, &'static str> {
    emp_state::validate_auth_json(auth).map_err(|_| "login_credentials_invalid")?;
    let config = state
        .backend
        .configuration
        .read()
        .map_err(|_| "login_storage_failed")?;
    let existing = crate::services::accounts::matching_account(
        &config,
        &state.backend.configuration.vault,
        auth,
    )
    .cloned();
    let mut imported = existing.unwrap_or_else(
        || json!({"name":emp_state::config::available_service_name(&config, "Native", None)}),
    );
    drop(config);
    imported["auth_json"] = auth.clone();
    let account = crate::services::accounts::import_account_state(None, state, &imported)
        .map_err(|_| "login_import_failed")?;
    Ok(Selection::for_account(
        account["id"].as_str().expect("imported account ID"),
        auth,
    ))
}
