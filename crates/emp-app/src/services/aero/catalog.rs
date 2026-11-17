use super::{enabled, route_name};
use serde_json::{Value, json};

fn role_models<'a>(profile: &Value, models: &'a Value, key: &str) -> Option<[&'a Value; 2]> {
    let models = models.as_array().expect("catalog models");
    Some([
        models
            .iter()
            .find(|model| model[key] == profile["planner"]["model"])?,
        models
            .iter()
            .find(|model| model[key] == profile["executor"]["model"])?,
    ])
}

pub(crate) fn augment_catalog(config: &Value, catalog: &mut Value) {
    if !enabled() {
        return;
    }
    for profile in config["aero_profiles"].as_array().into_iter().flatten() {
        let Some(roles) = role_models(profile, &catalog["models"], "slug") else {
            continue;
        };
        let mut model = roles[0].clone();
        let window = roles
            .iter()
            .filter_map(|role| role["context_window"].as_u64())
            .min()
            .unwrap_or(128000);
        let modalities: Vec<Value> = roles[0]["input_modalities"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![json!("text")])
            .into_iter()
            .filter(|modality| {
                roles[1]["input_modalities"]
                    .as_array()
                    .map_or(*modality == "text", |values| values.contains(modality))
            })
            .collect();
        let fields = json!({
            "slug":route_name(profile), "display_name":format!("aero · {}",profile["name"].as_str().expect("profile name")),
            "description":"Orchestrator and executor through aero", "visibility":"list", "supported_in_api":true,
            "default_reasoning_level":null,"supported_reasoning_levels":[],"supports_reasoning_effort_updates":false,
            "supports_reasoning_summaries":false,"supports_reasoning_summary_parameter":false,
            "prefer_websockets":false,"use_responses_lite":false,
            "context_window":window,"max_context_window":window,"auto_compact_token_limit":null,
            "input_modalities":modalities,
            "service_tiers":[],"additional_speed_tiers":[],"upgrade":null,"availability_nux":null,
        });
        model
            .as_object_mut()
            .expect("catalog model")
            .extend(fields.as_object().expect("fields").clone());
        catalog["models"]
            .as_array_mut()
            .expect("catalog models")
            .push(model);
    }
}

pub(crate) fn augment_public(config: &Value, public: &mut Value) {
    public["aero_enabled"] = json!(enabled());
    public["experimental"] = json!(enabled());
    if !enabled() {
        return;
    }
    public["catalog_models"]
        .as_array_mut()
        .expect("model views")
        .retain(|model| model["source_type"] != "aero");
    public["catalog_families"]
        .as_array_mut()
        .expect("family views")
        .retain(|family| family["routes"][0]["source_type"] != "aero");
    for profile in config["aero_profiles"].as_array().into_iter().flatten() {
        let route = route_name(profile);
        let Some(roles) = role_models(profile, &public["catalog_models"], "id") else {
            continue;
        };
        let window = roles
            .iter()
            .filter_map(|role| role["context_window"].as_u64())
            .min()
            .unwrap_or(128000);
        let name = format!("aero · {}", profile["name"].as_str().expect("profile name"));
        public["catalog_models"]
            .as_array_mut()
            .expect("model views")
            .push(json!({
                "id":route,"family_id":profile["id"],"source_id":profile["id"],"source_type":"aero",
                "display_name":name,"default_display_name":name,
                "context_window":window,"reasoning_levels":[],"family_verified":true,
            "ultra_reasoning_levels":[],"ultra_default_effort":null,
            }));
        public["catalog_families"].as_array_mut().expect("family views").push(json!({
            "id":profile["id"],"display_name":name,"default_display_name":name,
            "context_window":window,"routes":[{"id":route,"source_id":profile["id"],"source_type":"aero"}],
            "presentation":{"catalog_alias":"","show_context":true,"reasoning_summary":"auto"},
        }));
    }
}

pub(crate) fn validate_update(
    previous: &Value,
    updated: &Value,
    read_catalog: impl FnOnce() -> Value,
) -> Result<(), String> {
    let Some(profiles) = updated["aero_profiles"]
        .as_array()
        .filter(|profiles| !profiles.is_empty())
    else {
        return Ok(());
    };
    let catalog = read_catalog();
    let visible_catalog = emp_state::config::client_catalog(updated, catalog.clone());
    for profile in profiles {
        let previous = previous["aero_profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|old| old["id"] == profile["id"]);
        for role in ["planner", "executor"] {
            if previous.is_some_and(|old| old[role] == profile[role]) {
                continue;
            }
            if !catalog["models"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|model| model["slug"] == profile[role]["model"])
            {
                return Err("aero.choose_models".into());
            }
        }
        if catalog["models"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(visible_catalog["models"].as_array().into_iter().flatten())
            .any(|model| {
                model["slug"] == route_name(profile)
                    || profile["route_names"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|route| model["slug"] == *route)
            })
        {
            return Err("aero.duplicate_name".into());
        }
    }
    Ok(())
}
