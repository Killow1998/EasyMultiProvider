//! Credential-free model and family controls for the unchanged management UI.
use crate::merged_catalog::{build_catalog, family_identity, strip_context_prefix, usable_context};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Project only UI fields from one already resolved set of catalog sources.
pub fn model_views(
    config: &Value,
    native: &Value,
    account_catalogs: &BTreeMap<String, Value>,
    duplicates: &BTreeMap<String, String>,
) -> Value {
    let catalog = build_catalog(config, native, account_catalogs, duplicates);
    let mut baseline_config = config.clone();
    baseline_config["catalog_presentations"] = json!({});
    baseline_config["catalog_family_presentations"] = json!({});
    let baseline = build_catalog(&baseline_config, native, account_catalogs, duplicates);
    let models = catalog_models(config, native, &catalog, &baseline);
    let families = catalog_families(config, &models);
    json!({
        "subscription_models":subscription_model_options(native),
        "catalog_models":models, "catalog_families":families,
    })
}

/// Subscription edit controls use the owner's unmodified catalog limits, not
/// context overrides or display visibility from EMP configuration.
pub fn subscription_model_options(catalog: &Value) -> Vec<Value> {
    array(catalog, "models").iter().filter(|model| {
        model.is_object() && !text(model, "slug").trim().is_empty()
            && visible(model) && model.get("supported_in_api") != Some(&Value::Bool(false))
    }).map(|model| {
        let percentage = model.get("effective_context_window_percent")
            .filter(|value| truthy(value)).cloned().unwrap_or(json!(95));
        let mut effective = model.clone();
        effective["effective_context_window_percent"] = percentage.clone();
        let maximum = model.get("max_context_window").filter(|value| !value.is_null())
            .or_else(|| model.get("context_window")).and_then(crate::context_limit).unwrap_or(0);
        json!({
            "id":text(model,"slug"), "display_name":nonempty(model,"display_name").unwrap_or(text(model,"slug")),
            "description":text(model,"description"), "context_window":usable_context(&effective),
            "default_context_window":model.get("context_window").cloned().unwrap_or(json!(0)),
            "max_context_window":maximum, "effective_context_window_percent":percentage,
            "supports_reasoning_summaries":model.get("supports_reasoning_summary_parameter")==Some(&Value::Bool(true))
                || model.get("supports_reasoning_summaries")==Some(&Value::Bool(true)),
        })
    }).collect()
}

fn catalog_models(config: &Value, native: &Value, catalog: &Value, baseline: &Value) -> Vec<Value> {
    array(catalog,"models").iter().filter(|model| {
        model.is_object() && visible(model) && model.get("supported_in_api")!=Some(&Value::Bool(false))
            && !text(model,"slug").is_empty()
    }).map(|model| {
        let route = text(model,"slug");
        let external = array(config,"models").iter().find(|model| text(model,"id")==route);
        let (kind, source_id, source, fallback, verified) = if let Some(external) = external {
            ("provider", text(external,"provider"), external, route,
                ["family_id", "upstream_id"].iter().any(|field|!text(external,field).trim().is_empty()))
        } else if let Some((account, suffix)) = array(config,"accounts").iter().filter(|account| {
            !text(account,"id").is_empty() && !text(account,"prefix").is_empty()
        }).find_map(|account| route.strip_prefix(&format!("{}/",text(account,"prefix"))).map(|suffix|(account,suffix))) {
            ("account", text(account,"id"), native_model(native,suffix), suffix, true)
        } else {
            ("native", "", native_model(native,route), route, true)
        };
        let default_name = array(baseline,"models").iter().find(|entry|text(entry,"slug")==route)
            .and_then(|entry|nonempty(entry,"display_name")).unwrap_or(route);
        json!({
            "id":route, "display_name":nonempty(model,"display_name").unwrap_or(route),
            "default_display_name":strip_context_prefix(default_name), "context_window":usable_context(model),
            "source_type":kind, "source_id":source_id, "family_id":family_identity(source,fallback),
            "family_verified":verified,
            "supports_reasoning_summaries":model.get("supports_reasoning_summary_parameter")==Some(&Value::Bool(true)),
        })
    }).collect()
}

/// Keep native/account/provider preference and first-seen family ordering.
pub fn catalog_families(config: &Value, models: &[Value]) -> Vec<Value> {
    let mut groups = Vec::<Value>::new();
    for model in models {
        let family = nonempty(model, "family_id").unwrap_or(text(model, "id"));
        let position = groups.iter().position(|group|text(group,"id")==family).unwrap_or_else(|| {
            groups.push(json!({
                "id":family, "default_display_name":model["default_display_name"],
                "context_window":model["context_window"], "supports_reasoning_summaries":true,"routes":[],
            }));
            groups.len()-1
        });
        let group = &mut groups[position];
        let preferred = array(group, "routes")
            .iter()
            .map(|route| rank(text(route, "source_type")))
            .min()
            .unwrap_or(9);
        if rank(text(model, "source_type")) < preferred {
            group["default_display_name"] = model["default_display_name"].clone();
            group["context_window"] = model["context_window"].clone();
        }
        group["supports_reasoning_summaries"] = json!(
            group["supports_reasoning_summaries"] == true
                && model["supports_reasoning_summaries"] == true
        );
        group["routes"].as_array_mut().expect("routes").push(json!({
            "id":model["id"],"source_type":model["source_type"],"source_id":model["source_id"],
        }));
    }
    for group in &mut groups {
        let presentation = config
            .get("catalog_family_presentations")
            .and_then(|value| value.get(text(group, "id")))
            .filter(|value| truthy(value))
            .or_else(|| {
                config
                    .get("catalog_presentations")
                    .and_then(|value| value.get(text(&group["routes"][0], "id")))
            })
            .filter(|value| value.is_object())
            .unwrap_or(&Value::Null);
        let alias = text(presentation, "catalog_alias");
        group["presentation"] = json!({
            "catalog_alias":alias, "show_context":presentation.get("show_context")!=Some(&Value::Bool(false)),
            "reasoning_summary":nonempty(presentation,"reasoning_summary").unwrap_or("auto"),
        });
        group["display_name"] = if alias.is_empty() {
            group["default_display_name"].clone()
        } else {
            json!(alias)
        };
    }
    groups
}
fn rank(source: &str) -> usize {
    match source {
        "native" => 0,
        "account" => 1,
        "provider" => 2,
        _ => 9,
    }
}
fn visible(model: &Value) -> bool {
    model.get("visibility").is_none_or(|value| value == "list")
}
fn native_model<'a>(native: &'a Value, route: &str) -> &'a Value {
    array(native, "models")
        .iter()
        .find(|model| text(model, "slug") == route)
        .unwrap_or(&Value::Null)
}
fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or_default()
}
fn nonempty<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}
fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Number(value) => value.as_f64() != Some(0.0),
    }
}
