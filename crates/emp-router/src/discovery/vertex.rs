//! Google Model Garden catalog; project access is checked by the model service.

use super::*;
use emp_domain::model_metadata::{ModelMetadata, apply_catalog_metadata};

pub(super) async fn discover(
    client: &HttpClient,
    provider: &Map<String, Value>,
    vertex_models: &[ModelMetadata],
    official_models: &OfficialModels,
) -> Result<Vec<Value>, RouterError> {
    if provider["auth_mode"] == "vertex_api_key" {
        if provider["api_key"].as_str().is_none_or(str::is_empty) {
            return Err(discovery_error(
                RouterErrorKind::MissingCredential,
                503,
                FailureClass::RouterError,
                "Vertex API Key is not configured",
            ));
        }
        let models = official_models.enrich_discovered_models(
            provider,
            api_key_candidates(vertex_models, official_models),
        );
        if provider["vertex"]["project"] == "" {
            return verify_express_models(client, provider, models).await;
        }
        return Ok(models);
    }
    let mut headers = crate::vertex::headers(provider, &BTreeMap::new()).await?;
    headers.insert(
        "x-goog-user-project".into(),
        provider["vertex"]["project"]
            .as_str()
            .expect("normalized project")
            .to_owned(),
    );
    let mut budget = DiscoveryBudget::new();
    let mut result = Vec::new();
    for publisher in ["google", "anthropic"] {
        let mut page = None::<String>;
        for _ in 0..MAX_DISCOVERY_PAGES {
            let mut url = Url::parse(&format!(
                "https://aiplatform.googleapis.com/v1beta1/publishers/{publisher}/models"
            ))
            .expect("Google catalog URL");
            url.query_pairs_mut().append_pair("listAllVersions", "true");
            if let Some(token) = &page {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let value = get_json(client, url.as_str(), headers.clone(), &mut budget).await?;
            for model in value
                .get("publisherModels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let base = model["name"]
                    .as_str()
                    .and_then(|name| name.rsplit('/').next())
                    .unwrap_or_default();
                let version = model["versionId"].as_str().unwrap_or_default();
                let raw = if version.starts_with("gemini-") || version.starts_with("claude-") {
                    version.to_owned()
                } else if !version.is_empty() && base.starts_with("claude-") {
                    format!("{base}@{version}")
                } else {
                    base.to_owned()
                };
                let Some(id) = model_id(Some(&json!(raw))) else {
                    continue;
                };
                if !(if publisher == "google" {
                    id.starts_with("gemini-")
                        && !["-embedding", "-image", "-tts", "-audio", "-live"]
                            .iter()
                            .any(|kind| id.contains(kind))
                } else {
                    id.starts_with("claude-")
                }) {
                    continue;
                }
                if result.len() >= MAX_DISCOVERED_MODELS {
                    return Err(model_count_error());
                }
                if result.iter().any(|old: &Value| old["upstream_id"] == id) {
                    continue;
                }
                result.push(json!({"upstream_id":id,"display_name":model["displayName"].as_str().unwrap_or(&id)}));
            }
            page = pagination_token(value.get("nextPageToken"))?;
            if page.is_none() {
                break;
            }
        }
    }
    let mut models = official_models.enrich_discovered_models(provider, result);
    apply_catalog_metadata(&mut models, vertex_models);
    Ok(models)
}

fn is_vertex_text_model(id: &str) -> bool {
    id.starts_with("gemini-")
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && ![
            "-embedding",
            "-image",
            "-tts",
            "-audio",
            "-live",
            "-transcribe",
        ]
        .iter()
        .any(|kind| id.contains(kind))
}

fn api_key_candidates(
    vertex_models: &[ModelMetadata],
    official_models: &OfficialModels,
) -> Vec<Value> {
    let mut models = BTreeMap::new();
    for mut model in official_models.published_text_models("google_gemini") {
        let id = model["upstream_id"].as_str().expect("reference model ID");
        if is_vertex_text_model(id) {
            let id = id.to_owned();
            model["catalog_source"] = json!("official");
            models.insert(id, model);
        }
    }
    for metadata in vertex_models {
        let id = &metadata.upstream_id;
        if !is_vertex_text_model(id) {
            continue;
        }
        let model = models.entry(id.clone()).or_insert_with(
            || json!({"upstream_id":id,"display_name":id,"catalog_source":"litellm"}),
        );
        metadata.apply(model);
    }
    models
        .into_values()
        .map(|mut model| {
            model["availability"] = json!("reference");
            model
        })
        .collect()
}

async fn verify_express_models(
    client: &HttpClient,
    provider: &Map<String, Value>,
    models: Vec<Value>,
) -> Result<Vec<Value>, RouterError> {
    if models.len() > MAX_DISCOVERED_MODELS {
        return Err(model_count_error());
    }
    let headers = crate::vertex::headers(provider, &common_discovery_headers()).await?;
    let probe =
        serde_json::to_vec(&json!({"contents":[{"role":"user","parts":[{"text":"Hello"}]}]}))
            .expect("token probe JSON");
    let mut budget = DiscoveryBudget::new();
    let mut available = Vec::new();
    let mut rejection = None;
    for mut model in models {
        let id = model["upstream_id"].as_str().expect("Vertex candidate ID");
        let url = format!(
            "https://aiplatform.googleapis.com/v1/publishers/google/models/{id}:countTokens"
        );
        match discovery_json(
            client,
            HttpMethod::Post,
            &url,
            headers.clone(),
            Some(probe.clone()),
            &mut budget,
        )
        .await
        {
            Ok(response)
                if response["totalTokens"]
                    .as_u64()
                    .is_some_and(|count| count > 0) =>
            {
                model["availability"] = json!("count_tokens_verified");
                available.push(model);
            }
            Ok(_) => {
                return Err(discovery_error(
                    RouterErrorKind::Protocol,
                    502,
                    FailureClass::ProtocolError,
                    "Vertex countTokens returned an invalid token count",
                ));
            }
            Err(error) if matches!(error.status(), 400 | 403 | 404) => {
                if rejection
                    .as_ref()
                    .is_none_or(|old: &RouterError| old.status() == 404)
                {
                    rejection = Some(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    if available.is_empty()
        && let Some(error) = rejection
    {
        return Err(error);
    }
    Ok(available)
}
