//! Codec side of Python-compatible migration bundles.

use super::*;

/// Derive a Fernet key from a password and salt using fixed scrypt parameters.
/// Returns a padded URL-safe base64 string, matching Python's Fernet key format.
fn derive_fernet_encoded_key(password: &[u8], salt: &[u8]) -> FernetKey {
    let params = Params::new(14, 8, 1).expect("valid scrypt params");
    let mut derived = [0u8; 32];
    scrypt(password, salt, &params, &mut derived).expect("32-byte output is valid");
    let encoded = Zeroizing::new(URL_SAFE.encode(derived));
    derived.zeroize();
    FernetKey::from_encoded(&encoded).expect("derived key is valid Fernet")
}

/// Build the JSON envelope with the given salt and Fernet-encrypted payload.
fn build_envelope(salt: &[u8; 16], encrypted_payload: &[u8]) -> Map<String, Value> {
    let mut scrypt_obj = Map::new();
    scrypt_obj.insert("n".to_string(), Value::Number(SCRYPT_N.into()));
    scrypt_obj.insert("r".to_string(), Value::Number(SCRYPT_R.into()));
    scrypt_obj.insert("p".to_string(), Value::Number(SCRYPT_P.into()));

    let mut envelope = Map::new();
    envelope.insert(
        "schema".to_string(),
        Value::String(MIGRATION_SCHEMA.to_string()),
    );
    envelope.insert(
        "version".to_string(),
        Value::Number(MIGRATION_VERSION.into()),
    );
    envelope.insert("kdf".to_string(), Value::String("scrypt".to_string()));
    envelope.insert("scrypt".to_string(), Value::Object(scrypt_obj));
    envelope.insert("salt".to_string(), Value::String(URL_SAFE.encode(salt)));
    envelope.insert(
        "payload".to_string(),
        Value::String(URL_SAFE.encode(encrypted_payload)),
    );
    envelope
}

/// Encode a plaintext payload into a `.emp` migration bundle.
pub fn encode_migration(
    password: &str,
    salt: &[u8; 16],
    plaintext: &[u8],
) -> MigrationResult<Vec<u8>> {
    if plaintext.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    let password = normalize_password(password)?;
    let fernet_key = derive_fernet_encoded_key(&password, salt);
    let fernet = Fernet::new(&fernet_key);
    let encrypted = fernet.encrypt(plaintext);
    let envelope = build_envelope(salt, &encrypted);
    let envelope_json = serde_json::to_vec(&Value::Object(envelope)).expect("serializable JSON");

    let mut output = Vec::with_capacity(MIGRATION_MAGIC.len() + envelope_json.len() + 1);
    output.extend_from_slice(MIGRATION_MAGIC);
    output.extend_from_slice(&envelope_json);
    output.push(b'\n');

    if output.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    Ok(output)
}

/// Decode a `.emp` migration bundle and return the decrypted plaintext.
pub fn decode_migration(password: &str, encoded: &[u8]) -> MigrationResult<Zeroizing<Vec<u8>>> {
    if encoded.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    if !encoded.starts_with(MIGRATION_MAGIC) {
        return Err(MigrationError::NotMigrationFormat);
    }

    let body = &encoded[MIGRATION_MAGIC.len()..];
    let body = if body.last() == Some(&b'\n') {
        &body[..body.len() - 1]
    } else {
        body
    };

    let envelope: Value =
        serde_json::from_slice(body).map_err(|_| MigrationError::InvalidEnvelope)?;
    let envelope = envelope
        .as_object()
        .ok_or(MigrationError::InvalidEnvelope)?;

    let schema = envelope.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != MIGRATION_SCHEMA {
        return Err(MigrationError::UnsupportedVersion);
    }
    let version = envelope.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version != MIGRATION_VERSION {
        return Err(MigrationError::UnsupportedVersion);
    }

    let kdf = envelope.get("kdf").and_then(Value::as_str).unwrap_or("");
    if kdf != "scrypt" {
        return Err(MigrationError::UnsupportedKdf);
    }
    let params = envelope
        .get("scrypt")
        .and_then(Value::as_object)
        .ok_or(MigrationError::UnsupportedKdf)?;
    let n = params.get("n").and_then(Value::as_u64).unwrap_or(0);
    let r = params.get("r").and_then(Value::as_u64).unwrap_or(0);
    let p = params.get("p").and_then(Value::as_u64).unwrap_or(0);
    if n != SCRYPT_N || r != SCRYPT_R || p != SCRYPT_P {
        return Err(MigrationError::UnsupportedKdf);
    }

    let salt_str = envelope
        .get("salt")
        .and_then(Value::as_str)
        .ok_or(MigrationError::InvalidSalt)?;
    let salt = URL_SAFE
        .decode(salt_str.as_bytes())
        .map_err(|_| MigrationError::InvalidSalt)?;
    if salt.len() != SALT_BYTES {
        return Err(MigrationError::InvalidSalt);
    }

    let payload_str = envelope
        .get("payload")
        .and_then(Value::as_str)
        .ok_or(MigrationError::InvalidPayload)?;
    let encrypted = URL_SAFE
        .decode(payload_str.as_bytes())
        .map_err(|_| MigrationError::InvalidPayload)?;

    let password = normalize_password(password)?;
    let fernet_key = derive_fernet_encoded_key(&password, &salt);
    let fernet = Fernet::new(&fernet_key);
    let plaintext = fernet
        .decrypt(&encrypted)
        .map_err(|_| MigrationError::DecryptFailed)?;
    Ok(plaintext)
}

/// Parse a migration bundle's JSON envelope without decryption.
pub fn parse_envelope(encoded: &[u8]) -> MigrationResult<Map<String, Value>> {
    if encoded.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    if !encoded.starts_with(MIGRATION_MAGIC) {
        return Err(MigrationError::NotMigrationFormat);
    }
    let body = &encoded[MIGRATION_MAGIC.len()..];
    let body = if body.last() == Some(&b'\n') {
        &body[..body.len() - 1]
    } else {
        body
    };
    let envelope: Value =
        serde_json::from_slice(body).map_err(|_| MigrationError::InvalidEnvelope)?;
    envelope
        .as_object()
        .cloned()
        .ok_or(MigrationError::InvalidEnvelope)
}
