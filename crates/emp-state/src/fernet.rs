//! Fernet key validation and operations backed by the RustCrypto feature of
//! `fernet`. The wire format is interoperable with Python's
//! `cryptography.fernet.Fernet` and does not require a platform OpenSSL install.

use base64::{Engine, engine::general_purpose::URL_SAFE};
use fernet::Fernet as FernetToken;
use std::fmt;
use zeroize::Zeroizing;

/// A validated, URL-safe Base64 Fernet key whose encoded form is wiped on drop.
#[derive(Clone)]
pub struct FernetKey {
    encoded: Zeroizing<String>,
}

impl FernetKey {
    /// Validate that `encoded` decodes to the 32 bytes required by Fernet.
    pub fn from_encoded(encoded: &str) -> Result<Self, FernetKeyError> {
        let decoded = Zeroizing::new(
            URL_SAFE
                .decode(encoded)
                .map_err(|_| FernetKeyError::Invalid)?,
        );
        if decoded.len() != 32 || URL_SAFE.encode(decoded.as_slice()) != encoded {
            return Err(FernetKeyError::Invalid);
        }
        FernetToken::new(encoded).ok_or(FernetKeyError::Invalid)?;
        Ok(Self {
            encoded: Zeroizing::new(encoded.to_owned()),
        })
    }

    /// Expose the encoded key only at the crypto boundary.
    pub(crate) fn encoded(&self) -> &str {
        self.encoded.as_str()
    }
}

impl fmt::Debug for FernetKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FernetKey([REDACTED])")
    }
}

/// A malformed Fernet key. The error intentionally never includes key data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FernetKeyError {
    Invalid,
}

impl fmt::Display for FernetKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Fernet key must encode exactly 32 bytes")
    }
}

impl std::error::Error for FernetKeyError {}

/// Fernet encryptor/decryptor wrapper.
pub struct Fernet {
    token: FernetToken,
}

impl Fernet {
    pub fn new(key: &FernetKey) -> Self {
        Self {
            token: FernetToken::new(key.encoded()).expect("FernetKey is validated"),
        }
    }

    /// Encrypt plaintext into a padded URL-safe Base64 Fernet token.
    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        self.token.encrypt(plaintext).into_bytes()
    }

    /// Decrypt without a TTL, matching Python's `Fernet.decrypt(token)` call.
    pub fn decrypt(&self, token: &[u8]) -> Result<Zeroizing<Vec<u8>>, FernetError> {
        let token = std::str::from_utf8(token).map_err(|_| FernetError::DecryptFailed)?;
        self.token
            .decrypt(token)
            .map(Zeroizing::new)
            .map_err(|_| FernetError::DecryptFailed)
    }
}

/// Fernet deliberately exposes one failure class for malformed, unauthenticated,
/// wrong-key and invalid-padding tokens, matching Python's `InvalidToken`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FernetError {
    DecryptFailed,
}

impl fmt::Display for FernetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("token cannot be decrypted")
    }
}

impl std::error::Error for FernetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_key_and_debug_output_never_disclose_secret_material() {
        assert_eq!(
            FernetKey::from_encoded("not-a-key").expect_err("invalid key"),
            FernetKeyError::Invalid
        );
        let key = FernetKey::from_encoded("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=")
            .expect("valid key");
        assert_eq!(format!("{key:?}"), "FernetKey([REDACTED])");
        assert_eq!(
            FernetKey::from_encoded("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8")
                .expect_err("Python rejects an unpadded 32-byte key"),
            FernetKeyError::Invalid
        );
    }
}
