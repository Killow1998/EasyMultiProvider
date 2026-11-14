//! Project an external model into the Codex catalog contract.
use super::{
    DEFAULT_EFFECTIVE_CONTEXT_PERCENT, description_with_context, display_name, integer, nonempty,
    text,
};
use emp_state::{codex_input_modalities, normalize_reasoning_levels};
use serde_json::{Map, Value, json};

pub(super) fn entry(model: &Value, template: &Value, provider: &Value) -> Value {
    let claude_cli = text(provider, "execution_backend") == "claude_cli";
    let mut entry = Map::new();
    // Codex requires this even before a native catalog has been observed.
    // Match its unknown-model fallback; a supplied template takes precedence.
    entry.insert(
        "truncation_policy".to_owned(),
        json!({"mode":"bytes", "limit":10_000}),
    );
    for field in [
        "base_instructions",
        "shell_type",
        "truncation_policy",
        "include_skills_usage_instructions",
        "include_plugin_usage_instructions",
        "include_apps_usage_instructions",
        "node_repl_auto_review_required",
        "node_repl_disabled",
    ] {
        if let Some(value) = template.get(field) {
            entry.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(messages) = template.get("model_messages").and_then(Value::as_object) {
        let allowed = [
            "instructions_template",
            "instructions_variables",
            "tools",
            "approvals",
            "collaboration_modes",
            "auto_review",
            "permissions",
            "multi_agent",
        ];
        entry.insert(
            "model_messages".to_owned(),
            Value::Object(
                messages
                    .iter()
                    .filter(|(key, _)| allowed.contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
        );
    }
    let levels = normalize_reasoning_levels(model.get("reasoning_levels"));
    let protocol = if text(provider, "protocol") == "auto" {
        nonempty(model, "resolved_protocol")
            .or_else(|| nonempty(provider, "resolved_protocol"))
            .unwrap_or("auto")
    } else {
        text(provider, "protocol")
    };
    let summaries = model.get("supports_reasoning_summaries") == Some(&Value::Bool(true))
        && protocol == "responses";
    let mut input_modalities = codex_input_modalities(model.get("input_modalities"));
    if !claude_cli
        && matches!(protocol, "responses" | "chat_completions")
        && model["input_modalities"]
            .as_array()
            .is_some_and(|modes| modes.iter().any(|mode| mode == "audio"))
    {
        input_modalities.push("audio".into());
    }
    let supports_image_detail_original =
        !claude_cli && model.get("supports_image_detail_original") == Some(&Value::Bool(true));
    let friendly = text(model, "display_name").trim();
    let description = text(model, "description").trim();
    let description =
        if description.is_empty() && !friendly.is_empty() && friendly != text(model, "id") {
            friendly
        } else {
            description
        };
    let route = text(model, "id");
    let source_prefix = format!("{}/", text(model, "provider"));
    let name = match (
        nonempty(provider, "name"),
        route.strip_prefix(&source_prefix),
    ) {
        (Some(source), Some(model)) => format!("{source} · {model}"),
        _ => route.to_owned(),
    };
    let parallel_tools = [model, provider]
        .into_iter()
        .find_map(|source| {
            source["parallel_tools"]
                .as_bool()
                .or_else(|| source["capabilities"]["parallel_tools"].as_bool())
        })
        .unwrap_or(false);
    let mut entry = Value::Object(entry);
    let fields = json!({
        "service_tiers":[], "additional_speed_tiers":[], "default_service_tier":null, "availability_nux":null,
        "upgrade":null, "model_specialty":null, "experimental_supported_tools":[], "multi_agent_reasoning_effort":null,
        "use_responses_lite":false, "supports_experimental_context":false, "priority":0,
        "slug":route, "display_name":name,
        "description":if description.is_empty() { "External provider model" } else { description },
        "visibility":model.get("visibility").cloned().unwrap_or(json!("list")), "supported_in_api":true,
        "input_modalities":input_modalities,
        "supports_reasoning_summaries":summaries, "supports_reasoning_summary_parameter":summaries,
        "default_reasoning_summary":if summaries {"auto"} else {"none"},
        "support_verbosity":false, "default_verbosity":null, "supports_search_tool":true,
        "supports_image_detail_original":supports_image_detail_original,
        "supports_parallel_tool_calls":parallel_tools,
        "apply_patch_tool_type":null, "multi_agent_version":null, "supported_reasoning_levels":[],
    });
    entry
        .as_object_mut()
        .expect("object")
        .extend(fields.as_object().expect("object").clone());
    if !levels.is_empty() {
        let cli_default = claude_cli
            .then(|| {
                emp_state::official_registry::claude_cli_default_effort(text(model, "upstream_id"))
            })
            .flatten()
            .filter(|level| levels.contains(level));
        entry["default_reasoning_level"] = json!(
            cli_default
                .as_deref()
                .unwrap_or(&levels[(levels.len() - 1) / 2])
        );
        entry["supported_reasoning_levels"] = json!(
            levels
                .iter()
                .map(|level| {
                    let description = match level.as_str() {
                        "minimal" => "Fast responses with minimal reasoning",
                        "low" => "Fast responses with lighter reasoning",
                        "medium" => "Balances speed and reasoning depth",
                        "high" => "Greater reasoning depth for complex tasks",
                        "xhigh" => "Extra high reasoning depth for hard tasks",
                        "max" => "Maximum reasoning depth",
                        "ultra" => "Maximum reasoning with automatic task delegation",
                        _ => level.as_str(),
                    };
                    json!({"effort":level,"description":description})
                })
                .collect::<Vec<_>>()
        );
    }
    let context = integer(model.get("context_window")).min(100_000_000);
    if context != 0 {
        entry["context_window"] = json!(context);
        entry["max_context_window"] = json!(context);
        entry["auto_compact_token_limit"] = json!(context * 4 / 5);
        // Codex applies this share of the window to every model; state it so
        // labels and the management UI show what Codex will actually use.
        entry["effective_context_window_percent"] = model
            .get("effective_context_window_percent")
            .filter(|value| {
                value
                    .as_f64()
                    .is_some_and(|value| value > 0.0 && value <= 100.0)
            })
            .cloned()
            .unwrap_or(json!(DEFAULT_EFFECTIVE_CONTEXT_PERCENT));
    }
    // Keep default labels stable before applying family and route overrides.
    entry["display_name"] = json!(display_name(&entry, &json!({}), true));
    entry["description"] = json!(description_with_context(&entry, &json!({}), true));
    entry
}
