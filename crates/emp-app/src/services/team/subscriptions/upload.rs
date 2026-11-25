use super::{plan, stored_auth, subscription};
use crate::services::team::{Failure, Result};
use crate::{app::ServerState, services::native_login::retained::Selection};
use emp_modules::workspace::{NATIVE_SHARE, SharePayload, SubscriptionSnapshot};
use serde_json::{Value, json};

struct UploadSource {
    name: String,
    account_id: String,
    identity: String,
    snapshot: SubscriptionSnapshot,
}

impl UploadSource {
    fn details(&self) -> Value {
        json!({"id":self.account_id,"name":self.name,"plan":self.snapshot.plan})
    }
}

enum Upload {
    Native(SubscriptionSnapshot),
    Retained(UploadSource),
    Choose(Vec<UploadSource>),
    SignIn,
}

fn upload_source(state: &ServerState) -> Result<Upload> {
    let path = &state.backend.codex.auth_path;
    let document = crate::services::accounts::native_auth_document(path);
    let now = crate::util::system_now() as i64;
    let Some(auth) = document else {
        return Ok(Upload::SignIn);
    };
    if !emp_codex::configuration::local_auth::is_local(&auth) {
        let Some(mut snapshot) = subscription(&auth).filter(|snapshot| snapshot.expires_at > now)
        else {
            return Ok(Upload::SignIn);
        };
        snapshot.plan = state
            .backend
            .quota
            .with_native_quota(|quota| plan(quota["plan_type"].as_str()?))
            .flatten()
            .or(snapshot.plan);
        return Ok(Upload::Native(snapshot));
    }

    let stored = Selection::load(state).map_err(|_| Failure::unavailable())?;
    if matches!(stored, Some(Selection::Removed)) {
        return Ok(Upload::SignIn);
    }
    let settings = &state.backend.configuration;
    let config = settings.read().map_err(|_| Failure::unavailable())?;
    let mut options = Vec::new();
    for account in config["accounts"].as_array().expect("normalized accounts") {
        if account["workspace_shared"] == true || account["credential_status"] == "invalid" {
            continue;
        }
        let id = account["id"].as_str().expect("normalized account ID");
        if let Some(Selection::Kept { account_id, .. }) = &stored
            && account_id != id
        {
            continue;
        }
        let Some(auth) = stored_auth(&settings.vault, account) else {
            continue;
        };
        let Some(snapshot) = subscription(&auth).filter(|snapshot| snapshot.expires_at > now)
        else {
            continue;
        };
        let source = UploadSource {
            name: account["name"]
                .as_str()
                .expect("normalized account name")
                .into(),
            account_id: id.into(),
            identity: emp_state::accounts::account_auth_identity(&auth),
            snapshot,
        };
        if let Some(Selection::Kept { identity, .. }) = &stored {
            if source.identity == *identity {
                return Ok(Upload::Retained(source));
            }
        } else {
            options.push(source);
        }
    }
    Ok(if options.is_empty() {
        Upload::SignIn
    } else {
        Upload::Choose(options)
    })
}

pub(in crate::services::team) fn upload_details(state: &ServerState) -> Result<Value> {
    Ok(match upload_source(state)? {
        Upload::Native(snapshot) => {
            json!({"source":{"id":null,"name":"Native · Codex","plan":snapshot.plan}})
        }
        Upload::Retained(source) => json!({"source":source.details()}),
        Upload::Choose(options) => {
            json!({"choices":options.iter().map(UploadSource::details).collect::<Vec<_>>()})
        }
        Upload::SignIn => json!({}),
    })
}

pub(in crate::services::team) fn publish(
    state: &ServerState,
    account_id: Result<Option<String>>,
) -> Result<SharePayload> {
    let _operation = state
        .backend
        .integration
        .manager
        .operation_lock()
        .map_err(|_| Failure::unavailable())?;
    let account_id = account_id?;
    let selected = account_id.as_deref();
    let invalid_selection = || Failure(409, "workspace_subscription_selection_invalid".into());
    let snapshot = match upload_source(state)? {
        Upload::Native(snapshot) if selected.is_none() => snapshot,
        Upload::Retained(source)
            if selected.is_none_or(|selected| selected == source.account_id) =>
        {
            source.snapshot
        }
        Upload::Native(_) | Upload::Retained(_) => return Err(invalid_selection()),
        Upload::Choose(options) => {
            let selected = selected
                .ok_or_else(|| Failure(409, "workspace_subscription_selection_required".into()))?;
            let source = options
                .into_iter()
                .find(|source| source.account_id == selected)
                .ok_or_else(invalid_selection)?;
            Selection::Kept {
                account_id: source.account_id,
                identity: source.identity,
            }
            .save(state)
            .map_err(|_| Failure::unavailable())?;
            source.snapshot
        }
        Upload::SignIn => {
            return Err(Failure(
                409,
                "workspace_subscription_sign_in_required".into(),
            ));
        }
    };
    Ok(SharePayload {
        provider: json!({"id":NATIVE_SHARE,"name":"Native","auth_mode":"codex_subscription"}),
        models: Vec::new(),
        subscription: Some(snapshot),
    })
}
