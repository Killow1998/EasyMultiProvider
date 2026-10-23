//! Ownership of the local inference credential. Callers serialize mutations
//! with the integration operation lock; an existing login always wins.
use crate::IntegrationError;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const LIMIT: u64 = 1024 * 1024;

pub fn read(path: &Path) -> Result<Option<Value>, IntegrationError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(IntegrationError("Codex authentication cannot be read")),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > LIMIT {
        return Err(IntegrationError(
            "Codex authentication is not a regular file",
        ));
    }
    let bytes =
        fs::read(path).map_err(|_| IntegrationError("Codex authentication cannot be read"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| IntegrationError("Codex authentication is invalid"))
}

pub fn is_local(auth: &Value) -> bool {
    auth["emp_local_connection"] == true
        && auth["OPENAI_API_KEY"]
            .as_str()
            .is_some_and(|key| key.starts_with("emp-local-"))
}

pub fn mode(path: &Path) -> &'static str {
    match read(path) {
        Ok(None) => "missing",
        Ok(Some(auth)) if is_local(&auth) => "local_key",
        Ok(Some(auth)) if emp_state::validate_auth_json(&auth).is_ok() => "subscription",
        Ok(Some(auth))
            if auth["OPENAI_API_KEY"]
                .as_str()
                .is_some_and(|key| !key.is_empty()) =>
        {
            "api_key"
        }
        _ => "unavailable",
    }
}

fn journal(path: &Path) -> PathBuf {
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("easy-multi-provider/local-auth.json")
}

/// Create only when signed out. The journal is also the fully written private
/// staging file; hard-linking it publishes without overwriting a concurrent login.
pub fn ensure(path: &Path) -> Result<bool, IntegrationError> {
    if read(path)?.is_some() {
        return Ok(false);
    }
    let mut random = [0_u8; 32];
    getrandom::getrandom(&mut random)
        .map_err(|_| IntegrationError("Local key generation failed"))?;
    let key = format!(
        "emp-local-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let auth = json!({"OPENAI_API_KEY":key, "emp_local_connection":true});
    let saved = journal(path);
    emp_state::atomic_write_private_state(&saved, &serde_json::to_vec(&auth).unwrap())
        .map_err(|_| IntegrationError("Local key cannot be saved"))?;
    match fs::hard_link(&saved, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => Err(IntegrationError("Local key cannot be installed")),
    }
}

/// Restore the original signed-out state. A login written by Codex or another
/// application is preserved, including after a crash during subscription login.
pub fn restore(path: &Path) -> Result<(), IntegrationError> {
    let saved = journal(path);
    let Some(owned) = read(&saved)? else {
        return Ok(());
    };
    if is_local(&owned) && read(path)?.as_ref() == Some(&owned) {
        fs::remove_file(path).map_err(|_| IntegrationError("Local key cannot be removed"))?;
    }
    fs::remove_file(saved).map_err(|_| IntegrationError("Local key ownership cannot be released"))
}

/// The pending login captured its original authentication before starting.
/// Refuse to overwrite any intervening edit or login, and retain no old secrets.
pub fn install_subscription(
    path: &Path,
    expected: &Option<Value>,
    auth: &Value,
) -> Result<bool, IntegrationError> {
    let validated = emp_state::validate_auth_json(auth)
        .map_err(|_| IntegrationError("Subscription login returned invalid credentials"))?;
    if &read(path)? != expected {
        return Ok(false);
    }
    emp_state::filesystem::atomic_write_config(path, &serde_json::to_vec(&validated).unwrap())
        .map_err(|_| IntegrationError("Subscription login cannot be saved"))?;
    restore(path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_key_is_stable_and_restore_returns_to_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        assert!(ensure(&path).unwrap());
        let first = read(&path).unwrap();
        assert!(!ensure(&path).unwrap());
        assert_eq!(read(&path).unwrap(), first);
        assert_eq!(mode(&path), "local_key");
        restore(&path).unwrap();
        assert_eq!(mode(&path), "missing");
    }

    #[test]
    fn existing_or_replaced_authentication_is_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        ensure(&path).unwrap();
        let original = br#"{"OPENAI_API_KEY":"user-key"}"#;
        emp_state::filesystem::atomic_write_config(&path, original).unwrap();
        assert!(!ensure(&path).unwrap());
        restore(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn malformed_auth_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(&path, b"partial").unwrap();
        assert!(ensure(&path).is_err());
        assert_eq!(fs::read(path).unwrap(), b"partial");
    }
}
