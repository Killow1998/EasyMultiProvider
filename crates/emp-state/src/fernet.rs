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
    /// Generate a key with the operating system CSPRNG used by `fernet`.
    pub fn generate() -> Self {
        let encoded = Zeroizing::new(FernetToken::generate_key());
        Self::from_encoded(&encoded).expect("fernet generated a valid key")
    }

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
        let header = URL_SAFE
            .decode(token.get(..12).ok_or(FernetError::DecryptFailed)?)
            .map_err(|_| FernetError::DecryptFailed)?;
        let timestamp = u64::from_be_bytes(
            header
                .get(1..9)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(FernetError::DecryptFailed)?,
        );
        // fernet-rs applies a 60s future-clock check even without a TTL. Evaluate
        // at the token's time; the library still verifies its HMAC and padding.
        self.token
            .decrypt_at_time(token, None, timestamp.saturating_sub(60))
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
    fn no_ttl_accepts_a_sender_ahead_of_the_receiver_and_rejects_tampering() {
        let key = FernetKey::generate();
        let sender = FernetToken::new(key.encoded()).unwrap();
        let token = sender.encrypt_at_time(b"migration payload", 4_102_444_800);
        assert!(sender.decrypt(&token).is_err());
        assert_eq!(
            &*Fernet::new(&key).decrypt(token.as_bytes()).unwrap(),
            b"migration payload"
        );
        assert!(
            Fernet::new(&FernetKey::generate())
                .decrypt(token.as_bytes())
                .is_err()
        );
        let mut bytes = URL_SAFE.decode(&token).unwrap();
        bytes[8] ^= 1;
        assert!(
            Fernet::new(&key)
                .decrypt(URL_SAFE.encode(bytes).as_bytes())
                .is_err()
        );
    }

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
