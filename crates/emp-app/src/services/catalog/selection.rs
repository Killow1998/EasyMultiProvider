//! Selecting discovered models commits their config and generated catalog together.
use super::{generated_catalog_path, server_catalog};
use crate::app::ServerState;
use crate::services::configuration::ChangeError;
use emp_state::discovery_merge::merge_selected_models;
use emp_state::filesystem::write_catalog_json;
use serde_json::Value;
use std::path::PathBuf;

pub(crate) struct ModelSelection {
    pub(crate) available: usize,
    pub(crate) added: usize,
    pub(crate) hidden: usize,
    pub(crate) catalog_path: PathBuf,
    pub(crate) model_count: usize,
}

pub(crate) fn select_models(
    state: &ServerState,
    provider_id: &str,
    discovered: &[Value],
    selected: &Value,
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
    let merged = merge_selected_models(
        &config,
        provider_id,
        discovered,
        selected,
        &emp_state::observed_at_now(),
    )
    .map_err(ChangeError::invalid)?;
    let catalog_path = generated_catalog_path(state);
    let catalog = config
        .commit_with(&merged.config, |saved, transaction| {
            let catalog = server_catalog(state, saved);
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
