use super::*;
use emp_state::filesystem::{create_private_directory, write_new_private_file};
use serde_json::json;
use std::fs;

fn fixture() -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    create_private_directory(&state).unwrap();
    let vault = VaultStore::from_sources(None, &state.join("master.key")).unwrap();
    let credential = state.join("old.enc");
    vault
        .write_encrypted_text(&credential, "synthetic-secret")
        .unwrap();
    let config = dir.path().join("config.json");
    fs::write(
        &config,
        json!({"providers":[{"api_key_file":credential}]}).to_string(),
    )
    .unwrap();
    let key = fs::read(state.join("master.key")).unwrap();
    (dir, config, key)
}

#[test]
fn missing_key_does_not_silently_replace_saved_credentials_key() {
    let (dir, config, _) = fixture();
    let key = dir.path().join("state/master.key");
    fs::remove_file(&key).unwrap();
    let config: Value = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
    assert!(matches!(
        open_vault_with(&config, &dir.path().join("state"), false),
        Err(AppError::CredentialKeyMissing)
    ));
    assert!(!key.exists());
}

#[test]
fn saved_device_keys_use_credential_recovery_without_rotating_the_master_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    create_private_directory(&state).unwrap();
    let path = state.join("gateway-clients.json");
    fs::write(&path, b"[]").unwrap();
    let config = json!({"providers": [], "accounts": []});
    let vault = open_vault_with(&config, &state, false).unwrap();
    let clients =
        crate::services::gateway_clients::GatewayClients::open(path.clone(), &vault).unwrap();
    use crate::services::modules::{Modules, Startup};
    let modules = Modules::open(state.join("modules"), Startup::Normal).unwrap();
    let saved =
        serde_json::to_value(clients.issue("Test PC", None, &vault, &modules).unwrap()).unwrap();
    fs::remove_file(state.join("master.key")).unwrap();
    assert!(matches!(
        open_vault_with(&config, &state, false),
        Err(AppError::CredentialKeyMissing)
    ));
    assert!(!state.join("master.key").exists());
    let wrong = VaultStore::from_sources(None, &state.join("master.key")).unwrap();
    assert!(matches!(
        open_vault_with(&config, &state, false),
        Err(AppError::Filesystem(
            FilesystemError::CredentialDecryptFailed
        ))
    ));
    let error = crate::services::gateway_clients::GatewayClients::open(path.clone(), &wrong)
        .err()
        .unwrap();
    assert!(recoverable(&error));
    let restored = crate::services::gateway_clients::GatewayClients::open(path, &vault).unwrap();
    assert_eq!(
        serde_json::to_value(restored.list()).unwrap()[0]["api_key"],
        saved["api_key"]
    );
}

#[test]
fn mismatched_key_is_visible_but_one_bad_account_does_not_block_good_accounts() {
    let (dir, config, _) = fixture();
    let state = dir.path().join("state");
    fs::remove_file(state.join("master.key")).unwrap();
    let vault = VaultStore::from_sources(None, &state.join("master.key")).unwrap();
    let mut config: Value = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
    assert!(matches!(
        open_vault_with(&config, &state, false),
        Err(AppError::Filesystem(
            FilesystemError::CredentialDecryptFailed
        ))
    ));
    let good = state.join("good.enc");
    vault
        .write_encrypted_text(&good, "another-fixture")
        .unwrap();
    config["accounts"] = json!([{"auth_file":good}]);
    assert!(open_vault_with(&config, &state, false).is_ok());
}

#[test]
fn reset_backup_is_optional_and_native_and_external_files_are_untouched() {
    for backup in [false, true] {
        let (dir, config, key) = fixture();
        let before = fs::read(&config).unwrap();
        let state = dir.path().join("state");
        let _owner = emp_state::IntegrationFileLock::acquire(
            &state.join("service.lock"),
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        )
        .unwrap();
        let native = dir.path().join("native-auth.json");
        fs::write(&native, "native-kept").unwrap();
        let (saved, cleanup_pending) = reset::rebuild_with(&config, backup, || Ok(())).unwrap();
        assert!(!cleanup_pending);
        assert_eq!(saved.is_some(), backup);
        assert_eq!(fs::read_to_string(native).unwrap(), "native-kept");
        assert!(!state.join("old.enc").exists());
        assert!(check_pending(&config).is_ok());
        assert!(
            emp_state::IntegrationFileLock::acquire(
                &state.join("service.lock"),
                std::time::Duration::ZERO,
                std::time::Duration::ZERO
            )
            .is_err()
        );
        let fresh = emp_state::load_configuration(Some(&config)).unwrap();
        assert!(fresh["providers"].as_array().unwrap().is_empty());
        assert_eq!(fresh["auto_enable_on_start"], false);
        assert_ne!(fs::read(state.join("master.key")).unwrap(), key);
        if let Some(saved) = saved {
            assert_eq!(fs::read(saved.join("config.json")).unwrap(), before);
            assert_eq!(fs::read(saved.join("state/master.key")).unwrap(), key);
            let old = VaultStore::from_sources(None, &saved.join("state/master.key")).unwrap();
            assert_eq!(
                old.read_encrypted_text(&saved.join("state/old.enc"))
                    .unwrap()
                    .as_str(),
                "synthetic-secret"
            );
        } else {
            assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("recovery-")
            }));
        }
    }
}

#[test]
fn failed_install_rolls_back_exact_old_files_even_without_selected_backup() {
    let (dir, config, key) = fixture();
    let before = fs::read(&config).unwrap();
    let state = dir.path().join("state");
    let credential = fs::read(state.join("old.enc")).unwrap();
    assert_eq!(
        reset::rebuild_with(&config, false, || Err("injected_install_failure")),
        Err("injected_install_failure")
    );
    assert_eq!(fs::read(&config).unwrap(), before);
    assert_eq!(fs::read(state.join("master.key")).unwrap(), key);
    assert_eq!(fs::read(state.join("old.enc")).unwrap(), credential);
    assert!(check_pending(&config).is_ok());
}

#[test]
fn incomplete_marker_blocks_startup_but_confirmed_reset_recovers() {
    let (dir, config, _) = fixture();
    write_new_private_file(&dir.path().join("state/recovery-reset.json"), b"{}").unwrap();
    assert!(matches!(
        check_pending(&config),
        Err(AppError::RecoveryIncomplete)
    ));
    reset::rebuild_with(&config, false, || Ok(())).unwrap();
    assert!(check_pending(&config).is_ok());
    let fresh = emp_state::load_configuration(Some(&config)).unwrap();
    assert!(open_vault_with(&fresh, &dir.path().join("state"), false).is_ok());
}

#[test]
fn interrupted_moves_can_be_reset_again_with_or_without_backup() {
    for keep in [false, true] {
        for phase in 0..=4 {
            let (dir, config, old_key) = fixture();
            let state = dir.path().join("state");
            let saved = dir.path().join("recovery-staging-0123456789abcdef01234567");
            create_private_directory(&saved.join("state")).unwrap();
            write_new_private_file(
                &state.join("recovery-reset.json"),
                &serde_json::to_vec(&json!({
                    "config_path":config,"saved_files":saved,"keep_backup":false
                }))
                .unwrap(),
            )
            .unwrap();
            if phase >= 1 {
                fs::rename(&config, saved.join("config.json")).unwrap();
            }
            if phase >= 2 {
                fs::rename(state.join("master.key"), saved.join("state/master.key")).unwrap();
            }
            if phase >= 3 {
                VaultStore::from_sources(None, &state.join("master.key")).unwrap();
            }
            if phase >= 4 {
                fs::write(&config, b"{}").unwrap();
            }
            let (backup, cleanup) = reset::rebuild_with(&config, keep, || Ok(())).unwrap();
            assert!(!cleanup);
            assert_eq!(backup.is_some(), keep);
            assert_eq!(saved.exists(), keep);
            assert!(check_pending(&config).is_ok());
            let fresh = emp_state::load_configuration(Some(&config)).unwrap();
            assert!(fresh["providers"].as_array().unwrap().is_empty());
            assert!(open_vault_with(&fresh, &state, false).is_ok());
            assert_ne!(fs::read(state.join("master.key")).unwrap(), old_key);
            if keep && phase >= 2 {
                assert_eq!(fs::read(saved.join("state/master.key")).unwrap(), old_key);
            }
        }
    }
}

#[test]
fn failed_repeat_reset_preserves_pending_data_and_can_continue() {
    let (dir, config, key) = fixture();
    let marker = dir.path().join("state/recovery-reset.json");
    write_new_private_file(&marker, b"{}").unwrap();
    assert_eq!(
        reset::rebuild_with(&config, false, || Err("injected_failure")),
        Err("injected_failure")
    );
    assert_eq!(fs::read(&marker).unwrap(), b"{}");
    assert_eq!(fs::read(dir.path().join("state/master.key")).unwrap(), key);
    reset::rebuild_with(&config, false, || Ok(())).unwrap();
    assert!(check_pending(&config).is_ok());
}

#[test]
fn pending_marker_never_authorizes_clearing_an_external_directory() {
    let (dir, config, _) = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), b"untouched").unwrap();
    write_new_private_file(
        &dir.path().join("state/recovery-reset.json"),
        &serde_json::to_vec(&json!({
            "config_path":config,"saved_files":outside.path()
        }))
        .unwrap(),
    )
    .unwrap();
    reset::rebuild_with(&config, false, || Ok(())).unwrap();
    assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"untouched");
}

#[cfg(unix)]
#[test]
fn recovery_refuses_config_symlinks_and_never_deletes_link_targets() {
    use std::os::unix::fs::symlink;
    let (dir, config, _) = fixture();
    let outside = dir.path().join("outside.json");
    fs::write(&outside, "keep-outside").unwrap();
    fs::remove_file(&config).unwrap();
    symlink(&outside, &config).unwrap();
    assert_eq!(
        reset::rebuild_with(&config, false, || Ok(())),
        Err("unsafe_path")
    );
    fs::remove_file(&config).unwrap();
    fs::write(&config, "invalid-old-config").unwrap();
    symlink(&outside, dir.path().join("state/external-link")).unwrap();
    reset::rebuild_with(&config, false, || Ok(())).unwrap();
    assert_eq!(fs::read_to_string(outside).unwrap(), "keep-outside");
}

#[test]
fn recovery_routes_match_legacy_bytes_before_reset_or_exit() {
    use crate::server::{
        ServerControl,
        http::tests::{assert_status, exchange_wire, json_body, request},
    };
    use crate::services::connection_admission::{ConnectionAdmission, ConnectionAdmissionConfig};
    use std::{net::TcpListener, sync::Arc};

    for restart in [false, true] {
        let (old_dir, old_config, _) = fixture();
        let (new_dir, new_config, _) = fixture();
        let old_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let new_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let (old, new) = (
            old_listener.local_addr().unwrap(),
            new_listener.local_addr().unwrap(),
        );
        let legacy_config = old_config.clone();
        let candidate_config = new_config.clone();
        let legacy = std::thread::spawn(move || {
            super::serve_for_test(old_listener, &legacy_config, "recovery-fixture").unwrap()
        });
        let candidate = std::thread::spawn(move || {
            server::serve_candidate(
                new_listener,
                &candidate_config,
                "config_invalid",
                "recovery-fixture",
                Arc::new(ServerControl::recovery(Arc::new(ConnectionAdmission::new(
                    ConnectionAdmissionConfig::default(),
                )))),
            )
            .unwrap()
        });
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let auth = "X-EMP-Recovery: recovery-fixture";
                let cases = [
                    ("GET", "/", vec![], 200),
                    ("GET", "/healthz", vec![], 503),
                    ("GET", "/assets/i18n.js", vec![], 200),
                    ("GET", "/assets/copy-recovery.js", vec![], 200),
                    ("GET", "/api/recovery", vec![auth], 200),
                    ("GET", "/api/recovery", vec![], 401),
                    ("GET", "/api/recovery?extra=1", vec![auth], 400),
                    ("HEAD", "/", vec![auth], 400),
                    ("GET", "/", vec!["Origin: https://other.invalid"], 403),
                    (
                        "POST",
                        "/api/recovery/reset",
                        vec![auth, "Content-Length: 0"],
                        409,
                    ),
                    (
                        "POST",
                        "/api/recovery/exit",
                        vec![auth, "Content-Length: 1"],
                        400,
                    ),
                    (
                        "POST",
                        "/api/recovery/reset",
                        vec!["Content-Length: 0"],
                        401,
                    ),
                    ("POST", "/api/recovery/exit", vec!["Content-Length: 0"], 401),
                ];
                for (method, path, headers, status) in cases {
                    let a = request(old.port(), method, path, &headers, b"");
                    let b = request(new.port(), method, path, &headers, b"");
                    let (a, b) =
                        tokio::join!(exchange_wire(old, &a, true), exchange_wire(new, &b, true));
                    assert_status(&a, status);
                    assert_eq!(a, b, "{method} {path}");
                }
                let path = if restart {
                    "/api/recovery/reset"
                } else {
                    "/api/recovery/exit"
                };
                let headers = [
                    auth,
                    "Content-Length: 0",
                    "X-EMP-Recovery-Confirm: clear-and-reset",
                ];
                let a = request(old.port(), "POST", path, &headers, b"");
                let b = request(new.port(), "POST", path, &headers, b"");
                let (a, b) =
                    tokio::join!(exchange_wire(old, &a, true), exchange_wire(new, &b, true));
                assert_status(&a, 200);
                assert_eq!(a, b);
                let response = json_body(&b);
                assert_eq!(
                    response[if restart { "restarting" } else { "exiting" }],
                    true
                );
            });
        assert_eq!(legacy.join().unwrap(), restart);
        assert_eq!(candidate.join().unwrap(), restart);
        for (address, config, root) in [
            (old, old_config, old_dir.path()),
            (new, new_config, new_dir.path()),
        ] {
            assert!(std::net::TcpStream::connect(address).is_err());
            assert_eq!(root.join("state/old.enc").exists(), !restart);
            assert!(config.is_file());
        }
    }
}
