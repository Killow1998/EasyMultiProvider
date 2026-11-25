//! Selecting discovered models commits their config and generated catalog together.
use super::{generated_catalog_path, server_catalog};
use crate::app::ServerState;
use crate::services::configuration::ChangeError;
use emp_state::discovery_merge::{SelectedModels, merge_model_selection};
use emp_state::filesystem::write_catalog_json;
use serde_json::Value;
use std::path::PathBuf;

pub(super) struct ModelSelection {
    pub(super) available: usize,
    pub(super) added: usize,
    pub(super) hidden: usize,
    pub(super) catalog_path: PathBuf,
    pub(super) model_count: usize,
}

pub(super) fn select_models(
    state: &ServerState,
    provider_id: &str,
    discovered: &[Value],
    selected: Result<SelectedModels, emp_state::ConfigError>,
    expected_provider: &Value,
) -> Result<ModelSelection, ChangeError> {
    let mut config = state
        .backend
        .configuration
        .edit()
        .map_err(|_| ChangeError::Unavailable)?;
    if !super::provider_models::unchanged(state, &config, expected_provider) {
        return Err(ChangeError::Invalid(
            "Service changed; update the model list".into(),
        ));
    }
    let merged = merge_model_selection(
        &config,
        provider_id,
        discovered,
        &selected.map_err(ChangeError::invalid)?,
        &emp_state::observed_at_now(),
        state.backend.configuration.official_models,
    )
    .map_err(ChangeError::invalid)?;
    let catalog_path = generated_catalog_path(state);
    let catalog = config
        .commit_with(&merged.config, |saved, transaction| {
            let catalog = emp_state::config::client_catalog(saved, server_catalog(state, saved));
            transaction.remember(&catalog_path)?;
            write_catalog_json(&catalog_path, &catalog)?;
            Ok(catalog)
        })
        .map_err(|_| ChangeError::Unavailable)?;
    Ok(ModelSelection {
        available: merged.available,
        added: merged.added,
        hidden: merged.hidden,
        catalog_path,
        model_count: catalog["models"].as_array().map_or(0, Vec::len),
    })
}
