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
