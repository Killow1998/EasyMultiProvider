use crate::app::ServerState;
use crate::services::accounts::{account_credentials, native_auth_document, regular_file};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) struct CatalogSources {
    pub(super) native: Value,
    pub(super) accounts: BTreeMap<String, Value>,
    duplicates: BTreeMap<String, String>,
    native_is_local_key: bool,
    registry: emp_state::official_registry::RegistrySource,
}

impl CatalogSources {
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub(super) fn load(state: &ServerState, config: &mut Value) -> Self {
        let native = emp_codex::load_native_catalog(config);
        let credentials = account_credentials(config, &state.backend.configuration.vault);
        let by_id: BTreeMap<_, _> = credentials
            .iter()
            .map(|(id, auth)| (id.as_str(), auth))
            .collect();
        state
            .backend
            .team
            .project_subscription_credentials(state, config);
        let mut accounts = BTreeMap::new();
        let configured_accounts = config["accounts"].as_array().expect("normalized accounts");
        for record in configured_accounts {
            let id = record["id"].as_str().expect("normalized account ID");
            let credential_id = record["_emp_shared_source"].as_str().unwrap_or(id);
            let auth_path = record
                .get("_emp_catalog_auth_file")
                .unwrap_or(&record["auth_file"]);
            let catalog = emp_codex::cached_subscription_catalog(
                config.as_object().expect("config object"),
                auth_path.as_str().expect("normalized auth path"),
                || {
                    by_id
                        .get(credential_id)
                        .and_then(|auth| emp_codex::account_auth_headers(auth))
                },
            )
            .unwrap_or_else(|| {
                let mut catalog = native.clone();
                catalog["_emp_catalog_origin"] = serde_json::json!("reference");
                catalog
            });
            accounts.insert(id.to_owned(), catalog);
        }
        let native_auth = native_auth_document(&state.backend.accounts.native_auth_path);
        let duplicates = emp_state::duplicate_account_status(native_auth.as_ref(), &credentials);
        let native_is_local_key = native_auth
            .as_ref()
            .is_some_and(emp_codex::configuration::local_auth::is_local);
        Self {
            native,
            accounts,
            duplicates,
            native_is_local_key,
            registry: state.backend.configuration.official_models,
        }
    }

    pub(super) fn merged(&self, config: &Value) -> Value {
        let mut catalog = emp_codex::merged_catalog::build_catalog_with_registry(
            config,
            &self.native,
            &self.accounts,
            &self.duplicates,
            self.registry,
        );
        if self.native_is_local_key {
            emp_codex::merged_catalog::hide_native_models(&mut catalog, &self.native);
        }
        catalog
    }

    pub(super) fn public_configuration(&self, state: &ServerState, config: &Value) -> Value {
        let mut public = emp_state::public_configuration_with_file_status(
            config,
            &self.duplicates,
            regular_file,
        );
        state
            .backend
            .team
            .label_subscription_permissions(&mut public["accounts"]);
        public["claude_quota"] = state
            .backend
            .claude_quota
            .snapshot(crate::util::system_now());
        public["emp_version"] = json!(crate::VERSION);
        public["memory_routing_status"] = state.backend.memory_status.snapshot();
        public["codex_context_settings"] = state
            .backend
            .integration
            .preferences
            .snapshot(
                state
                    .backend
                    .integration
                    .inventory
                    .selected_trusted_version()
                    .as_deref(),
            )
            .unwrap_or_else(|_| json!({"unavailable":true}));
        public["native_account"] =
            crate::services::accounts::native_account_snapshot(state, config);
        let views = emp_codex::management_views::model_views(
            config,
            &self.native,
            &self.accounts,
            &self.duplicates,
            self.native_is_local_key,
            self.registry,
        );
        public
            .as_object_mut()
            .expect("public config")
            .extend(views.as_object().expect("model views").clone());
        public
    }
}
