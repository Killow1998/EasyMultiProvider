//! Isolated subscription jobs share the official helper. Imported subscriptions
//! use the existing account transaction; native replacement checks ownership.
use crate::app::ServerState;
use emp_codex::configuration::local_auth;
use emp_codex::quota::{LoginMethod, SubscriptionLogin};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod credentials;
pub(super) mod retained;

#[derive(Default)]
pub(crate) struct LoginSession(Mutex<LoginState>);
#[derive(Default)]
enum LoginState {
    #[default]
    Idle,
    Pending(Pending),
    Completed(Value),
    Failed(&'static str),
    Cancelled,
}
struct Pending {
    helper: SubscriptionLogin,
    expected: Option<Value>,
    account_name: Option<String>,
    started: Instant,
}

pub(crate) struct LoginOperation {
    importing: bool,
    command: LoginCommand,
}

enum LoginCommand {
    Start(Result<LoginStart, &'static str>),
    Status,
    Cancel,
    UseCredentials(Result<credentials::Source, &'static str>),
    UseLocalKey(Result<AccountAction, &'static str>),
    Unknown,
}

struct LoginStart {
    method: LoginMethod,
    account_name: Option<String>,
}

enum AccountAction {
    Keep,
    Remove,
}

impl LoginOperation {
    pub(crate) fn parse(operation: &str, body: &Value, importing: bool) -> Self {
        let command = match operation {
            "start" => LoginCommand::Start(LoginStart::parse(body, importing)),
            "status" => LoginCommand::Status,
            "cancel" => LoginCommand::Cancel,
            "use-api-key" if !importing => LoginCommand::UseCredentials(
                body["api_key"]
                    .as_str()
                    .map(str::trim)
                    .filter(|key| {
                        !key.is_empty()
                            && key.len() <= 16384
                            && key.bytes().all(|byte| byte.is_ascii_graphic())
                    })
                    .map(|key| credentials::Source::ApiKey(key.to_owned()))
                    .ok_or("login_invalid_api_key"),
            ),
            "use-account" if !importing => LoginCommand::UseCredentials(
                body["account_id"]
                    .as_str()
                    .map(|id| credentials::Source::Account(id.to_owned()))
                    .ok_or("login_account_required"),
            ),
            "use-local-key" if !importing => {
                LoginCommand::UseLocalKey(match body["account_action"].as_str() {
                    Some("keep") => Ok(AccountAction::Keep),
                    Some("remove") => Ok(AccountAction::Remove),
                    _ => Err("login_invalid_action"),
                })
            }
            _ => LoginCommand::Unknown,
        };
        Self { importing, command }
    }
}

pub(crate) fn execute(
    state: &ServerState,
    operation: LoginOperation,
) -> Result<Value, &'static str> {
    let session = if operation.importing {
        &state.backend.account_login
    } else {
        &state.backend.native_login
    };
    let mut login = session.0.lock().map_err(|_| "login_busy")?;
    match operation.command {
        LoginCommand::UseCredentials(_) | LoginCommand::UseLocalKey(_)
            if matches!(*login, LoginState::Pending(_)) =>
        {
            return Err("login_busy");
        }
        LoginCommand::UseCredentials(source) => {
            let result = credentials::switch(state, source?)?;
            *login = LoginState::Idle;
            return Ok(result);
        }
        LoginCommand::UseLocalKey(action) => {
            let result = credentials::use_local_key(state, action?)?;
            *login = LoginState::Idle;
            return Ok(result);
        }
        LoginCommand::Start(start) if !matches!(*login, LoginState::Pending(_)) => {
            let LoginStart {
                method,
                account_name,
            } = start?;
            let expected = if account_name.is_some() {
                None
            } else {
                local_auth::read(&state.backend.codex.auth_path)
                    .map_err(|_| "login_auth_unreadable")?
            };
            let helper = (|| {
                let engine = crate::services::runtime::helper_binary(state)?;
                SubscriptionLogin::start(&engine, method)
            })()
            .map_err(|error| {
                let code = match error.code() {
                    "quota_runtime_missing" => "login_engine_missing",
                    "quota_runtime_untrusted" => "login_engine_untrusted",
                    _ => "login_engine_probe_failed",
                };
                state.backend.diagnostics.journal.event(
                    "warning",
                    "codex_login_start_failed",
                    &json!({"phase":"engine_selection","code":code}),
                );
                code
            })?;
            *login = LoginState::Pending(Pending {
                helper,
                expected,
                account_name,
                started: Instant::now(),
            });
        }
        LoginCommand::Cancel => {
            *login = LoginState::Cancelled;
        }
        LoginCommand::Start(_) | LoginCommand::Status => {}
        LoginCommand::Unknown => return Err("login_unknown_operation"),
    }
    if let LoginState::Pending(pending) = &*login {
        let result = if pending.started.elapsed() > Duration::from_secs(16 * 60) {
            Some(Err("login_expired"))
        } else {
            pending.helper.take_result()
        };
        if let Some(result) = result {
            let result = result.and_then(|auth| {
                if state
                    .lifecycle
                    .shutdown
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    return Err("login_cancelled");
                }
                if let Some(name) = &pending.account_name {
                    crate::services::accounts::import_emp_login_account_state(
                        state,
                        &json!({"name":name,"auth_json":auth}),
                    )
                    .map_err(|error| {
                        if error == "core.service_name_already_in_use" {
                            "login_name_already_in_use"
                        } else {
                            "login_import_failed"
                        }
                    })?;
                    return Ok(json!({"status":"completed","restart_required":false}));
                }
                credentials::install(state, &pending.expected, &auth)
            });
            *login = match result {
                Ok(value) => LoginState::Completed(value),
                Err(code) => {
                    state.backend.diagnostics.journal.event(
                        "warning",
                        "codex_login_failed",
                        &json!({"code":code}),
                    );
                    LoginState::Failed(code)
                }
            };
        }
    }
    Ok(match &*login {
        LoginState::Idle => json!({"status":"idle"}),
        LoginState::Pending(pending) => pending.helper.snapshot(),
        LoginState::Completed(value) => value.clone(),
        LoginState::Failed(code) => json!({"status":"failed","code":code}),
        LoginState::Cancelled => json!({"status":"cancelled"}),
    })
}

impl LoginStart {
    fn parse(body: &Value, importing: bool) -> Result<Self, &'static str> {
        let method = match body["method"].as_str() {
            None | Some("device") => LoginMethod::DeviceCode,
            Some("browser") => LoginMethod::Browser,
            _ => return Err("login_invalid_method"),
        };
        let account_name = if importing {
            let name = body["name"]
                .as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty() && name.chars().count() <= 256)
                .ok_or("login_name_required")?;
            if name.contains('/') {
                return Err("login_name_contains_slash");
            }
            Some(name.to_owned())
        } else {
            None
        };
        Ok(Self {
            method,
            account_name,
        })
    }
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
    use emp_test_support::{Deadline, test_timeout};
    use std::net::{IpAddr, Ipv4Addr};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn local_key_round_trip_keeps_other_and_installs_device_login() {
        check_local_key_round_trip(false);
    }

    /// Fails if official Other login is treated as shared or retained Native credentials become owned.
    #[test]
    fn official_other_login_records_ownership_without_claiming_retained_native() {
        check_local_key_round_trip(true);
    }

    fn check_local_key_round_trip(importing: bool) {
        let root = emp_test_support::fixture_dir().unwrap();
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
        if importing {
            crate::services::accounts::import_emp_login_account_state(
                &server.state,
                &json!({"name":"Original login","auth_json":original}),
            )
            .unwrap();
        }
        let result = execute(
            &server.state,
            LoginOperation::parse("use-local-key", &json!({"account_action":"keep"}), false),
        )
        .unwrap();
        assert_eq!(result["integration_updated"], true);
        assert_eq!(local_auth::mode(&auth_path), "local_key");
        let accounts = server.state.backend.configuration.read().unwrap()["accounts"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].get("credential_source").is_none());
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
        execute(
            &server.state,
            LoginOperation::parse("start", &json!({"name":"EMP login"}), importing),
        )
        .unwrap();
        let deadline = Deadline::after(test_timeout(Duration::from_secs(15)));
        loop {
            let status = execute(
                &server.state,
                LoginOperation::parse("status", &json!({}), importing),
            )
            .unwrap();
            if status["status"] == "completed" {
                deadline.accept((), "device login completion").unwrap();
                assert_eq!(status["restart_required"], !importing);
                break;
            }
            assert_ne!(status["status"], "failed", "{status}");
            assert!(!deadline.is_expired(), "device login timed out: {status}");
            #[expect(
                clippy::disallowed_methods,
                reason = "设备登录完成通过状态接口报告，以固定间隔轮询"
            )]
            std::thread::sleep(Duration::from_millis(25));
        }
        if importing {
            assert_eq!(local_auth::mode(&auth_path), "local_key");
            let accounts =
                server.state.backend.configuration.snapshot().unwrap()["accounts"].clone();
            let accounts = accounts.as_array().unwrap();
            assert_eq!(accounts.len(), 2);
            let owned = accounts
                .iter()
                .find(|account| account["name"] == "EMP login")
                .unwrap();
            assert_eq!(owned["credential_source"], "emp_login");
            server.shutdown().unwrap();
            return;
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
        server.shutdown().unwrap();
    }
}
