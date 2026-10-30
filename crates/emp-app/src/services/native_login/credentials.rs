use super::*;

pub(super) fn switch(
    state: &ServerState,
    operation: &str,
    body: &Value,
) -> Result<Value, &'static str> {
    let auth = match operation {
        "use-api-key" => {
            let key = body["api_key"]
                .as_str()
                .map(str::trim)
                .filter(|key| {
                    !key.is_empty()
                        && key.len() <= 16384
                        && key.bytes().all(|byte| byte.is_ascii_graphic())
                })
                .ok_or("login_invalid_api_key")?;
            json!({"auth_mode":"apikey","OPENAI_API_KEY":key})
        }
        "use-account" => {
            let id = body["account_id"]
                .as_str()
                .ok_or("login_account_required")?;
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
        _ => unreachable!("validated credential operation"),
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
