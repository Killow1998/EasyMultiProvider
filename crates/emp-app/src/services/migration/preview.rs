//! Session-bound, bounded import previews. Only commit publishes credentials.
use super::{MigrationError, commit_selected, preferences};
use crate::app::ServerState;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Package {
    token: String,
    session: String,
    expires: Instant,
    payload: Value,
    demo: bool,
}
#[derive(Default)]
pub(crate) struct MigrationPreviews {
    packages: Mutex<VecDeque<Package>>,
}
impl MigrationPreviews {
    fn store(&self, session: &str, payload: Value, demo: bool) -> Result<String, MigrationError> {
        let token = crate::util::random_hex(24).map_err(|_| MigrationError::Internal)?;
        let mut packages = self.packages.lock().map_err(|_| MigrationError::Internal)?;
        packages.retain(|p| p.expires > Instant::now());
        while packages.len() >= 4 {
            packages.pop_front();
        }
        packages.push_back(Package {
            token: token.clone(),
            session: session.into(),
            expires: Instant::now() + Duration::from_secs(1800),
            payload,
            demo,
        });
        Ok(token)
    }
    fn get(&self, session: &str, token: &str) -> Result<(Value, bool), MigrationError> {
        let mut packages = self.packages.lock().map_err(|_| MigrationError::Internal)?;
        packages.retain(|p| p.expires > Instant::now());
        packages
            .iter()
            .find(|p| p.session == session && p.token == token)
            .map(|p| (p.payload.clone(), p.demo))
            .ok_or_else(|| MigrationError::invalid("Reopen the migration package"))
    }
    fn remove(&self, session: &str, token: &str) {
        if let Ok(mut packages) = self.packages.lock() {
            packages.retain(|p| p.session != session || p.token != token);
        }
    }
}

fn scrub(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(scrub).collect()),
        Value::Object(items) => Value::Object(
            items
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.to_ascii_lowercase().as_str(),
                        "auth"
                            | "imported_auth"
                            | "provider_keys"
                            | "api_key"
                            | "api_key_file"
                            | "auth_file"
                            | "auth_json"
                            | "access_token"
                            | "refresh_token"
                            | "id_token"
                            | "password"
                            | "authorization"
                            | "cookie"
                    )
                })
                .map(|(key, value)| (key.clone(), scrub(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}
fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap_or_default()
}
fn read_config(state: &ServerState) -> Result<Value, MigrationError> {
    state
        .backend
        .configuration
        .read()
        .map(|config| config.clone())
        .map_err(|_| MigrationError::Internal)
}
fn describe(payload: &Value, token: &str) -> Value {
    json!({"token":token,"payload":scrub(payload),
        "credential_accounts":payload["accounts"].as_array().into_iter().flatten().map(|a|a["metadata"]["id"].clone()).collect::<Vec<_>>(),
        "credential_providers":payload["provider_keys"].as_object().into_iter().flat_map(|p|p.keys()).collect::<Vec<_>>()})
}
fn selections(body: &Value) -> Result<&[Value], MigrationError> {
    body["selections"]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| MigrationError::invalid("Select services"))
}

pub(crate) fn preview_request(
    request_id: Option<&str>,
    state: &ServerState,
    session: &str,
    operation: &str,
    body: &Value,
) -> Result<Value, MigrationError> {
    let cache = &state.backend.migration_previews;
    match operation {
        "decode" => {
            let bundle = STANDARD
                .decode(body["bundle"].as_str().unwrap_or_default())
                .map_err(MigrationError::invalid)?;
            let plain = emp_state::migration::decode_migration(
                body["password"].as_str().unwrap_or_default(),
                &bundle,
            )
            .map_err(MigrationError::invalid)?;
            let payload: Value = serde_json::from_slice(&plain).map_err(MigrationError::invalid)?;
            emp_state::migration::validate_migration_payload(&payload)
                .map_err(MigrationError::invalid)?;
            let token = cache.store(session, payload.clone(), false)?;
            Ok(describe(&payload, &token))
        }
        "demo" => {
            let config = read_config(state)?;
            let account_id = config["accounts"]
                .as_array()
                .and_then(|v| v.first())
                .map(id)
                .unwrap_or("sample-account");
            let provider_id = config["providers"]
                .as_array()
                .and_then(|v| v.first())
                .map(id)
                .unwrap_or("sample-api");
            let account = json!({"id":account_id,"prefix":account_id,"name":"Example subscription","hidden_models":[]});
            let provider = json!({"id":provider_id,"name":"Example subscription","base_url":"https://example.invalid/v1","protocol":"responses","auth_mode":"api_key"});
            let payload = json!({"schema":"easy-multi-provider-migration","version":1,"config":{"accounts":[account],"providers":[provider],"models":[{"id":format!("{provider_id}/sample-model"),"provider":provider_id,"upstream_id":"sample-model","display_name":"Example model","context_window":128000}]},"accounts":[{"metadata":account,"auth":{"tokens":{"account_id":"emp-demo-only","access_token":"emp-demo-only"}}}],"provider_keys":{provider_id:"emp-demo-only"}});
            let token = cache.store(session, payload.clone(), true)?;
            Ok(describe(&payload, &token))
        }
        "preview" | "import" => {
            let token = body["token"].as_str().unwrap_or_default();
            let (source, demo) = cache.get(session, token)?;
            let payload = emp_state::migration::select_migration_payload(
                &source,
                selections(body)?,
                body["include_context"].as_bool().unwrap_or(true),
                body["include_rename"].as_bool().unwrap_or(true),
            )
            .map_err(MigrationError::invalid)?;
            if operation == "import" {
                if demo {
                    return Err(MigrationError::invalid("Demo packages cannot be imported"));
                }
                let result = crate::services::observation::operation::observe(
                    &state.backend.diagnostics,
                    request_id,
                    "migration_import",
                    |receipt| {
                        commit_selected(state, &payload, &body["preference_choices"], receipt)
                    },
                )?;
                cache.remove(session, token);
                return Ok(result);
            }
            crate::services::observation::operation::observe(
                &state.backend.diagnostics,
                request_id,
                "migration_preview",
                |receipt| {
                    let current = receipt.step("read_configuration", || read_config(state))?;
                    let resolution = receipt.step("resolve_preferences", || {
                        preferences::resolve(
                            state,
                            &current,
                            &payload,
                            &body["preference_choices"],
                            false,
                        )
                    })?;
                    receipt.step("build_preview", || {
                        preview(state, &current, resolution, token)
                    })
                },
            )
        }
        _ => Err(MigrationError::invalid("Unknown migration operation")),
    }
}

fn preview(
    state: &ServerState,
    current: &Value,
    resolution: preferences::Resolution,
    token: &str,
) -> Result<Value, MigrationError> {
    let payload = &resolution.payload;
    let (mut config, auth, summary) = emp_state::migration::preview_migration_import(
        current,
        payload,
        &state.backend.configuration.config_path,
        &state.backend.configuration.vault,
        &BTreeMap::new(),
    )
    .map_err(MigrationError::invalid)?;
    preferences::apply(&mut config, &resolution.updates);
    let mut warnings = Vec::new();
    let mut model_previews = serde_json::Map::new();
    let native =
        crate::services::accounts::native_auth_document(&state.backend.accounts.native_auth_path);
    let credentials = config["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|account| {
            let id = account["id"].as_str()?;
            let credential = auth.get(id).cloned().or_else(|| {
                account["auth_file"].as_str().and_then(|path| {
                    state
                        .backend
                        .configuration
                        .vault
                        .read_encrypted_json(std::path::Path::new(path))
                        .ok()
                })
            })?;
            Some((id.to_owned(), credential))
        })
        .collect::<Vec<_>>();
    let duplicates = emp_state::duplicate_account_status(native.as_ref(), &credentials);
    let native_visible = native
        .as_ref()
        .is_some_and(|auth| emp_state::validate_auth_json(auth).is_ok());
    if native_visible {
        config["native_account"] =
            crate::services::accounts::native_account_snapshot(state, &config);
    }
    config["_shadow_merge_preview"] = json!(true);
    for (field, kind) in [("accounts", "account"), ("providers", "provider")] {
        if let Some(records) = config[field].as_array_mut() {
            for record in records {
                let target = id(record).to_owned();
                if kind == "account"
                    && let Some(owner) = duplicates.get(&target)
                {
                    record["duplicate"] = json!(true);
                    record["duplicate_of"] = json!(owner);
                }
                let source = if kind == "provider" {
                    summary
                        .provider_id_map
                        .iter()
                        .find(|(_, id)| **id == target)
                        .map(|(source, _)| source.clone())
                } else {
                    auth.get(&target)
                        .and_then(|value| {
                            payload["accounts"]
                                .as_array()
                                .and_then(|accounts| accounts.iter().find(|a| a["auth"] == *value))
                        })
                        .map(|a| id(&a["metadata"]).to_owned())
                };
                let Some(source) = source else { continue };
                record["_shadow_migration_token"] = json!(token);
                record["_shadow_source_id"] = json!(source);
                record[if kind == "provider" {
                    "api_key_set"
                } else {
                    "credential_set"
                }] = json!(
                    kind == "account"
                        || payload["provider_keys"][&source]
                            .as_str()
                            .is_some_and(|key| !key.is_empty())
                );
                if kind == "account" {
                    model_previews
                        .insert(target.clone(), payload["model_previews"][&source].clone());
                    if record["duplicate_of"] == "当前 Codex 登录" {
                        model_previews
                            .insert("@native".into(), payload["model_previews"][&source].clone());
                        warnings.push(json!({"kind":"account","id":"@native","message":"已确认与 Native 为同一账号，将合并为一个服务。","message_en":"Confirmed the same account as Native; merge into one service."}));
                    }
                }
                let existed = current[field]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|r| id(r) == target));
                if existed || source != target {
                    let (zh, en) = if existed {
                        (
                            "已确认同一账号或连接，合并更新已有服务。其他模型保留。",
                            "Verified matching account or connection; update the existing service and retain other models.",
                        )
                    } else {
                        (
                            "身份不同或无法确认，保留原服务并作为独立服务导入。",
                            "Different or unverifiable identity; import separately and retain the original.",
                        )
                    };
                    warnings.push(json!({"kind":kind,"id":target,"message":zh,"message_en":en}));
                }
            }
        }
    }
    let names: Vec<_> = [("accounts", "account"), ("providers", "provider")]
        .into_iter()
        .flat_map(|(field, kind)| {
            config[field]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |r| {
                    (
                        kind,
                        id(r).to_owned(),
                        r["name"].as_str().unwrap_or(id(r)).to_owned(),
                        r.get("_shadow_migration_token").is_some(),
                    )
                })
        })
        .collect();
    for (kind, target, name, imported) in &names {
        if *imported
            && names
                .iter()
                .any(|(k, id, n, _)| (k != kind || id != target) && n == name)
        {
            warnings.push(json!({"kind":kind,"id":target,"code":"duplicate_name","message":"名称相同，可直接在右侧预览中调整。名称不影响身份合并。","message_en":"Matching name; edit it directly in the right preview. Names do not determine identity."}));
        }
    }
    config["_shadow_model_previews"] = json!(model_previews);
    Ok(
        json!({"config":scrub(&config),"warnings":warnings,"preference_conflicts":resolution.conflicts}),
    )
}
