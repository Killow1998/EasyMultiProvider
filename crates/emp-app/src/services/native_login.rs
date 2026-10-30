//! Isolated subscription jobs share the official helper. Imported subscriptions
//! use the existing account transaction; native replacement checks ownership.
use crate::app::ServerState;
use emp_codex::quota::{LoginMethod, SubscriptionLogin};
use emp_integration::local_auth;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod credentials;

#[derive(Default)]
pub(crate) struct LoginSession(Mutex<LoginState>);
#[derive(Default)]
struct LoginState {
    pending: Option<Pending>,
    outcome: Value,
}
struct Pending {
    helper: SubscriptionLogin,
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
    if !importing && matches!(operation, "use-api-key" | "use-account") {
        if login.pending.is_some() {
            return Err("login_busy");
        }
        let result = credentials::switch(state, operation, body)?;
        login.outcome = Value::Null;
        return Ok(result);
    }
    if !importing && operation == "use-local-key" {
        if login.pending.is_some() {
            return Err("login_busy");
        }
        let keep = match body["account_action"].as_str() {
            Some("keep") => true,
            Some("remove") => false,
            _ => return Err("login_invalid_action"),
        };
        let manager = &state.backend.integration.manager;
        let _operation = manager.operation_lock().map_err(|_| "login_busy")?;
        let path = &state.backend.accounts.native_auth_path;
        let auth = local_auth::read(path)
            .map_err(|_| "login_auth_unreadable")?
            .ok_or("login_auth_unreadable")?;
        let active = manager.status().map_err(|_| "login_busy")?.relation == "applied";
        if keep {
            emp_state::validate_auth_json(&auth).map_err(|_| "login_credentials_invalid")?;
            let config = state
                .backend
                .configuration
                .read()
                .map_err(|_| "login_storage_failed")?;
            let existing = crate::services::accounts::matching_account(
                &config,
                &state.backend.configuration.vault,
                &auth,
            )
            .cloned();
            drop(config);
            let mut imported = existing.unwrap_or_else(|| json!({"name":"Native"}));
            imported["auth_json"] = auth.clone();
            crate::services::accounts::import_account_state(None, state, &imported)
                .map_err(|_| "login_import_failed")?;
        }
        if !local_auth::replace_with_local(path, &auth, &state.backend.gateway_key)
            .map_err(|_| "login_storage_failed")?
        {
            return Err("login_auth_changed");
        }
        if let Ok(mut quota) = state.backend.accounts.native_quota.lock() {
            *quota = None;
        }
        let integration_updated = !active
            || (manager.restore().is_ok_and(|result| result.ok())
                && crate::services::integration::enable::apply(state)
                    .is_ok_and(|result| result.ok()));
        native_login_changed(state);
        login.outcome = Value::Null;
        return Ok(
            json!({"status":"completed", "restart_required":true, "integration_updated":integration_updated}),
        );
    }
    match operation {
        "start" if login.pending.is_none() => {
            let method = match body["method"].as_str() {
                None | Some("device") => LoginMethod::DeviceCode,
                Some("browser") => LoginMethod::Browser,
                _ => return Err("login_invalid_method"),
            };
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
            let helper = SubscriptionLogin::start(selector, method)
                .map_err(|_| "login_engine_unavailable")?;
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
                    return Ok(json!({"status":"completed","restart_required":false}));
                }
                credentials::install(state, &pending.expected, &auth)
            });
            login.pending = None;
            login.outcome = match result {
                Ok(value) => value,
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

fn native_login_changed(state: &ServerState) {
    crate::services::runtime::mark_active_pending(
        state,
        "Codex login changed; restart Codex Runtime",
    );
    state
        .backend
        .management_events
        .publish(crate::services::management_events::Change::Integration);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::lifecycle::ServerHandle;
    use std::net::{IpAddr, Ipv4Addr};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn local_key_round_trip_keeps_other_and_installs_device_login() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        std::fs::write(&config, b"{}").unwrap();
        let home = root.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        let auth_path = home.join("auth.json");
        let original = json!({"tokens":{"account_id":"original","access_token":"fixture-token"}});
        emp_state::atomic_write_private_state(&auth_path, &serde_json::to_vec(&original).unwrap())
            .unwrap();
        let helper = root.path().join("codex-fixture");
        std::fs::write(
            &helper,
            include_bytes!("../../../../scripts/shadow_frontend/fixtures/device-codex.py"),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.path().join("login-control"), "complete").unwrap();
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config,
            helper.to_str().unwrap(),
            auth_path.clone(),
        )
        .unwrap();
        let result = execute(
            &server.state,
            "use-local-key",
            &json!({"account_action":"keep"}),
            false,
        )
        .unwrap();
        assert_eq!(result["integration_updated"], true);
        assert_eq!(local_auth::mode(&auth_path), "local_key");
        let accounts = server.state.backend.configuration.read().unwrap()["accounts"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(accounts.len(), 1);
        let retained = server
            .state
            .backend
            .configuration
            .vault
            .read_encrypted_json(std::path::Path::new(
                accounts[0]["auth_file"].as_str().unwrap(),
            ))
            .unwrap();
        assert!(emp_state::same_account_auth(&retained, &original));
        execute(&server.state, "start", &json!({}), false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = execute(&server.state, "status", &json!({}), false).unwrap();
            if status["status"] == "completed" {
                assert_eq!(status["restart_required"], true);
                break;
            }
            assert_ne!(status["status"], "failed", "{status}");
            assert!(
                Instant::now() < deadline,
                "device login timed out: {status}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(local_auth::mode(&auth_path), "subscription");
        assert_eq!(
            local_auth::read(&auth_path).unwrap().unwrap()["tokens"]["account_id"],
            "fixture-login"
        );
        assert_eq!(
            server.state.backend.configuration.read().unwrap()["accounts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
