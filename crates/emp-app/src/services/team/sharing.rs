use super::*;
use emp_modules::{
    team::{ShareMetadata, SyncPlan},
    workspace::{NATIVE_SHARE, Publish, SharedService},
};
use emp_state::{ConfigError, FileTransaction};
use std::{collections::BTreeSet, path::Path};

impl TeamState {
    pub(super) fn publish(
        &self,
        state: &ServerState,
        session: &Session,
        body: &Value,
    ) -> Result<Value> {
        if !body["provider"].is_null() && body["provider"] != NATIVE_SHARE {
            return Err(Failure(400, "workspace_only_native_subscription".into()));
        }
        let token = Some(session.token.as_str());
        let version = request(state, &session.base, token, Operation::PublishVersion, None)?;
        let publish = Publish {
            source: NATIVE_SHARE.into(),
            expected_version: version["version"].as_i64().ok_or_else(Failure::invalid)?,
            payload: super::subscriptions::publish(state, body)?,
        };
        request(
            state,
            &session.base,
            token,
            Operation::Publish,
            Some(&json!(publish)),
        )
    }

    pub(super) fn sync(&self, state: &ServerState, mut store: Store) -> Result<Value> {
        let session = store.session.as_mut().ok_or_else(Failure::signed_out)?;
        let workspace = session.workspace().to_owned();
        let (identity, permissions) = remote_identity(state, session)?;
        let access_changed = session.identity != identity;
        session.identity = identity;
        if !permissions.sync_allowed {
            self.save_store(state, store)?;
            super::super::catalog::refresh_catalog(state).map_err(|_| Failure::unavailable())?;
            return Err(Failure(403, "workspace_permission_required".into()));
        }
        let snapshot = request(
            state,
            &session.base,
            Some(&session.token),
            Operation::Shares,
            None,
        )?;
        if snapshot["workspace_id"] != workspace {
            return Err(Failure(409, "workspace_identity_changed".into()));
        }
        let shares: Vec<SharedService> = serde_json::from_value(snapshot["shares"].clone())
            .map_err(|_| Failure(502, "invalid_workspace_reply".into()))?;
        let native =
            super::super::accounts::native_auth_document(&state.backend.accounts.native_auth_path);
        let metadata: Vec<_> = shares
            .iter()
            .map(|share| {
                let valid_payload = share.payload.valid();
                let duplicates_native = valid_payload
                    && valid_identifier(&share.id)
                    && share.version > 0
                    && share.payload.subscription.as_ref().is_some_and(|auth| {
                        native.as_ref().is_some_and(|native| {
                            emp_state::same_account_auth(native, &auth.auth_document())
                        })
                    });
                ShareMetadata {
                    id: share.id.clone(),
                    version: share.version,
                    valid_payload,
                    duplicates_native,
                }
            })
            .collect();
        let native_duplicates = metadata
            .iter()
            .enumerate()
            .filter(|(_, share)| share.duplicates_native)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let plan: SyncPlan = policy(state, Request::SyncShares { shares: metadata })?;
        let selected = plan
            .selected
            .into_iter()
            .map(|index| (index, true))
            .chain(native_duplicates.into_iter().map(|index| (index, false)))
            .map(|(index, install)| {
                shares
                    .get(index)
                    .map(|share| (share, install))
                    .ok_or_else(|| Failure(400, "invalid_module_reply".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        let available: BTreeSet<_> = selected
            .iter()
            .map(|(share, _)| share.id.as_str())
            .collect();
        let mut access_changed = access_changed;
        for record in store
            .imported
            .values_mut()
            .filter(|record| record.workspace == workspace)
        {
            let current = available.contains(record.share.as_str());
            access_changed |= current != record.available;
            record.available = current;
        }
        let settings = &state.backend.configuration;
        let mut config = settings.edit().map_err(|_| Failure::unavailable())?;
        let mut updated = config.clone();
        let legacy_paths = remove_team_entries(state, &mut updated, &store.imported, false)?;
        store
            .imported
            .retain(|_, record| record.expires_at.is_some());
        let mut credentials = Vec::new();
        let mut changed = 0;
        for (share, install) in selected {
            let local_id = format!("team-{}", share.id);
            let metadata = store.imported.get(&local_id);
            if install
                && super::subscriptions::stage(
                    state,
                    &mut updated,
                    session,
                    &local_id,
                    metadata,
                    share,
                    &mut credentials,
                )?
            {
                changed += 1;
            }
            let auth = share
                .payload
                .subscription
                .as_ref()
                .ok_or_else(|| Failure(502, "invalid_workspace_share".into()))?;
            let account_identity =
                emp_state::accounts::account_auth_identity(&auth.auth_document());
            access_changed |= metadata.is_none_or(|record| {
                record.account_identity.as_deref() != Some(account_identity.as_str())
                    || record.version != share.version
                    || record.expires_at != Some(auth.expires_at)
            });
            store.imported.insert(
                local_id,
                Imported {
                    workspace: workspace.clone(),
                    share: share.id.clone(),
                    version: share.version,
                    available: true,
                    expires_at: Some(auth.expires_at),
                    account_identity: Some(account_identity),
                },
            );
        }
        let available_identities: BTreeSet<_> = store
            .imported
            .values()
            .filter(|record| record.workspace == workspace && record.usable())
            .filter_map(|record| record.account_identity.as_deref())
            .collect();
        super::subscriptions::mark_expired_private(
            &settings.vault,
            &mut updated,
            &available_identities,
        );
        if changed > 0 || access_changed || !legacy_paths.is_empty() || *config != updated {
            emp_state::config::preserve_service_route_names(&config, &mut updated);
            emp_state::config::validate_service_names(&config, &updated)
                .map_err(|_| Failure(409, "workspace_service_name_conflict".into()))?;
            config
                .commit_with(&updated, |_, files| {
                    for path in &legacy_paths {
                        remove_private_file(files, path)?;
                    }
                    super::subscriptions::persist(&settings.vault, files, credentials)?;
                    files.remember(&self.path)?;
                    self.write(&settings.vault, &store)?;
                    Ok(())
                })
                .map_err(|_| Failure::unavailable())?;
            *self.store.lock().map_err(|_| Failure::unavailable())? = Some(store);
            drop(config);
            super::super::catalog::refresh_catalog(state).map_err(|_| Failure::unavailable())?;
        }
        Ok(json!({"synced":changed,"skipped_native":plan.skipped_native}))
    }

    pub(crate) fn purge_local(&self, state: &ServerState) -> Result<()> {
        let settings = &state.backend.configuration;
        let mut config = settings.edit().map_err(|_| Failure::unavailable())?;
        let mut updated = config.clone();
        let old = self.snapshot()?;
        let paths = remove_team_entries(state, &mut updated, &old.imported, true)?;
        let store = Store {
            device: old.device,
            ..Store::default()
        };
        config
            .commit_with(&updated, |_, files| {
                for path in &paths {
                    remove_private_file(files, path)?;
                }
                self.persist_device(&store.device)?;
                remove_private_file(files, &self.path)
            })
            .map_err(|_| Failure::unavailable())?;
        *self.store.lock().map_err(|_| Failure::unavailable())? = Some(store);
        drop(config);
        self.discovery.stop();
        let _ = super::super::catalog::refresh_catalog(state);
        Ok(())
    }
}

fn remove_team_entries(
    state: &ServerState,
    config: &mut Value,
    imported: &BTreeMap<String, Imported>,
    subscriptions: bool,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut removed = BTreeSet::new();
    config["providers"]
        .as_array_mut()
        .expect("normalized providers")
        .retain(|entry| {
            let Some(id) = entry["id"].as_str().filter(|id| {
                imported
                    .get(*id)
                    .is_some_and(|record| record.expires_at.is_none())
            }) else {
                return true;
            };
            if let Some(path) = entry["api_key_file"]
                .as_str()
                .filter(|path| !path.is_empty())
            {
                paths.push(PathBuf::from(path));
            }
            removed.insert(id.to_owned());
            false
        });
    config["models"]
        .as_array_mut()
        .expect("normalized models")
        .retain(|model| {
            !model["provider"]
                .as_str()
                .is_some_and(|id| removed.contains(id))
        });
    if subscriptions {
        for entry in config["accounts"].as_array().expect("normalized accounts") {
            let Some(id) = entry["id"]
                .as_str()
                .filter(|id| entry["workspace_shared"] == true && is_team_service_id(id))
            else {
                continue;
            };
            let auth =
                emp_state::account_auth_path(config, id, &state.backend.configuration.config_path)
                    .map_err(|_| Failure::unavailable())?;
            if !super::subscriptions::owns_credential(entry, &auth) {
                continue;
            }
            paths.push(auth.with_file_name("config.toml"));
            paths.push(auth);
            removed.insert(id.to_owned());
        }
        config["accounts"]
            .as_array_mut()
            .expect("normalized accounts")
            .retain(|account| {
                !account["id"]
                    .as_str()
                    .is_some_and(|id| removed.contains(id))
            });
        if config["subscription_search"]["account_id"]
            .as_str()
            .is_some_and(|id| removed.contains(id))
        {
            config["subscription_search"]["enabled"] = json!(false);
            config["subscription_search"]["account_id"] = json!("");
        }
    }
    if config["decision_provider"]
        .as_str()
        .is_some_and(|id| removed.contains(id))
    {
        config["decision_provider"] = json!("");
    }
    let retained: BTreeSet<_> = [("accounts", "auth_file"), ("providers", "api_key_file")]
        .into_iter()
        .flat_map(|(kind, field)| {
            config[kind]
                .as_array()
                .expect("normalized services")
                .iter()
                .filter_map(move |entry| entry[field].as_str().map(PathBuf::from))
        })
        .collect();
    paths.retain(|path| {
        !retained.contains(path)
            && (path.file_name().and_then(|name| name.to_str()) != Some("config.toml")
                || !retained.iter().any(|auth| auth.parent() == path.parent()))
    });
    Ok(paths)
}

fn remove_private_file(
    files: &mut FileTransaction,
    path: &Path,
) -> std::result::Result<(), ConfigError> {
    files.remember(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(emp_state::FilesystemError::ManagedFileUnavailable.into()),
    }
}
