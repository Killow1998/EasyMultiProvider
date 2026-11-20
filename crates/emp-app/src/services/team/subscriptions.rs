use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use emp_modules::workspace::{NATIVE_SHARE, SharePayload, SharedService, SubscriptionSnapshot};
use emp_state::{ConfigError, filesystem::FileTransaction};

pub(super) fn publish(state: &ServerState) -> Result<SharePayload> {
    let path = &state.backend.accounts.native_auth_path;
    let auth = (emp_integration::local_auth::mode(path) == "subscription")
        .then(|| crate::services::accounts::native_auth_document(path))
        .flatten();
    let quota_plan = state
        .backend
        .accounts
        .native_quota
        .lock()
        .ok()
        .and_then(|quota| quota.as_ref()?["plan_type"].as_str().map(str::to_owned));
    let mut snapshot = auth
        .as_ref()
        .and_then(subscription)
        .filter(|auth| auth.expires_at > crate::util::system_now() as i64)
        .ok_or_else(|| Failure(409, "workspace_subscription_sign_in_required".into()))?;
    snapshot.plan = quota_plan.as_deref().and_then(plan).or(snapshot.plan);
    Ok(SharePayload {
        provider: json!({"id":NATIVE_SHARE,"name":"Native","auth_mode":"codex_subscription"}),
        models: Vec::new(),
        subscription: Some(snapshot),
    })
}

fn subscription(auth: &Value) -> Option<SubscriptionSnapshot> {
    let headers = emp_codex::account_auth_headers(auth)?;
    let access_token = headers.get("Authorization")?.strip_prefix("Bearer ")?;
    let claims = URL_SAFE_NO_PAD
        .decode(access_token.split('.').nth(1)?)
        .ok()?;
    let claims: Value = serde_json::from_slice(&claims).ok()?;
    let account_id = headers.get("chatgpt-account-id")?;
    let tokens = auth.get("tokens").unwrap_or(auth);
    Some(SubscriptionSnapshot {
        access_token: access_token.to_owned(),
        id_token: tokens["id_token"].as_str().map(str::to_owned),
        account_id: account_id.to_owned(),
        expires_at: claims["exp"].as_i64()?,
        plan: claims["https://api.openai.com/auth"]["chatgpt_plan_type"]
            .as_str()
            .and_then(plan),
    })
}

fn plan(value: &str) -> Option<String> {
    let key = value.to_ascii_lowercase().replace([' ', '-', '_'], "");
    let plan = match key.as_str() {
        "prolite" | "litepro" => "prolite",
        "promax" => "promax",
        "team" | "enterprise" | "edu" | "business" => "business",
        "free" | "go" | "plus" | "pro" => key.as_str(),
        _ => return None,
    };
    Some(plan.to_owned())
}

pub(super) fn duplicates_native(native: &Value, auth: &SubscriptionSnapshot) -> bool {
    emp_state::same_account_auth(native, &document(auth))
}

fn document(auth: &SubscriptionSnapshot) -> Value {
    json!({"auth_mode":"chatgptAuthTokens","tokens":{
        "access_token":auth.access_token,"account_id":auth.account_id,
        "id_token":auth.id_token.as_deref().unwrap_or(&auth.access_token),"refresh_token":""
    }})
}

pub(super) fn owns_credential(account: &Value, path: &std::path::Path) -> bool {
    account["workspace_shared"] == true
        && account["auth_file"]
            .as_str()
            .is_some_and(|configured| std::path::Path::new(configured) == path)
}

pub(super) fn stage(
    state: &ServerState,
    updated: &mut Value,
    session: &Session,
    local_id: &str,
    existing: Option<&Imported>,
    share: &SharedService,
    credentials: &mut Vec<(PathBuf, Value)>,
) -> Result<bool> {
    let auth = share
        .payload
        .subscription
        .as_ref()
        .expect("subscription share");
    let document = document(auth);
    if subscription(&document).is_none_or(|snapshot| snapshot.expires_at != auth.expires_at)
        || auth.expires_at <= crate::util::system_now() as i64
    {
        return Err(Failure(502, "invalid_workspace_share".into()));
    }
    let path =
        emp_state::account_auth_path(updated, local_id, &state.backend.configuration.config_path)
            .map_err(|_| Failure::unavailable())?;
    let accounts = updated["accounts"].as_array().expect("normalized accounts");
    let index = accounts
        .iter()
        .position(|account| account["id"] == local_id);
    if updated["providers"]
        .as_array()
        .expect("normalized providers")
        .iter()
        .any(|provider| provider["id"] == local_id)
        || index.is_some_and(|index| !owns_credential(&accounts[index], &path))
        || (index.is_none() && existing.is_none() && path.exists())
    {
        return Err(Failure(409, "workspace_share_collision".into()));
    }
    let label = share.owner.as_deref().unwrap_or("Native");
    let old_default = format!(
        "{label} ({})",
        session.identity["workspace"]["name"]
            .as_str()
            .unwrap_or("Team")
    )
    .replace(['/', '\\'], " ")
    .chars()
    .take(54)
    .collect::<String>();
    let renamed = index.is_none_or(|index| {
        let name = accounts[index]["name"].as_str().expect("normalized name");
        name == old_default
            || name
                .strip_prefix(&old_default)
                .and_then(|suffix| suffix.strip_prefix(' '))
                .is_some_and(|suffix| suffix.parse::<u32>().is_ok())
    });
    let new_name = renamed.then(|| {
        let preferred = label.replace(['/', '\\'], " ");
        let preferred = preferred.trim();
        let preferred = if preferred.is_empty() {
            "Codex"
        } else {
            preferred
        };
        let names: std::collections::BTreeSet<_> = accounts
            .iter()
            .chain(
                updated["providers"]
                    .as_array()
                    .expect("normalized providers"),
            )
            .filter(|item| item["id"] != local_id)
            .flat_map(|item| {
                item["route_names"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .chain(item["name"].as_str())
                    .chain(item["prefix"].as_str())
                    .chain(item["id"].as_str())
            })
            .map(|name| name.trim().to_lowercase())
            .collect();
        if names.contains(&preferred.to_lowercase()) {
            (2..)
                .map(|n| format!("{preferred} {n}"))
                .find(|name| !names.contains(&name.to_lowercase()))
                .expect("available service name")
        } else {
            preferred.to_owned()
        }
    });
    if let Some(record) = existing {
        if record.workspace != session.workspace() || record.share != share.id {
            return Err(Failure(409, "workspace_share_collision".into()));
        }
        if record.version > share.version {
            return Err(Failure(409, "workspace_share_version_conflict".into()));
        }
        if record.version == share.version
            && index.is_some_and(|index| accounts[index]["credential_status"] != "invalid")
            && state
                .backend
                .configuration
                .vault
                .read_encrypted_json(&path)
                .is_ok_and(|auth| auth == document)
        {
            if let (Some(index), Some(name)) = (index, new_name) {
                updated["accounts"][index]["name"] = json!(name);
            }
            return Ok(false);
        }
    }
    let accounts = updated["accounts"]
        .as_array_mut()
        .expect("normalized accounts");
    if let Some(index) = index {
        if let Some(name) = new_name {
            accounts[index]["name"] = json!(name);
        }
        accounts[index]["credential_status"] = json!("unknown");
        accounts[index]["auth_file"] = json!(path);
        accounts[index]["workspace_shared"] = json!(true);
    } else {
        let name = new_name.expect("name for new team subscription");
        accounts.push(json!({"id":local_id,"prefix":local_id,"name":name,"auth_file":path,"workspace_shared":true}));
    }
    credentials.push((path, document));
    Ok(true)
}

pub(super) fn persist(
    vault: &VaultStore,
    files: &mut FileTransaction,
    credentials: Vec<(PathBuf, Value)>,
) -> std::result::Result<(), ConfigError> {
    for (path, auth) in credentials {
        let config = path.with_file_name("config.toml");
        files.remember(&path)?;
        files.remember(&config)?;
        vault.write_encrypted_json(&path, &auth)?;
        emp_state::atomic_write_private_state(&config, b"cli_auth_credentials_store = \"file\"\n")?;
    }
    Ok(())
}
