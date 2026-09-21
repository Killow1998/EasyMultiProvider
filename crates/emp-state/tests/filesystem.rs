use emp_state::{
    FileTransaction, FilesystemError, MASTER_KEY_ENV, MASTER_KEY_FILE_ENV,
    MAX_TRANSACTION_FILE_BYTES, VAULT_MAGIC, VaultStore, with_file_transaction,
};
use serde_json::json;
use std::fs;
use std::sync::Mutex;
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

const SYNTHETIC_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const OTHER_KEY: &str = "Hh0cGxoZGBcWFRQTEhEQDw4NDAsKCQgHBgUEAwIBAAA=";
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn generated_key_is_private_valid_and_reused() {
    let directory = tempdir().expect("tempdir");
    let key_path = canonical_temp_path(&directory).join("private/master.key");
    let first = VaultStore::from_sources(None, &key_path).expect("create key");
    let original = fs::read(&key_path).expect("read generated key");
    assert_eq!(
        first.ensure_master_key(),
        Some(key_path.as_path()),
        "absolute temp paths remain unchanged"
    );
    assert_eq!(original.last(), Some(&b'\n'));
    assert_eq!(original.len(), 45);

    let second = VaultStore::from_sources(None, &key_path).expect("reuse key");
    assert_eq!(second.ensure_master_key(), Some(key_path.as_path()));
    assert_eq!(fs::read(&key_path).expect("read reused key"), original);

    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(&key_path)
                .expect("key metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(key_path.parent().expect("parent"))
                .expect("parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn invalid_existing_key_is_never_replaced() {
    let directory = tempdir().expect("tempdir");
    let key_path = canonical_temp_path(&directory).join("master.key");
    drop(VaultStore::from_sources(None, &key_path).expect("create private key file"));
    fs::write(&key_path, b"invalid-key\n").expect("write invalid key");

    assert_eq!(
        VaultStore::from_sources(None, &key_path)
            .err()
            .expect("invalid key error"),
        FilesystemError::KeyFileInvalid
    );
    assert_eq!(
        fs::read(&key_path).expect("invalid key remains"),
        b"invalid-key\n"
    );
}

#[test]
fn environment_key_takes_precedence_without_creating_a_file() {
    let directory = tempdir().expect("tempdir");
    let key_path = directory.path().join("must-not-exist/master.key");
    let store = VaultStore::from_sources(Some(SYNTHETIC_KEY), &key_path).expect("environment key");
    assert_eq!(store.ensure_master_key(), None);
    assert!(!key_path.exists());
    assert_eq!(
        VaultStore::from_sources(Some("invalid"), &key_path)
            .err()
            .expect("invalid environment key"),
        FilesystemError::EnvironmentKeyInvalid
    );
}

#[test]
fn process_environment_selects_key_and_path_once() {
    let _lock = ENV_LOCK.lock().expect("environment lock");
    let directory = tempdir().expect("tempdir");
    let root = canonical_temp_path(&directory);
    let default_path = root.join("default/master.key");
    let configured_path = root.join("configured/master.key");
    let original_key = std::env::var_os(MASTER_KEY_ENV);
    let original_path = std::env::var_os(MASTER_KEY_FILE_ENV);

    // SAFETY: this test serializes all environment mutations in this crate and
    // restores both variables before releasing the lock.
    unsafe {
        std::env::set_var(MASTER_KEY_ENV, SYNTHETIC_KEY);
        std::env::set_var(MASTER_KEY_FILE_ENV, &configured_path);
    }
    let environment_store =
        VaultStore::from_environment(&default_path).expect("environment-backed store");
    assert_eq!(environment_store.ensure_master_key(), None);
    assert!(!configured_path.exists());

    // SAFETY: guarded and restored below.
    unsafe {
        std::env::remove_var(MASTER_KEY_ENV);
    }
    let file_store = VaultStore::from_environment(&default_path).expect("file-backed store");
    assert_eq!(
        file_store.ensure_master_key(),
        Some(configured_path.as_path())
    );
    assert!(configured_path.exists());

    // SAFETY: guarded restoration of the original process environment.
    unsafe {
        match original_key {
            Some(value) => std::env::set_var(MASTER_KEY_ENV, value),
            None => std::env::remove_var(MASTER_KEY_ENV),
        }
        match original_path {
            Some(value) => std::env::set_var(MASTER_KEY_FILE_ENV, value),
            None => std::env::remove_var(MASTER_KEY_FILE_ENV),
        }
    }
}

#[test]
fn encrypted_bytes_text_and_json_never_write_plaintext() {
    let directory = tempdir().expect("tempdir");
    let store = VaultStore::from_sources(Some(SYNTHETIC_KEY), &directory.path().join("unused.key"))
        .expect("store");
    let json_path = directory.path().join("nested/auth.json.enc");
    let value = json!({
        "tokens": {"access_token": "do-not-store-plain"},
        "label": "測試"
    });
    store
        .write_encrypted_json(&json_path, &value)
        .expect("write JSON");
    let raw = fs::read(&json_path).expect("read ciphertext");
    assert!(raw.starts_with(VAULT_MAGIC));
    assert!(!String::from_utf8_lossy(&raw).contains("do-not-store-plain"));
    assert_eq!(
        store.read_encrypted_json(&json_path).expect("read JSON"),
        value
    );
    set_mode(&json_path, 0o666);
    let replacement = json!({"replacement": true});
    store
        .write_encrypted_json(&json_path, &replacement)
        .expect("overwrite JSON");
    assert_eq!(
        store
            .read_encrypted_json(&json_path)
            .expect("read replacement"),
        replacement
    );

    let text_path = directory.path().join("nested/text.enc");
    store
        .write_encrypted_text(&text_path, "secret text 測試")
        .expect("write text");
    assert_eq!(
        store
            .read_encrypted_text(&text_path)
            .expect("read text")
            .as_str(),
        "secret text 測試"
    );

    let bytes_path = directory.path().join("nested/bytes.enc");
    store
        .write_encrypted_bytes(&bytes_path, b"\x00\xffbinary")
        .expect("write bytes");
    assert_eq!(
        store
            .read_encrypted_bytes(&bytes_path)
            .expect("read bytes")
            .as_slice(),
        b"\x00\xffbinary"
    );

    let wrong_store =
        VaultStore::from_sources(Some(OTHER_KEY), &directory.path().join("unused-2.key"))
            .expect("wrong store");
    assert_eq!(
        wrong_store
            .read_encrypted_bytes(&bytes_path)
            .expect_err("wrong key"),
        FilesystemError::CredentialDecryptFailed
    );
    let plain_path = directory.path().join("plain");
    fs::write(&plain_path, b"plaintext").expect("write plaintext");
    assert_eq!(
        store
            .read_encrypted_bytes(&plain_path)
            .expect_err("unsupported plaintext"),
        FilesystemError::UnsupportedCredentialFormat
    );

    let invalid_json_path = directory.path().join("nested/invalid-json.enc");
    store
        .write_encrypted_bytes(&invalid_json_path, b"{not-json")
        .expect("write invalid JSON payload");
    assert_eq!(
        store
            .read_encrypted_json(&invalid_json_path)
            .expect_err("invalid JSON"),
        FilesystemError::InvalidJson
    );
    let invalid_text_path = directory.path().join("nested/invalid-text.enc");
    store
        .write_encrypted_bytes(&invalid_text_path, b"\xff")
        .expect("write invalid text payload");
    assert_eq!(
        store
            .read_encrypted_text(&invalid_text_path)
            .expect_err("invalid text"),
        FilesystemError::InvalidText
    );

    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(&json_path)
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(json_path.parent().expect("parent"))
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn key_path_rejects_a_non_directory_parent() {
    let directory = tempdir().expect("tempdir");
    let parent = canonical_temp_path(&directory).join("not-a-directory");
    fs::write(&parent, b"file").expect("parent blocker");
    assert_eq!(
        VaultStore::from_sources(None, &parent.join("master.key"))
            .err()
            .expect("non-directory parent"),
        FilesystemError::KeyDirectoryNotRegular
    );
}

#[cfg(unix)]
#[test]
fn key_paths_reject_symlink_components_and_public_permissions() {
    let directory = tempdir().expect("tempdir");
    let root = canonical_temp_path(&directory);
    let real = root.join("real");
    fs::create_dir(&real).expect("real directory");
    let linked = root.join("linked");
    symlink(&real, &linked).expect("directory symlink");
    assert_eq!(
        VaultStore::from_sources(None, &linked.join("master.key"))
            .err()
            .expect("symlink parent"),
        FilesystemError::KeyDirectoryNotRegular
    );

    let target = root.join("target.key");
    fs::write(&target, format!("{SYNTHETIC_KEY}\n")).expect("target key");
    set_mode(&target, 0o600);
    let key_link = root.join("key-link");
    symlink(&target, &key_link).expect("key symlink");
    assert_eq!(
        VaultStore::from_sources(None, &key_link)
            .err()
            .expect("symlink key"),
        FilesystemError::KeyFileNotRegular
    );

    set_mode(&target, 0o644);
    assert_eq!(
        VaultStore::from_sources(None, &target)
            .err()
            .expect("public key file"),
        FilesystemError::KeyFileNotPrivate
    );
}

#[test]
fn transaction_rolls_back_existing_and_new_files_and_preserves_first_snapshot() {
    let directory = tempdir().expect("tempdir");
    let existing = directory.path().join("existing.json");
    let created = directory.path().join("created.json");
    fs::write(&existing, b"original").expect("original file");
    set_mode(&existing, 0o640);

    let result: Result<(), FilesystemError> = with_file_transaction(|transaction| {
        transaction.remember(&existing)?;
        transaction.remember(&existing)?;
        transaction.remember(&created)?;
        fs::write(&existing, b"changed").map_err(|_| FilesystemError::ManagedFileUnavailable)?;
        fs::write(&created, b"new").map_err(|_| FilesystemError::ManagedFileUnavailable)?;
        Err(FilesystemError::InvalidJson)
    });
    assert_eq!(
        result.expect_err("operation failure"),
        FilesystemError::InvalidJson
    );
    assert_eq!(fs::read(&existing).expect("restored file"), b"original");
    assert!(!created.exists());
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&existing)
            .expect("restored metadata")
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
}

#[test]
fn transaction_commit_persists_and_drop_rolls_back() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("value");
    fs::write(&path, b"before").expect("before");

    {
        let mut transaction = FileTransaction::new();
        transaction.remember(&path).expect("remember");
        fs::write(&path, b"committed").expect("committed value");
        transaction.commit();
    }
    assert_eq!(fs::read(&path).expect("after commit"), b"committed");

    {
        let mut transaction = FileTransaction::new();
        transaction.remember(&path).expect("remember");
        fs::write(&path, b"temporary").expect("temporary value");
    }
    assert_eq!(fs::read(&path).expect("after drop"), b"committed");
}

#[test]
fn transaction_rejects_nonregular_and_oversized_files() {
    let directory = tempdir().expect("tempdir");
    let mut transaction = FileTransaction::new();
    assert_eq!(
        transaction
            .remember(directory.path())
            .expect_err("directory is not a file"),
        FilesystemError::ManagedFileNotRegular
    );

    let oversized = directory.path().join("oversized");
    let file = fs::File::create(&oversized).expect("oversized file");
    file.set_len(MAX_TRANSACTION_FILE_BYTES as u64 + 1)
        .expect("sparse size");
    assert_eq!(
        transaction
            .remember(&oversized)
            .expect_err("oversized snapshot"),
        FilesystemError::ManagedFileTooLarge
    );
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
}

#[cfg(not(unix))]
fn set_mode(_: &std::path::Path, _: u32) {}

fn canonical_temp_path(directory: &tempfile::TempDir) -> std::path::PathBuf {
    fs::canonicalize(directory.path()).expect("canonical temporary directory")
}
