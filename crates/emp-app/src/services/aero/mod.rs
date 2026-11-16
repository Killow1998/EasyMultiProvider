mod catalog;
mod runtime;

pub(crate) use catalog::{augment_catalog, augment_public, validate_update};
pub(crate) use runtime::AeroRuntime;
use serde_json::Value;

pub(crate) fn enabled() -> bool {
    crate::VERSION.starts_with("999.")
}

pub(crate) fn route_name(profile: &Value) -> String {
    format!(
        "aero/{}",
        profile["name"].as_str().expect("normalized aero name")
    )
}

pub(crate) fn profile<'a>(config: &'a Value, model: &str) -> Option<&'a Value> {
    config["aero_profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|profile| {
            route_name(profile) == model
                || profile["route_names"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|route| route == model)
        })
}
