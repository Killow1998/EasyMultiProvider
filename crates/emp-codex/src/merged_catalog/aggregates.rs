use super::*;

pub(crate) fn append(
    config: &Value,
    models: &mut Vec<Value>,
    family_names: &BTreeMap<String, String>,
) {
    for group in array(config.get("model_groups")) {
        let sources = array(group.get("sources"))
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|source| models.iter().find(|model| text(model, "slug") == source))
            .collect::<Vec<_>>();
        let Some(first) = sources.first() else {
            continue;
        };
        let mut entry = (*first).clone();
        entry["slug"] = group["id"].clone();
        entry["description"] = json!("Aggregate routing");
        entry["_emp_aggregate"] = json!(true);
        let original = array(config.get("models"))
            .iter()
            .find(|model| model["id"] == first["slug"])
            .unwrap_or(first);
        entry["family_id"] = json!(family_identity(
            original,
            text(first, "slug").rsplit('/').next().expect("source")
        ));
        entry["display_name"] = if let Some(prefix) = group["name_prefix"].as_str() {
            let name = family_names
                .get(text(&entry, "family_id"))
                .cloned()
                .unwrap_or_else(|| strip_context(text(first, "display_name")));
            json!(if prefix.is_empty() {
                name
            } else {
                format!("{prefix} · {name}")
            })
        } else {
            group["name"].clone()
        };
        for field in [
            "context_window",
            "max_context_window",
            "auto_compact_token_limit",
            "default_max_output_tokens",
        ] {
            if let Some(limit) = sources
                .iter()
                .filter_map(|model| model[field].as_u64())
                .min()
            {
                entry[field] = json!(limit);
            }
        }
        for field in [
            "input_modalities",
            "output_modalities",
            "additional_speed_tiers",
        ] {
            if let Some(values) = first[field].as_array() {
                entry[field] = json!(
                    values
                        .iter()
                        .filter(|value| sources.iter().all(|model| model[field]
                            .as_array()
                            .is_some_and(|items| items.contains(value))))
                        .collect::<Vec<_>>()
                );
            }
        }
        if let Some(levels) = first["supported_reasoning_levels"].as_array() {
            entry["supported_reasoning_levels"] = json!(
                levels
                    .iter()
                    .filter(|level| sources.iter().all(|model| array(
                        model.get("supported_reasoning_levels")
                    )
                    .iter()
                    .any(|other| other["effort"] == level["effort"])))
                    .collect::<Vec<_>>()
            );
            if !array(entry.get("supported_reasoning_levels"))
                .iter()
                .any(|level| level["effort"] == entry["default_reasoning_level"])
            {
                entry["default_reasoning_level"] =
                    entry["supported_reasoning_levels"][0]["effort"].clone();
            }
        }
        for field in [
            "supports_reasoning_summaries",
            "supports_reasoning_summary_parameter",
            "supports_image_detail_original",
            "supports_parallel_tool_calls",
        ] {
            entry[field] = json!(sources.iter().all(|model| model[field] == true));
        }
        apply_presentation(config, &mut entry);
        let position = models
            .iter()
            .rposition(|model| {
                array(group.get("sources"))
                    .iter()
                    .any(|source| source.as_str() == Some(text(model, "slug")))
                    || (model["_emp_aggregate"] == true && model["family_id"] == entry["family_id"])
            })
            .expect("at least one source exists");
        models.insert(position + 1, entry);
    }
}
