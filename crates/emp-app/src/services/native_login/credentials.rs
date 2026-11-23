use super::*;

pub(super) enum Source {
    ApiKey(String),
    Account(String),
}

pub(super) fn switch(state: &ServerState, source: Source) -> Result<Value, &'static str> {
    let auth = match source {
        Source::ApiKey(key) => json!({"auth_mode":"apikey","OPENAI_API_KEY":key}),
        Source::Account(id) => {
            if state.backend.team.shared_subscription(&id) {
                return Err("login_account_required");
            }
            let config = state
                .backend
                .configuration
                .read()
                .map_err(|_| "login_storage_failed")?;
            let account = config["accounts"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|account| account["id"] == id)
                .ok_or("login_account_required")?;
            let path = account["auth_file"]
                .as_str()
                .ok_or("login_credentials_missing")?;
            let stored = state
                .backend
                .configuration
                .vault
                .read_encrypted_json(std::path::Path::new(path))
                .map_err(|_| "login_credentials_missing")?;
            emp_state::validate_auth_json(&stored).map_err(|_| "login_credentials_invalid")?
        }
    };
    let expected = local_auth::read(&state.backend.accounts.native_auth_path)
        .map_err(|_| "login_auth_unreadable")?;
    install(state, &expected, &auth)
}

pub(super) fn install(
    state: &ServerState,
    expected: &Option<Value>,
    auth: &Value,
) -> Result<Value, &'static str> {
    let manager = &state.backend.integration.manager;
    let _operation = manager.operation_lock().map_err(|_| "login_busy")?;
    if state.shutdown.load(std::sync::atomic::Ordering::Acquire) {
        return Err("login_cancelled");
    }
    if !local_auth::install(&state.backend.accounts.native_auth_path, expected, auth)
        .map_err(|_| "login_storage_failed")?
    {
        return Err("login_auth_changed");
    }
    if let Ok(mut quota) = state.backend.accounts.native_quota.lock() {
        *quota = None;
    }
    let integration_updated = !manager
        .status()
        .map_err(|_| "login_busy")?
        .relation
        .eq("applied")
        || crate::services::integration::enable::apply(state).is_ok_and(|result| result.ok());
    native_login_changed(state);
    Ok(
        json!({"status":"completed","restart_required":true,"integration_updated":integration_updated}),
    )
}
pub(super) fn use_local_key(
    state: &ServerState,
    action: AccountAction,
) -> Result<Value, &'static str> {
    let manager = &state.backend.integration.manager;
    let _operation = manager.operation_lock().map_err(|_| "login_busy")?;
    let path = &state.backend.accounts.native_auth_path;
    let auth = local_auth::read(path)
        .map_err(|_| "login_auth_unreadable")?
        .ok_or("login_auth_unreadable")?;
    let active = manager.status().map_err(|_| "login_busy")?.relation == "applied";
    let retained = match action {
        AccountAction::Keep => retained::import_native(state, &auth)?,
        AccountAction::Remove => retained::Selection::Removed,
    };
    let mut files = emp_state::FileTransaction::new();
    files
        .remember(&retained::path(state))
        .map_err(|_| "login_storage_failed")?;
    retained.save(state).map_err(|_| "login_storage_failed")?;
    if !local_auth::replace_with_local(path, &auth, &state.backend.gateway_key)
        .map_err(|_| "login_storage_failed")?
    {
        return Err("login_auth_changed");
    }
    files.commit();
    if let Ok(mut quota) = state.backend.accounts.native_quota.lock() {
        *quota = None;
    }
    let integration_updated = !active
        || (manager.restore().is_ok_and(|result| result.ok())
            && crate::services::integration::enable::apply(state).is_ok_and(|result| result.ok()));
    native_login_changed(state);
    Ok(
        json!({"status":"completed", "restart_required":true, "integration_updated":integration_updated}),
    )
}
