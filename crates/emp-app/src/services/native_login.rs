//! Isolated device-code jobs share the official helper. Imported subscriptions
//! use the existing account transaction; native replacement checks ownership.
use crate::app::ServerState;
use emp_codex::quota::DeviceLogin;
use emp_integration::local_auth;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct DeviceLoginSession(Mutex<LoginState>);
#[derive(Default)]
struct LoginState {
    pending: Option<Pending>,
    outcome: Value,
}
struct Pending {
    helper: DeviceLogin,
    _engine: Option<crate::services::runtime::HelperBinary>,
    expected: Option<Value>,
    account_name: Option<String>,
    started: Instant,
}

pub(crate) fn execute(
    state: &ServerState,
    operation: &str,
    body: &Value,
    importing: bool,
) -> Result<Value, &'static str> {
    let session = if importing {
        &state.backend.account_login
    } else {
        &state.backend.native_login
    };
    let mut login = session.0.lock().map_err(|_| "login_busy")?;
    match operation {
        "start" if login.pending.is_none() => {
            let (expected, account_name) = if importing {
                let name = body["name"]
                    .as_str()
                    .map(str::trim)
                    .filter(|name| !name.is_empty() && name.chars().count() <= 256)
                    .ok_or("login_name_required")?;
                (None, Some(name.to_owned()))
            } else {
                (
                    local_auth::read(&state.backend.accounts.native_auth_path)
                        .map_err(|_| "login_auth_unreadable")?,
                    None,
                )
            };
            let engine = crate::services::runtime::helper_binary(state).ok();
            let selector = engine
                .as_deref()
                .unwrap_or(&state.backend.accounts.codex_binary);
            let helper = DeviceLogin::start(selector).map_err(|_| "login_engine_unavailable")?;
            login.pending = Some(Pending {
                helper,
                _engine: engine,
                expected,
                account_name,
                started: Instant::now(),
            });
            login.outcome = Value::Null;
        }
        "cancel" => {
            login.pending = None;
            login.outcome = json!({"status":"cancelled"});
        }
        "start" | "status" => {}
        _ => return Err("login_unknown_operation"),
    }
    if let Some(pending) = &login.pending {
        let result = if pending.started.elapsed() > Duration::from_secs(16 * 60) {
            Some(Err("login_expired"))
        } else {
            pending.helper.take_result()
        };
        if let Some(result) = result {
            let result = result.and_then(|auth| {
                if state.shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    return Err("login_cancelled");
                }
                if let Some(name) = &pending.account_name {
                    crate::services::accounts::import_account_state(
                        None,
                        state,
                        &json!({"name":name,"auth_json":auth}),
                    )
                    .map_err(|_| "login_import_failed")?;
                    return Ok(());
                }
                let manager = &state.backend.integration.manager;
                let _operation = manager.operation_lock().map_err(|_| "login_busy")?;
                if state.shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    return Err("login_cancelled");
                }
                let installed = local_auth::install_subscription(
                    &state.backend.accounts.native_auth_path,
                    &pending.expected,
                    &auth,
                )
                .map_err(|_| "login_storage_failed")?;
                if !installed {
                    return Err("login_auth_changed");
                }
                if let Ok(mut quota) = state.backend.accounts.native_quota.lock() {
                    *quota = None;
                }
                if manager
                    .status()
                    .is_ok_and(|status| status.relation == "applied")
                {
                    let _ = crate::services::integration::enable::apply(state);
                }
                crate::services::runtime::mark_active_pending(
                    state,
                    "Codex login changed; restart Codex Runtime",
                );
                state
                    .backend
                    .management_events
                    .publish(crate::services::management_events::Change::Integration);
                Ok(())
            });
            login.pending = None;
            login.outcome = match result {
                Ok(()) => json!({"status":"completed","restart_required":!importing}),
                Err(code) => json!({"status":"failed","code":code}),
            };
        }
    }
    Ok(login.pending.as_ref().map_or_else(
        || {
            if login.outcome.is_null() {
                json!({"status":"idle"})
            } else {
                login.outcome.clone()
            }
        },
        |pending| pending.helper.snapshot(),
    ))
}
