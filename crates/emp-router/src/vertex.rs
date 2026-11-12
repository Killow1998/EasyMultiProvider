//! Vertex AI routing and Google Cloud credentials.

use super::*;
use google_cloud_auth::credentials::{self, AccessTokenCredentials};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use url::Url;

pub(super) mod gemini;
#[cfg(test)]
mod tests;

type CredentialCache = HashMap<String, ([u8; 32], AccessTokenCredentials)>;
static CREDENTIALS: OnceLock<Mutex<CredentialCache>> = OnceLock::new();
const CLOUD_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

pub(super) struct GeminiContext {
    scope: String,
    store: std::sync::Arc<emp_state::gemini_signatures::GeminiSignatures>,
}

impl std::fmt::Debug for GeminiContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiContext").finish_non_exhaustive()
    }
}

impl GeminiContext {
    pub(super) fn for_route(
        route: &ResolvedRoute,
        store: Option<&std::sync::Arc<emp_state::gemini_signatures::GeminiSignatures>>,
    ) -> Result<Option<Self>, RouterError> {
        let provider = route.provider.value();
        let Some(vertex) = provider.get("vertex") else {
            return Ok(None);
        };
        let model = route
            .upstream_model
            .strip_prefix("google/")
            .unwrap_or(&route.upstream_model);
        if !model.starts_with("gemini-") {
            return Ok(None);
        }
        let store = store.ok_or_else(|| auth_error("Gemini signature storage is unavailable"))?;
        Ok(Some(Self {
            scope: serde_json::json!([provider["id"], vertex["project"], model]).to_string(),
            store: store.clone(),
        }))
    }

    pub(super) fn remember(&self, item: &Value) -> Result<(), RouterError> {
        let Some(signature) = item
            .pointer("/extra_content/google/thought_signature")
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        let Some(id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        self.store
            .put(&self.scope, id, signature)
            .map_err(|_| auth_error("Unable to save Gemini tool signature"))
    }

    pub(super) fn restore(&self, payload: &mut Value) -> Result<(), RouterError> {
        let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
            return Ok(());
        };
        for message in messages {
            let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
                continue;
            };
            for (index, call) in calls.iter_mut().enumerate() {
                let Some(id) = call.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let signature = self
                    .store
                    .get(&self.scope, id)
                    .map_err(|_| auth_error("Unable to read Gemini tool signature"))?;
                if let Some(signature) = signature {
                    call["extra_content"]["google"]["thought_signature"] = Value::String(signature);
                } else if index == 0
                    && call
                        .pointer("/extra_content/google/thought_signature")
                        .is_none()
                {
                    // Google accepts this marker for tool history created by another model.
                    call["extra_content"]["google"]["thought_signature"] =
                        Value::String("skip_thought_signature_validator".to_owned());
                }
            }
        }
        Ok(())
    }
}

pub(super) fn endpoint(route: &ResolvedRoute, stream: bool) -> Result<String, RouterError> {
    let provider = route.provider.value();
    let vertex = provider["vertex"]
        .as_object()
        .expect("normalized Vertex provider");
    let location = vertex["location"].as_str().expect("normalized location");
    let project = vertex["project"].as_str().expect("normalized project");
    let api_key = provider["auth_mode"] == "vertex_api_key";
    let express = api_key && project.is_empty();
    if api_key
        && !route
            .upstream_model
            .strip_prefix("google/")
            .unwrap_or(&route.upstream_model)
            .starts_with("gemini-")
    {
        return Err(invalid_request(
            "Vertex API Key supports Gemini models; other models require Google Cloud credentials",
        ));
    }
    let host = match if express { "global" } else { location } {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
        _ => format!("{location}-aiplatform.googleapis.com"),
    };
    let mut url = Url::parse(&format!("https://{host}/v1")).expect("valid Google URL");
    {
        let mut path = url
            .path_segments_mut()
            .expect("Google URL has path segments");
        path.pop_if_empty();
        if !express {
            path.extend(["projects", project, "locations", location]);
        }
        if route.protocol == Protocol::AnthropicMessages {
            if api_key {
                return Err(invalid_request(
                    "Vertex API Key supports Gemini models; Claude requires Google Cloud credentials",
                ));
            }
            let model = route
                .upstream_model
                .strip_prefix("anthropic/")
                .unwrap_or(&route.upstream_model);
            path.extend(["publishers", "anthropic", "models"]);
            path.push(&format!(
                "{model}:{}",
                if stream {
                    "streamRawPredict"
                } else {
                    "rawPredict"
                }
            ));
        } else if api_key {
            let model = route
                .upstream_model
                .strip_prefix("google/")
                .unwrap_or(&route.upstream_model);
            path.extend(["publishers", "google", "models"]);
            path.push(&format!(
                "{model}:{}",
                if stream {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                }
            ));
        } else {
            path.extend(["endpoints", "openapi", "chat", "completions"]);
        }
    }
    if api_key && stream {
        url.query_pairs_mut().append_pair("alt", "sse");
    }
    Ok(url.into())
}

pub(super) async fn headers(
    provider: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, RouterError> {
    let mut headers = base_headers(incoming);
    let mode = provider["auth_mode"]
        .as_str()
        .expect("normalized Vertex authentication");
    let secret = provider["api_key"].as_str().unwrap_or_default();
    if mode == "vertex_api_key" {
        if secret.is_empty() {
            return Err(auth_error("Vertex API Key is not configured"));
        }
        headers.insert("x-goog-api-key".to_owned(), secret.to_owned());
        return Ok(headers);
    }
    let mut digest = Sha256::new();
    digest.update(mode.as_bytes());
    digest.update([0]);
    digest.update(secret.as_bytes());
    let fingerprint: [u8; 32] = digest.finalize().into();
    let id = provider["id"].as_str().expect("normalized provider ID");
    let credential = {
        let mut cache = CREDENTIALS
            .get_or_init(Default::default)
            .lock()
            .expect("credential cache lock");
        if let Some((_, credential)) = cache.get(id).filter(|(old, _)| *old == fingerprint) {
            credential.clone()
        } else {
            let credential = if mode == "vertex_adc" {
                credentials::Builder::default()
                    .with_scopes([CLOUD_SCOPE])
                    .build_access_token_credentials()
            } else {
                let key = serde_json::from_str(secret)
                    .map_err(|_| auth_error("Vertex service account JSON is invalid"))?;
                credentials::service_account::Builder::new(key)
                    .with_access_specifier(
                        credentials::service_account::AccessSpecifier::from_scopes([CLOUD_SCOPE]),
                    )
                    .build_access_token_credentials()
            }
            .map_err(|_| auth_error("Unable to load Google Cloud credentials"))?;
            cache.insert(id.to_owned(), (fingerprint, credential.clone()));
            credential
        }
    };
    let token = credential
        .access_token()
        .await
        .map_err(|_| auth_error("Unable to refresh Google Cloud credentials"))?;
    headers.insert(
        "Authorization".to_owned(),
        format!("Bearer {}", token.token),
    );
    Ok(headers)
}

fn auth_error(message: &'static str) -> RouterError {
    RouterError::new(
        RouterErrorKind::MissingCredential,
        503,
        FailureClass::RouterError,
        Some("vertex_credentials_unavailable".to_owned()),
        None,
        message,
    )
}
