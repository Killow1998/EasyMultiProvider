use super::*;
use emp_modules::{
    team::{ShareMetadata, SyncPlan},
    workspace::{NATIVE_SHARE, Publish, SharedService},
};
use std::collections::BTreeSet;

mod cleanup;
use cleanup::{
    merge_subscription_routes, remove_legacy_providers, remove_private_file, retire_subscriptions,
};

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
            self.apply_permissions(state, store)?;
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
        let syncable = plan.selected.len();
        let mut selected = plan
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
        selected.sort_by_key(|(share, _)| {
            (
                std::cmp::Reverse(share.payload.subscription.as_ref().map(|s| s.expires_at)),
                &share.id,
            )
        });
        let mut identities = BTreeSet::new();
        for (share, install) in &mut selected {
            if let Some(auth) = &share.payload.subscription {
                *install &= identities.insert(emp_state::accounts::account_auth_identity(
                    &auth.auth_document(),
                ));
            }
        }
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
        let session = session.clone();
        let settings = &state.backend.configuration;
        let (changed, refresh) = super::super::accounts::edit_account_credentials(
            state,
            |config| -> Result<_> {
                let mut updated = config.clone();
                let mut removed_paths = remove_legacy_providers(&mut updated, &store.imported);
                let mut retired = BTreeSet::new();
                for (source, record) in store.imported.iter_mut().filter(|(_, r)| {
                    !r.available && r.workspace == workspace && r.expires_at.is_some()
                }) {
                    retired.insert(source.clone());
                    if let Some(id) = record.local_account.take() {
                        retired.insert(id);
                    }
                }
                removed_paths.extend(retire_subscriptions(state, &mut updated, &retired)?);
                retired.clear();
                store
                    .imported
                    .retain(|_, record| record.expires_at.is_some());
                let mut credentials = Vec::new();
                let mut changed = 0;
                for (share, install) in selected {
                    let local_id = format!("team-{}", share.id);
                    let metadata = store.imported.get(&local_id);
                    let auth = share
                        .payload
                        .subscription
                        .as_ref()
                        .ok_or_else(|| Failure(502, "invalid_workspace_share".into()))?;
                    let document = auth.auth_document();
                    let account_identity = emp_state::accounts::account_auth_identity(&document);
                    let retained = super::subscriptions::matching_local(
                        &updated,
                        &settings.vault,
                        &document,
                        &account_identity,
                    );
                    if let Some(previous) = metadata.and_then(|r| r.local_account.as_ref())
                        && (!install || retained.as_ref() != Some(previous))
                    {
                        retired.insert(previous.clone());
                    }
                    let mut binding = None;
                    if install {
                        let written = super::subscriptions::stage(
                            state,
                            &mut updated,
                            &session,
                            retained.as_deref(),
                            metadata,
                            share,
                            &mut credentials,
                        )?;
                        if let Some(id) = retained {
                            if updated["accounts"]
                                .as_array()
                                .expect("accounts")
                                .iter()
                                .any(|r| r["id"] == id && r["workspace_shared"] == true)
                            {
                                binding = Some(id.clone());
                            }
                            if metadata.is_some() && local_id != id {
                                merge_subscription_routes(&mut updated, &local_id, &id);
                                retired.insert(local_id.clone());
                            }
                        }
                        if written
                            || metadata.is_none_or(|r| {
                                !r.available
                                    || r.version != share.version
                                    || r.local_account != binding
                            })
                        {
                            changed += 1;
                        }
                    } else if metadata.is_some() {
                        retired.insert(local_id.clone());
                    }
                    access_changed |=
                        metadata.and_then(|r| r.local_account.as_ref()) != binding.as_ref();
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
                            local_account: binding,
                        },
                    );
                }
                retired.retain(|id| {
                    !store
                        .imported
                        .values()
                        .any(|r| r.local_account.as_ref() == Some(id))
                });
                removed_paths.extend(retire_subscriptions(state, &mut updated, &retired)?);
                if store.recovery_required {
                    store.recovery_required = updated["accounts"]
                        .as_array()
                        .expect("accounts")
                        .iter()
                        .filter(|row| row["workspace_shared"] == true)
                        .any(|row| {
                            store
                                .grant(row["id"].as_str().expect("account ID"))
                                .is_none()
                        });
                    access_changed |= !store.recovery_required;
                }
                let refresh = changed > 0
                    || access_changed
                    || !removed_paths.is_empty()
                    || **config != updated;
                if refresh {
                    emp_state::config::preserve_service_route_names(config, &mut updated);
                    emp_state::config::validate_service_names(config, &updated)
                        .map_err(|_| Failure(409, "workspace_service_name_conflict".into()))?;
                    config
                        .commit_with(&updated, |_, files| {
                            for path in &removed_paths {
                                remove_private_file(files, path)?;
                            }
                            super::subscriptions::persist(&settings.vault, files, credentials)?;
                            files.remember(&self.path)?;
                            self.write(&settings.vault, &store)?;
                            Ok(())
                        })
                        .map_err(|_| Failure::unavailable())?;
                    *self.store.lock().map_err(|_| Failure::unavailable())? = Some(store);
                }
                Ok((changed, refresh))
            },
            |r| matches!(r, Ok((_, true))),
        )
        .ok_or_else(Failure::unavailable)??;
        if refresh {
            super::super::catalog::refresh_catalog(state).map_err(|_| Failure::unavailable())?;
        }
        Ok(json!({
            "synced": changed,
            "unchanged": syncable - changed,
            "skipped_native": plan.skipped_native,
        }))
    }

    pub(super) fn apply_permissions(&self, state: &ServerState, mut store: Store) -> Result<()> {
        let session = store.session.as_ref().ok_or_else(Failure::signed_out)?;
        let grants = session.identity["native_usage"].as_array();
        let mut retired = BTreeSet::new();
        for (source, record) in store
            .imported
            .iter_mut()
            .filter(|(_, r)| r.workspace == session.workspace() && r.expires_at.is_some())
        {
            if session.identity["member"]["sync"] != true
                || grants.is_some_and(|grants| {
                    !grants.iter().any(|g| {
                        g["share"] == record.share
                            && g["account_identity"].as_str() == record.account_identity.as_deref()
                    })
                })
            {
                if record.available {
                    retired.insert(source.clone());
                }
                record.available = false;
                if let Some(id) = record.local_account.take() {
                    retired.insert(id);
                }
            }
        }
        if retired.is_empty() {
            return self.save_store(state, store);
        }
        let settings = &state.backend.configuration;
        super::super::accounts::replacing_account_credentials(state, |config| -> Result<()> {
            let mut updated = config.clone();
            let paths = retire_subscriptions(state, &mut updated, &retired)?;
            config
                .commit_with(&updated, |_, files| {
                    for path in paths {
                        remove_private_file(files, &path)?;
                    }
                    files.remember(&self.path)?;
                    self.write(&settings.vault, &store)?;
                    Ok(())
                })
                .map_err(|_| Failure::unavailable())?;
            *self.store.lock().map_err(|_| Failure::unavailable())? = Some(store);
            Ok(())
        })
        .ok_or_else(Failure::unavailable)?
    }

    pub(crate) fn purge_local(&self, state: &ServerState) -> Result<()> {
        let old = self.snapshot()?;
        super::super::accounts::replacing_account_credentials(state, |config| -> Result<()> {
            let mut updated = config.clone();
            let managed = updated["accounts"]
                .as_array()
                .expect("normalized accounts")
                .iter()
                .filter(|row| row["workspace_shared"] == true)
                .filter_map(|row| {
                    row["id"].as_str().filter(|id| {
                        is_team_service_id(id)
                            || row["retained_identity"].is_string()
                            || old.grant(id).is_some()
                    })
                })
                .map(str::to_owned)
                .collect();
            let mut paths = retire_subscriptions(state, &mut updated, &managed)?;
            paths.extend(remove_legacy_providers(&mut updated, &old.imported));
            let removed = |id: &str| {
                managed.contains(id)
                    && !updated["accounts"]
                        .as_array()
                        .expect("normalized accounts")
                        .iter()
                        .any(|row| row["id"] == id)
            };
            let clear_search = updated["subscription_search"]["account_id"]
                .as_str()
                .is_some_and(removed);
            if updated["decision_provider"].as_str().is_some_and(removed) {
                updated["decision_provider"] = json!("");
            }
            if clear_search {
                updated["subscription_search"]["enabled"] = json!(false);
                updated["subscription_search"]["account_id"] = json!("");
            }
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
            Ok(())
        })
        .ok_or_else(Failure::unavailable)??;
        self.discovery.stop();
        let _ = super::super::catalog::refresh_catalog(state);
        Ok(())
    }
}
