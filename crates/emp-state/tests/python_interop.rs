use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE},
};
use emp_state::{
    FernetKey, MAX_BUNDLE_BYTES, MIGRATION_ENVELOPE_VERSION, MIGRATION_MAGIC, MIGRATION_VERSION,
    MigrationError, SCRYPT_N, SCRYPT_P, SCRYPT_R, SCRYPT_V2_N, SCRYPT_V2_P, SCRYPT_V2_R,
    VAULT_MAGIC, VaultError, decode_migration, decode_vault, encode_migration, encode_vault,
    parse_envelope,
};
use serde::Deserialize;
use serde_json::Value;
use std::process::Command;

/// New exports require a 12-byte password; the frozen Python fixture uses the
/// legacy 8-byte minimum and still has to decrypt.
const EXPORT_PASSWORD: &str = " synthetic-export-password ";

#[derive(Deserialize)]
struct PythonFixture {
    key: String,
    vault_plaintext_base64: String,
    vault_file_base64: String,
    migration_password: String,
    migration_salt_base64: String,
    migration_plaintext_base64: String,
    migration_bundle_base64: String,
}

fn fixture() -> PythonFixture {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-crypto-interop.json"
    ))
    .expect("valid Python fixture")
}

fn b64(value: &str) -> Vec<u8> {
    STANDARD.decode(value).expect("valid fixture base64")
}

#[test]
fn python_vault_and_migration_files_decrypt_in_rust() {
    let fixture = fixture();
    let key = FernetKey::from_encoded(&fixture.key).expect("fixture key");

    let vault_plaintext = b64(&fixture.vault_plaintext_base64);
    let vault_file = b64(&fixture.vault_file_base64);
    assert_eq!(&vault_file[..VAULT_MAGIC.len()], VAULT_MAGIC);
    assert_eq!(
        decode_vault(&key, &vault_file)
            .expect("Python vault decrypts")
            .as_slice(),
        vault_plaintext
    );

    let migration_plaintext = b64(&fixture.migration_plaintext_base64);
    let migration_bundle = b64(&fixture.migration_bundle_base64);
    assert_eq!(&migration_bundle[..MIGRATION_MAGIC.len()], MIGRATION_MAGIC);
    let legacy_envelope = parse_envelope(&migration_bundle).expect("legacy envelope");
    assert_eq!(legacy_envelope["version"], MIGRATION_VERSION);
    assert_eq!(fixture.migration_password.trim().len(), 8);
    assert_eq!(
        decode_migration(&fixture.migration_password, &migration_bundle)
            .expect("Python migration bundle decrypts")
            .as_slice(),
        migration_plaintext
    );
}

#[test]
fn rust_outputs_round_trip_and_keep_the_python_envelope_contract() {
    let fixture = fixture();
    let key = FernetKey::from_encoded(&fixture.key).expect("fixture key");
    let plaintext = "synthetic Rust vault value \0 測試".as_bytes();
    let vault = encode_vault(&key, plaintext);
    assert_eq!(
        decode_vault(&key, &vault)
            .expect("vault round trip")
            .as_slice(),
        plaintext
    );

    let salt: [u8; 16] = b64(&fixture.migration_salt_base64)
        .try_into()
        .expect("16-byte salt");
    let migration = encode_migration(EXPORT_PASSWORD, &salt, plaintext).expect("migration encode");
    assert_eq!(
        decode_migration(EXPORT_PASSWORD, &migration)
            .expect("migration round trip")
            .as_slice(),
        plaintext
    );

    let envelope = parse_envelope(&migration).expect("migration envelope");
    assert_eq!(envelope["version"], MIGRATION_ENVELOPE_VERSION);
    assert_eq!(envelope["kdf"], Value::String("scrypt".to_owned()));
    assert_eq!(envelope["scrypt"]["n"], SCRYPT_V2_N);
    assert_eq!(envelope["scrypt"]["r"], SCRYPT_V2_R);
    assert_eq!(envelope["scrypt"]["p"], SCRYPT_V2_P);
    for field in ["salt", "payload"] {
        let encoded = envelope[field].as_str().expect("base64 string");
        let decoded = URL_SAFE.decode(encoded).expect("strict URL-safe base64");
        assert_eq!(URL_SAFE.encode(decoded), encoded);
    }
}

#[test]
fn authentication_and_envelope_failures_are_bounded_and_secret_free() {
    let fixture = fixture();
    let key = FernetKey::from_encoded(&fixture.key).expect("fixture key");
    let wrong_key =
        FernetKey::from_encoded("Hh0cGxoZGBcWFRQTEhEQDw4NDAsKCQgHBgUEAwIBAAA=").expect("wrong key");
    let vault = b64(&fixture.vault_file_base64);
    assert_eq!(
        decode_vault(&key, b"plain text is not a vault").expect_err("plaintext vault"),
        VaultError::NotVaultFormat
    );
    assert_eq!(
        decode_vault(&wrong_key, &vault).expect_err("wrong key"),
        VaultError::DecryptFailed
    );

    let mut tampered_vault = vault.clone();
    *tampered_vault.last_mut().expect("non-empty vault") ^= 1;
    assert_eq!(
        decode_vault(&key, &tampered_vault).expect_err("tampered vault"),
        VaultError::DecryptFailed
    );

    let bundle = b64(&fixture.migration_bundle_base64);
    assert_eq!(
        decode_migration("wrong-password", &bundle).expect_err("wrong password"),
        MigrationError::DecryptFailed
    );

    let body = &bundle[MIGRATION_MAGIC.len()..];
    let mut envelope: Value = serde_json::from_slice(body).expect("fixture envelope");
    assert_eq!(envelope["scrypt"]["n"], SCRYPT_N);
    assert_eq!(envelope["scrypt"]["r"], SCRYPT_R);
    assert_eq!(envelope["scrypt"]["p"], SCRYPT_P);
    envelope["scrypt"]["n"] = Value::from(32768);
    let mut changed = MIGRATION_MAGIC.to_vec();
    changed.extend(serde_json::to_vec(&envelope).expect("changed envelope"));
    assert_eq!(
        decode_migration(&fixture.migration_password, &changed).expect_err("wrong KDF params"),
        MigrationError::UnsupportedKdf
    );

    envelope["scrypt"]["n"] = Value::from(SCRYPT_N);
    envelope["payload"] = Value::String("not+url/safe***=".to_owned());
    let mut invalid_base64 = MIGRATION_MAGIC.to_vec();
    invalid_base64.extend(serde_json::to_vec(&envelope).expect("changed envelope"));
    assert_eq!(
        decode_migration(&fixture.migration_password, &invalid_base64)
            .expect_err("invalid payload base64"),
        MigrationError::InvalidPayload
    );

    let mut too_large = vec![0_u8; MAX_BUNDLE_BYTES + 1];
    too_large[..MIGRATION_MAGIC.len()].copy_from_slice(MIGRATION_MAGIC);
    assert_eq!(
        decode_migration(&fixture.migration_password, &too_large).expect_err("oversized bundle"),
        MigrationError::TooLarge
    );
    let salt = [0_u8; 16];
    assert_eq!(
        encode_migration(EXPORT_PASSWORD, &salt, &too_large).expect_err("oversized plaintext"),
        MigrationError::TooLarge
    );
    assert_eq!(
        encode_migration(" short ", &salt, b"value").expect_err("short password"),
        MigrationError::PasswordTooShort
    );
    assert_eq!(
        encode_migration(&fixture.migration_password, &salt, b"value")
            .expect_err("legacy-length password is too short for new exports"),
        MigrationError::PasswordTooShort
    );
    assert_eq!(
        encode_migration(&"x".repeat(4097), &salt, b"value").expect_err("long password"),
        MigrationError::PasswordTooLong
    );

    for error in [
        decode_vault(&wrong_key, &vault)
            .expect_err("wrong key")
            .to_string(),
        decode_migration("wrong-password", &bundle)
            .expect_err("wrong password")
            .to_string(),
    ] {
        assert!(!error.contains("synthetic-only"));
        assert!(!error.contains(&fixture.key));
        assert!(!error.contains(&fixture.migration_password));
    }
}

#[test]
fn rust_ciphertext_can_be_checked_by_the_python_oracle_when_configured() {
    let Some(python) = std::env::var_os("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let key = FernetKey::from_encoded(&fixture.key).expect("fixture key");
    let plaintext = "Rust to Python synthetic payload \0 測試".as_bytes();
    let vault = encode_vault(&key, plaintext);
    let salt: [u8; 16] = b64(&fixture.migration_salt_base64)
        .try_into()
        .expect("16-byte salt");
    let migration = encode_migration(EXPORT_PASSWORD, &salt, plaintext).expect("migration encode");

    let script = r#"
import base64, json, sys
from cryptography.fernet import Fernet
from cryptography.hazmat.primitives.kdf.scrypt import Scrypt
key, vault_b64, bundle_b64, password, expected_b64 = sys.argv[1:]
expected = base64.b64decode(expected_b64)
vault = base64.b64decode(vault_b64)
assert vault.startswith(b'easy-multi-provider-v1\n')
assert Fernet(key.encode()).decrypt(vault.split(b'\n', 1)[1]) == expected
bundle = base64.b64decode(bundle_b64)
magic = b'EMP-MIGRATION\x01\n'
assert bundle.startswith(magic)
envelope = json.loads(bundle[len(magic):].decode())
salt = base64.b64decode(envelope['salt'], altchars=b'-_', validate=True)
assert envelope['version'] == 2 and envelope['scrypt'] == {'n': 2**17, 'r': 8, 'p': 1}
derived = Scrypt(salt=salt, length=32, n=2**17, r=8, p=1).derive(password.strip().encode())
token = base64.b64decode(envelope['payload'], altchars=b'-_', validate=True)
assert Fernet(base64.urlsafe_b64encode(derived)).decrypt(token) == expected
"#;
    let status = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(&fixture.key)
        .arg(STANDARD.encode(vault))
        .arg(STANDARD.encode(migration))
        .arg(EXPORT_PASSWORD)
        .arg(STANDARD.encode(plaintext))
        .status()
        .expect("start Python compatibility oracle");
    assert!(status.success(), "Python rejected Rust ciphertext");
}

/// End-to-end importer interop for v2 bundles: the real Rust exporter's output
/// must pass the in-tree Python `read_bundle`, and the in-tree Python exporter's
/// output must pass the Rust decoder, including provider credentials.
#[test]
fn real_exporters_and_importers_interoperate_on_v2_bundles_when_configured() {
    let Some(python) = std::env::var_os("EMP_PYTHON_INTEROP") else {
        return;
    };
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let directory = tempfile::tempdir().expect("temporary directory");
    let vault = emp_state::VaultStore::from_sources(
        Some("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="),
        &directory.path().join("unused.key"),
    )
    .expect("vault store");
    let config = emp_state::normalize_configuration(Some(&serde_json::json!({
        "providers": [{"id": "deepseek", "base_url": "https://api.deepseek.com/v1",
            "api_key": "synthetic-provider-key"}],
        "models": [{"id": "deepseek/chat", "provider": "deepseek"}],
    })))
    .expect("configuration");
    let groups = emp_state::ExportGroups::from_list(&["external"]).expect("export groups");
    let rust_bundle =
        emp_state::export_migration_bundle(&config, EXPORT_PASSWORD, &vault, Some(&groups), None)
            .expect("Rust export");
    let rust_path = directory.path().join("rust.emp");
    let python_path = directory.path().join("python.emp");
    std::fs::write(&rust_path, &rust_bundle).expect("write Rust bundle");

    let script = r#"
import sys
from pathlib import Path
from easy_multi_provider.config import normalize
from easy_multi_provider.migration import export_bundle, read_bundle
rust_path, python_path, password = sys.argv[1:]
payload = read_bundle(Path(rust_path).read_bytes(), password)
assert payload["provider_keys"] == {"deepseek": "synthetic-provider-key"}, payload["provider_keys"]
config = normalize({
    "providers": [{"id": "deepseek", "base_url": "https://api.deepseek.com/v1",
        "api_key": "synthetic-python-key"}],
    "models": [{"id": "deepseek/chat", "provider": "deepseek"}],
})
Path(python_path).write_bytes(export_bundle(config, Path("config.json"), password, ["external"]))
"#;
    let status = Command::new(python)
        .current_dir(&repository)
        .env("PYTHONPATH", &repository)
        .arg("-c")
        .arg(script)
        .arg(&rust_path)
        .arg(&python_path)
        .arg(EXPORT_PASSWORD)
        .status()
        .expect("start Python importer");
    assert!(
        status.success(),
        "Python importer rejected the Rust v2 bundle"
    );

    let python_bundle = std::fs::read(&python_path).expect("Python bundle");
    let envelope = parse_envelope(&python_bundle).expect("Python envelope");
    assert_eq!(envelope["version"], MIGRATION_ENVELOPE_VERSION);
    let plaintext = decode_migration(EXPORT_PASSWORD, &python_bundle).expect("Rust decodes v2");
    let payload: Value = serde_json::from_slice(&plaintext).expect("payload JSON");
    assert_eq!(payload["provider_keys"]["deepseek"], "synthetic-python-key");
}
