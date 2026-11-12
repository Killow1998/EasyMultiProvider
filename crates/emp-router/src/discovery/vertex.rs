//! Google Model Garden catalog; project access is checked by the model service.

use super::*;

pub(super) async fn discover(
    client: &HttpClient,
    provider: &Map<String, Value>,
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
        // Model Garden requires OAuth; API keys use a published reference list.
        let ids: Vec<String> = if provider["vertex"]["project"] == "" {
            [
                "gemini-3.1-pro-preview",
                "gemini-3-flash-preview",
                "gemini-2.5-pro",
                "gemini-2.5-flash",
                "gemini-2.5-flash-lite",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        } else {
            emp_state::official_registry::published_text_model_ids("google_gemini")
                .into_iter()
                .filter(|id| id.starts_with("gemini-"))
                .collect()
        };
        let models = ids
            .into_iter()
            .map(|id| json!({"upstream_id":id,"display_name":id}))
            .collect();
        return Ok(enrich_discovered_models(provider, models));
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
    Ok(enrich_discovered_models(provider, result))
}
