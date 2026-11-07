//! A request-local view of catalogs and account identities, never a stale cache.
use crate::app::ServerState;
use crate::services::accounts::{account_credentials, native_auth_document, regular_file};
use emp_codex::subscription_contexts::validate_subscription_contexts;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) struct CatalogSources {
    native: Value,
    accounts: BTreeMap<String, Value>,
    duplicates: BTreeMap<String, String>,
    native_plan: Option<String>,
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
        let native_plan = state
            .backend
            .accounts
            .native_quota
            .lock()
            .ok()
            .and_then(|quota| {
                quota
                    .as_ref()
                    .and_then(|value| value["plan_type"].as_str().map(str::to_owned))
            });
        Self {
            native,
            accounts,
            duplicates,
            native_plan,
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
        );
        public
            .as_object_mut()
            .expect("public config")
            .extend(views.as_object().expect("model views").clone());
        Ok(public)
    }

    pub(crate) fn validate_update(&self, updated: &Value, previous: &Value) -> Result<(), String> {
        validate_subscription_contexts(updated, Some(previous), &self.native, &self.accounts)
            .map_err(|error| error.to_string())?;
        let mut base = updated.clone();
        base["model_groups"] = json!([]);
        let catalog = self.merged(&base);
        for group in updated["model_groups"].as_array().into_iter().flatten() {
            if catalog["models"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|model| model["slug"] == group["id"])
            {
                return Err("Aggregate model ID is already in use".into());
            }
            let previous_sources = previous["model_groups"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|old| old["id"] == group["id"])
                .and_then(|old| old["sources"].as_array());
            let mut family = None;
            let mut plans = std::collections::BTreeSet::new();
            for source in group["sources"].as_array().expect("normalized sources") {
                let source = source.as_str().expect("normalized source");
                if !catalog["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|model| model["slug"] == source)
                {
                    if previous_sources
                        .is_some_and(|sources| sources.iter().any(|old| old == source))
                    {
                        continue;
                    }
                    return Err(format!("Aggregate source is unavailable: {source}"));
                }
                let external = updated["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|model| model["id"] == source);
                if group["policy"] == "balanced" {
                    if external.is_some() {
                        return Err("Quota balance requires Codex subscription sources; use priority or round robin for API sources".into());
                    }
                    let account =
                        updated["accounts"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|account| {
                                account["prefix"]
                                    .as_str()
                                    .is_some_and(|prefix| source.starts_with(&format!("{prefix}/")))
                            });
                    let plan = if let Some(account) = account {
                        account["quota"]["plan_type"].as_str()
                    } else {
                        self.native_plan.as_deref()
                    };
                    if let Some(plan) = plan {
                        plans.insert(plan);
                    }
                }
                let current = external
                    .and_then(|model| {
                        model["family_id"]
                            .as_str()
                            .filter(|value| !value.is_empty())
                            .or_else(|| model["upstream_id"].as_str())
                    })
                    .unwrap_or_else(|| source.rsplit('/').next().expect("source"));
                if family.is_some_and(|family| family != current) {
                    return Err("Aggregate sources must refer to the same model".into());
                }
                family = Some(current);
            }
            if plans.len() > 1 {
                return Err("Quota balance requires accounts on the same subscription plan".into());
            }
        }
        Ok(())
    }
}
