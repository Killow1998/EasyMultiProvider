use super::*;

pub async fn discover_decision_models_with_plan(
    client: &HttpClient,
    base: &str,
    key: &str,
    protocol: &str,
    check_key: bool,
    filter_decisions: bool,
) -> Result<Vec<Value>, RouterError> {
    let base = base.trim_end_matches('/');
    let headers = bearer_discovery_headers(key);
    let mut budget = DiscoveryBudget::new();
    if check_key {
        get_json(client, &format!("{base}/key"), headers.clone(), &mut budget).await?;
    }
    let filter = if filter_decisions {
        "?output_modalities=decisions"
    } else {
        ""
    };
    let catalog = get_json(
        client,
        &format!("{base}/models{filter}"),
        headers,
        &mut budget,
    )
    .await?;
    let native = protocol == "systemone" && catalog.contains_key("models");
    let items = catalog
        .get(if native { "models" } else { "data" })
        .and_then(Value::as_array)
        .filter(|items| items.len() <= MAX_DISCOVERED_MODELS)
        .ok_or_else(|| {
            discovery_error(
                RouterErrorKind::Protocol,
                502,
                FailureClass::ProtocolError,
                "provider returned an invalid decision model list",
            )
        })?;
    let mut models = Vec::new();
    for item in items {
        let Some(id) = item[if native { "name" } else { "id" }]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= MAX_DISCOVERY_FIELD_BYTES)
        else {
            continue;
        };
        let decisions = item["architecture"]["output_modalities"]
            .as_array()
            .is_some_and(|modalities| modalities.iter().any(|modality| modality == "decisions"));
        if !(native || decisions || protocol == "openai" && id == "gpt-6-luna") {
            continue;
        }
        let name = model_text(item.get("name"), id)?;
        models.push(json!({"id":id,"name":name}));
    }
    models.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(models)
}
