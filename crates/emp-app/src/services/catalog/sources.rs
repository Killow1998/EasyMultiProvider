//! A request-local view of catalogs and account identities, never a stale cache.
use crate::app::ServerState;
use crate::services::accounts::{account_credentials, native_auth_document, regular_file};
use emp_codex::subscription_contexts::{SubscriptionContextError, validate_subscription_contexts};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) struct CatalogSources {
    native: Value,
    accounts: BTreeMap<String, Value>,
    duplicates: BTreeMap<String, String>,
}

impl CatalogSources {
    pub(super) fn load(state: &ServerState, config: &Value) -> Self {
        let native = emp_codex::load_native_catalog(config);
        let credentials = account_credentials(config, &state.backend.configuration.vault);
        let by_id: BTreeMap<_, _> = credentials
            .iter()
            .map(|(id, auth)| (id.as_str(), auth))
            .collect();
        let mut accounts = BTreeMap::new();
        for account in config["accounts"].as_array().into_iter().flatten() {
            let Some(account) = account.as_object() else {
                continue;
            };
            let id = account
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let catalog = emp_codex::cached_account_catalog(
                config.as_object().expect("config object"),
                account,
                &mut |_| {
                    by_id
                        .get(id)
                        .and_then(|auth| emp_codex::account_auth_headers(auth))
                },
            )
            .unwrap_or_else(|| native.clone());
            accounts.insert(id.to_owned(), catalog);
        }
        let native_auth = native_auth_document(&state.backend.accounts.native_auth_path);
        let duplicates = emp_state::duplicate_account_status(native_auth.as_ref(), &credentials);
        Self {
            native,
            accounts,
            duplicates,
        }
    }

    pub(super) fn merged(&self, config: &Value) -> Value {
        emp_codex::merged_catalog::build_catalog(
            config,
            &self.native,
            &self.accounts,
            &self.duplicates,
        )
    }

    pub(super) fn public_configuration(
        &self,
        state: &ServerState,
        config: &Value,
    ) -> Result<Value, emp_state::ConfigError> {
        let mut public = emp_state::public_configuration_with_file_status(
            config,
            &self.duplicates,
            regular_file,
        )?;
        public["claude_quota"] = state
            .backend
            .claude_quota
            .snapshot(crate::util::system_now());
        public["emp_version"] = json!(crate::VERSION);
        public["native_account"] =
            crate::services::accounts::native_account_snapshot(state, config);
        let views = emp_codex::management_views::model_views(
            config,
            &self.native,
            &self.accounts,
            &self.duplicates,
        );
        public
            .as_object_mut()
            .expect("public config")
            .extend(views.as_object().expect("model views").clone());
        Ok(public)
    }

    pub(crate) fn validate_update(
        &self,
        updated: &Value,
        previous: &Value,
    ) -> Result<Value, SubscriptionContextError> {
        validate_subscription_contexts(updated, Some(previous), &self.native, &self.accounts)?;
        Ok(emp_state::migrate_duplicate_native_visibility(updated, &self.duplicates).0)
    }
}
