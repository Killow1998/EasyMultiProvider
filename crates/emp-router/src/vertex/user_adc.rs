use super::{CLOUD_SCOPE, HttpClient, HttpMethod, RouterError, Value, auth_error};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Deserialize)]
pub(super) struct UserCredential {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_uri: Option<String>,
    #[serde(skip)]
    cached: Mutex<Option<(String, Instant)>>,
}

impl UserCredential {
    pub(super) fn load() -> Result<Option<Self>, RouterError> {
        let path = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| {
                    PathBuf::from(home).join(".config/gcloud/application_default_credentials.json")
                })
            });
        let Some(path) = path else { return Ok(None) };
        let invalid = || auth_error("Unable to load Google Cloud credentials");
        let contents = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(invalid()),
        };
        let value: Value = serde_json::from_slice(&contents).map_err(|_| invalid())?;
        if value["type"] != "authorized_user" {
            return Ok(None);
        }
        if value["universe_domain"]
            .as_str()
            .is_some_and(|d| d != "googleapis.com")
        {
            return Err(invalid());
        }
        serde_json::from_value(value)
            .map(Some)
            .map_err(|_| invalid())
    }

    pub(super) async fn access_token(&self, client: &HttpClient) -> Result<String, RouterError> {
        let mut cached = self.cached.lock().await;
        let now = Instant::now();
        if let Some((token, expiry)) = cached.as_ref()
            && *expiry > now + Duration::from_secs(10)
        {
            return Ok(token.clone());
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "grant_type":"refresh_token",
            "client_id":self.client_id,
            "client_secret":self.client_secret,
            "refresh_token":self.refresh_token,
            "scopes":CLOUD_SCOPE,
        }))
        .expect("serializable OAuth request");
        let endpoint = self
            .token_uri
            .as_deref()
            .unwrap_or("https://oauth2.googleapis.com/token");
        let response = client
            .open(
                HttpMethod::Post,
                endpoint,
                [("Content-Type".into(), "application/json".into())].into(),
                Some(body),
                false,
            )
            .await
            .map_err(super::transport_error)?;
        if !(200..300).contains(&response.status()) {
            return Err(auth_error("Unable to refresh Google Cloud credentials"));
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: Option<u64>,
        }
        let data = response
            .read_limited(1024 * 1024)
            .await
            .map_err(super::transport_error)?;
        let token: TokenResponse = serde_json::from_slice(&data)
            .map_err(|_| auth_error("Invalid Google Cloud token response"))?;
        if token.access_token.is_empty() {
            return Err(auth_error("Invalid Google Cloud token response"));
        }
        *cached = token
            .expires_in
            .and_then(|ttl| now.checked_add(Duration::from_secs(ttl)))
            .map(|expiry| (token.access_token.clone(), expiry));
        Ok(token.access_token)
    }
}
