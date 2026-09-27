//! Codec side of Python-compatible migration bundles.

use super::*;

/// Derive a Fernet key from a password and salt using the fixed scrypt
/// parameters of one envelope version. Returns a padded URL-safe base64
/// string, matching Python's Fernet key format.
fn derive_fernet_encoded_key(password: &[u8], salt: &[u8], kdf: MigrationParams) -> FernetKey {
    let log_n = u8::try_from(kdf.n.trailing_zeros()).expect("fixed scrypt N is a power of two");
    let r = u32::try_from(kdf.r).expect("fixed scrypt r fits u32");
    let p = u32::try_from(kdf.p).expect("fixed scrypt p fits u32");
    let params = Params::new(log_n, r, p).expect("valid scrypt params");
    let mut derived = [0u8; 32];
    scrypt(password, salt, &params, &mut derived).expect("32-byte output is valid");
    let encoded = Zeroizing::new(URL_SAFE.encode(derived));
    derived.zeroize();
    FernetKey::from_encoded(&encoded).expect("derived key is valid Fernet")
}

/// Build the JSON envelope with the given salt and Fernet-encrypted payload.
fn build_envelope(
    version: u64,
    kdf: MigrationParams,
    salt: &[u8; 16],
    encrypted_payload: &[u8],
) -> Map<String, Value> {
    let mut scrypt_obj = Map::new();
    scrypt_obj.insert("n".to_string(), Value::Number(kdf.n.into()));
    scrypt_obj.insert("r".to_string(), Value::Number(kdf.r.into()));
    scrypt_obj.insert("p".to_string(), Value::Number(kdf.p.into()));

    let mut envelope = Map::new();
    envelope.insert(
        "schema".to_string(),
        Value::String(MIGRATION_SCHEMA.to_string()),
    );
    envelope.insert("version".to_string(), Value::Number(version.into()));
    envelope.insert("kdf".to_string(), Value::String("scrypt".to_string()));
    envelope.insert("scrypt".to_string(), Value::Object(scrypt_obj));
    envelope.insert("salt".to_string(), Value::String(URL_SAFE.encode(salt)));
    envelope.insert(
        "payload".to_string(),
        Value::String(URL_SAFE.encode(encrypted_payload)),
    );
    envelope
}

/// Encode a plaintext payload into a version-2 `.emp` migration bundle.
pub fn encode_migration(
    password: &str,
    salt: &[u8; 16],
    plaintext: &[u8],
) -> MigrationResult<Vec<u8>> {
    let password = normalize_password(password)?;
    encode_with_version(&password, salt, plaintext, MIGRATION_ENVELOPE_VERSION)
}

/// Encode a plaintext payload into a legacy version-1 bundle. Only used to
/// exercise the backwards-compatible import path in tests.
#[cfg(test)]
pub(crate) fn encode_legacy_v1_migration(
    password: &str,
    salt: &[u8; 16],
    plaintext: &[u8],
) -> MigrationResult<Vec<u8>> {
    let password = normalize_import_password(password)?;
    encode_with_version(&password, salt, plaintext, MIGRATION_VERSION)
}

fn encode_with_version(
    password: &[u8],
    salt: &[u8; 16],
    plaintext: &[u8],
    version: u64,
) -> MigrationResult<Vec<u8>> {
    if plaintext.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    let kdf =
        MigrationParams::for_envelope_version(version).ok_or(MigrationError::UnsupportedVersion)?;
    let fernet_key = derive_fernet_encoded_key(password, salt, kdf);
    let fernet = Fernet::new(&fernet_key);
    let encrypted = fernet.encrypt(plaintext);
    let envelope = build_envelope(version, kdf, salt, &encrypted);
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

/// Decode a version-1 or version-2 `.emp` migration bundle and return the
/// decrypted plaintext. KDF parameters are fixed per envelope version; the
/// bundle's `scrypt` object must match them exactly.
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
    let fixed_params =
        MigrationParams::for_envelope_version(version).ok_or(MigrationError::UnsupportedVersion)?;

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
    if (MigrationParams { n, r, p }) != fixed_params {
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

    let password = normalize_import_password(password)?;
    let fernet_key = derive_fernet_encoded_key(&password, &salt, fixed_params);
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

#[cfg(test)]
mod envelope_version_tests {
    use super::*;

    const SALT: [u8; 16] = [3_u8; 16];

    fn envelope_of(bundle: &[u8]) -> Map<String, Value> {
        parse_envelope(bundle).expect("migration envelope")
    }

    fn rewrite(bundle: &[u8], edit: impl FnOnce(&mut Map<String, Value>)) -> Vec<u8> {
        let mut envelope = envelope_of(bundle);
        edit(&mut envelope);
        let mut output = MIGRATION_MAGIC.to_vec();
        output.extend(serde_json::to_vec(&Value::Object(envelope)).expect("envelope JSON"));
        output
    }

    #[test]
    fn new_exports_write_v2_with_fixed_stronger_scrypt_and_round_trip() {
        let bundle = encode_migration("twelve-bytes", &SALT, b"payload").expect("v2 encode");
        let envelope = envelope_of(&bundle);
        assert_eq!(envelope["version"], MIGRATION_ENVELOPE_VERSION);
        assert_eq!(envelope["kdf"], "scrypt");
        assert_eq!(envelope["scrypt"]["n"], SCRYPT_V2_N);
        assert_eq!(envelope["scrypt"]["r"], SCRYPT_V2_R);
        assert_eq!(envelope["scrypt"]["p"], SCRYPT_V2_P);
        assert_eq!(
            decode_migration(" twelve-bytes ", &bundle)
                .expect("v2 decode")
                .as_slice(),
            b"payload"
        );
        assert_eq!(
            decode_migration("twelve-bytez", &bundle).expect_err("wrong password"),
            MigrationError::DecryptFailed
        );
    }

    #[test]
    fn export_requires_twelve_bytes_but_legacy_v1_import_accepts_eight() {
        assert_eq!(
            encode_migration(" elevenbytes ", &SALT, b"value").expect_err("short password"),
            MigrationError::PasswordTooShort
        );
        let legacy = encode_legacy_v1_migration("12345678", &SALT, b"legacy").expect("v1 encode");
        let envelope = envelope_of(&legacy);
        assert_eq!(envelope["version"], MIGRATION_VERSION);
        assert_eq!(envelope["scrypt"]["n"], SCRYPT_N);
        assert_eq!(
            decode_migration("12345678", &legacy)
                .expect("v1 decode")
                .as_slice(),
            b"legacy"
        );
        assert_eq!(
            decode_migration("1234567", &legacy).expect_err("below legacy minimum"),
            MigrationError::PasswordTooShort
        );
    }

    #[test]
    fn kdf_parameters_are_fixed_per_version_and_never_taken_from_the_bundle() {
        let legacy = encode_legacy_v1_migration("12345678", &SALT, b"legacy").expect("v1 encode");
        // A v1 envelope claiming v2 cost (or vice versa) is rejected.
        let upgraded = rewrite(&legacy, |envelope| {
            envelope.insert(
                "scrypt".to_owned(),
                serde_json::json!({"n": SCRYPT_V2_N, "r": 8, "p": 1}),
            );
        });
        assert_eq!(
            decode_migration("12345678", &upgraded).expect_err("mismatched v1 params"),
            MigrationError::UnsupportedKdf
        );
        let relabeled = rewrite(&legacy, |envelope| {
            envelope.insert(
                "version".to_owned(),
                Value::from(MIGRATION_ENVELOPE_VERSION),
            );
        });
        assert_eq!(
            decode_migration("12345678", &relabeled).expect_err("v2 label with v1 params"),
            MigrationError::UnsupportedKdf
        );
        let future = rewrite(&legacy, |envelope| {
            envelope.insert("version".to_owned(), Value::from(3));
        });
        assert_eq!(
            decode_migration("12345678", &future).expect_err("unknown version"),
            MigrationError::UnsupportedVersion
        );
    }
}
