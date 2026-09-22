//! Codex catalog composition from already resolved native and account snapshots.
use emp_state::{codex_input_modalities, normalize_reasoning_levels};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Compose Python's catalog without reading credentials or touching Codex files.
/// Account snapshot ownership and duplicate identity are resolved by the caller.
pub fn build_catalog(
    config: &Value,
    native: &Value,
    account_catalogs: &BTreeMap<String, Value>,
    duplicates: &BTreeMap<String, String>,
) -> Value {
    let mut hidden = strings(config.get("native_hidden_models"));
    for account in array(config.get("accounts")) {
        if duplicates
            .get(text(account, "id"))
            .is_some_and(|source| source == "当前 Codex 登录")
        {
            hidden.extend(strings(account.get("hidden_models")));
        }
    }
    let mut result = array(native.get("models"))
        .iter()
        .filter(|model| {
            model.is_object()
                && model.get("supported_in_api") != Some(&Value::Bool(false))
                && !(hidden.contains(text(model, "slug"))
                    && model.get("visibility").and_then(Value::as_str) != Some("hide"))
        })
        .cloned()
        .collect::<Vec<_>>();
    for (index, model) in result.iter_mut().enumerate() {
        super::apply_subscription_context(
            model.as_object_mut().expect("object"),
            config.get("native_model_context_windows"),
        );
        let family = family_identity(model, text(model, "slug"));
        metadata(model, &family, true, 0, index, 0);
    }
    let template = result.first().cloned().unwrap_or_else(|| json!({
        "base_instructions":"You are a helpful coding assistant.", "model_messages":{}, "shell_type":"shell_command"
    }));
    let mut existing = result
        .iter()
        .map(|model| text(model, "slug").to_owned())
        .collect::<BTreeSet<_>>();
    for (account_index, account) in array(config.get("accounts")).iter().enumerate() {
        if account.get("enabled") == Some(&Value::Bool(false))
            || text(account, "auth_file").is_empty()
            || text(account, "credential_status") == "invalid"
            || duplicates.contains_key(text(account, "id"))
        {
            continue;
        }
        let hidden = strings(account.get("hidden_models"));
        let catalog = account_catalogs.get(text(account, "id")).unwrap_or(native);
        for model in array(catalog.get("models")) {
            let slug = text(model, "slug");
            if !model.is_object()
                || slug.trim().is_empty()
                || model
                    .get("visibility")
                    .and_then(Value::as_str)
                    .unwrap_or("list")
                    != "list"
                || model.get("supported_in_api") == Some(&Value::Bool(false))
                || hidden.contains(slug)
            {
                continue;
            }
            let mut entry = model.clone();
            super::apply_subscription_context(
                entry.as_object_mut().expect("object"),
                account.get("model_context_windows"),
            );
            entry
                .as_object_mut()
                .expect("object")
                .remove("available_access_programs");
            let route = format!("{}/{slug}", text(account, "prefix"));
            if !existing.insert(route.clone()) {
                continue;
            }
            let label = nonempty(account, "name").unwrap_or(text(account, "prefix"));
            entry["slug"] = json!(route);
            entry["display_name"] = json!(format!(
                "{label} · {}",
                strip_context(nonempty(model, "display_name").unwrap_or(slug))
            ));
            entry["description"] = json!(format!("ChatGPT subscription: {label}"));
            entry["_emp_source_label"] = json!(label);
            entry["visibility"] = model.get("visibility").cloned().unwrap_or(json!("list"));
            entry["supported_in_api"] = model
                .get("supported_in_api")
                .cloned()
                .unwrap_or(json!(true));
            let family = family_identity(model, slug);
            metadata(&mut entry, &family, true, 1, account_index, 0);
            result.push(entry);
        }
    }
    let providers = array(config.get("providers"));
    let mut external: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let empty = json!({});
    for (index, model) in array(config.get("models")).iter().enumerate() {
        if model.get("enabled") == Some(&Value::Bool(false)) || existing.contains(text(model, "id"))
        {
            continue;
        }
        let provider_id = text(model, "provider");
        let provider_index = providers
            .iter()
            .position(|provider| text(provider, "id") == provider_id)
            .unwrap_or(providers.len());
        let provider = providers.get(provider_index).unwrap_or(&empty);
        let mut entry = external_entry(model, &template, provider);
        let family = family_identity(model, text(model, "id"));
        let verified = ["family_id", "upstream_id"]
            .iter()
            .any(|field| !text(model, field).trim().is_empty());
        metadata(&mut entry, &family, verified, 2, provider_index, index);
        entry["_emp_release"] = json!(integer(model.get("created_at")).max(0));
        entry["_emp_source_label"] = json!(
            nonempty(provider, "name")
                .or_else(|| nonempty(provider, "id"))
                .unwrap_or(provider_id)
        );
        external
            .entry(provider_id.to_owned())
            .or_default()
            .push(entry);
    }
    let mut order = providers
        .iter()
        .map(|provider| text(provider, "id").to_owned())
        .collect::<Vec<_>>();
    order.extend(
        external
            .keys()
            .filter(|id| {
                !providers
                    .iter()
                    .any(|provider| text(provider, "id") == id.as_str())
            })
            .cloned(),
    );
    for provider in order {
        if let Some(mut entries) = external.remove(&provider) {
            entries.sort_by_key(|entry| {
                (
                    -integer(entry.get("_emp_release")),
                    integer(entry.get("_emp_order")),
                )
            });
            result.extend(entries);
        }
    }
    let mut families = BTreeMap::<String, (i64, bool, usize)>::new();
    for entry in &result {
        let order = families.len();
        let family = text(entry, "_emp_family").to_owned();
        let state = families.entry(family).or_insert((0, false, order));
        state.0 = state.0.max(integer(entry.get("_emp_release")));
        state.1 |= entry.get("_emp_family_verified") == Some(&Value::Bool(true));
    }
    result.sort_by(|left, right| {
        let key = |entry: &Value| {
            let family = text(entry, "_emp_family");
            let (release, verified, order) = families[family];
            (
                -release,
                !verified,
                if verified {
                    family.to_owned()
                } else {
                    String::new()
                },
                if verified { 0 } else { order },
                integer(entry.get("_emp_source_rank")),
                integer(entry.get("_emp_source_order")),
                integer(entry.get("_emp_order")),
                text(entry, "slug").to_owned(),
            )
        };
        key(left).cmp(&key(right))
    });
    for entry in &mut result {
        apply_presentation(config, entry);
        for field in [
            "_emp_family",
            "_emp_family_verified",
            "_emp_release",
            "_emp_source_rank",
            "_emp_source_order",
            "_emp_order",
            "_emp_source_label",
        ] {
            entry.as_object_mut().expect("model object").remove(field);
        }
    }
    json!({"models":result})
}

fn metadata(
    entry: &mut Value,
    family: &str,
    verified: bool,
    rank: usize,
    source: usize,
    order: usize,
) {
    entry["_emp_family"] = json!(family);
    entry["_emp_family_verified"] = json!(verified);
    entry["_emp_release"] = json!(integer(entry.get("created_at")).max(0));
    entry["_emp_source_rank"] = json!(rank);
    entry["_emp_source_order"] = json!(source);
    entry["_emp_order"] = json!(order);
}
pub(crate) fn family_identity(model: &Value, fallback: &str) -> String {
    ["family_id", "upstream_id"]
        .iter()
        .map(|field| text(model, field).trim())
        .find(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}
fn external_entry(model: &Value, template: &Value, provider: &Value) -> Value {
    let mut entry = Map::new();
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
    let summaries = model.get("supports_reasoning_summaries") == Some(&Value::Bool(true))
        && (text(provider, "protocol") == "responses"
            || (text(provider, "protocol") == "auto"
                && nonempty(model, "resolved_protocol")
                    .or_else(|| nonempty(provider, "resolved_protocol"))
                    == Some("responses")));
    let friendly = text(model, "display_name").trim();
    let description = text(model, "description").trim();
    let description =
        if description.is_empty() && !friendly.is_empty() && friendly != text(model, "id") {
            friendly
        } else {
            description
        };
    let mut entry = Value::Object(entry);
    let fields = json!({
        "service_tiers":[], "additional_speed_tiers":[], "default_service_tier":null, "availability_nux":null,
        "upgrade":null, "model_specialty":null, "experimental_supported_tools":[], "multi_agent_reasoning_effort":null,
        "use_responses_lite":false, "supports_experimental_context":false, "priority":0,
        "slug":text(model,"id"), "display_name":text(model,"id"),
        "description":if description.is_empty() { "External provider model" } else { description },
        "visibility":model.get("visibility").cloned().unwrap_or(json!("list")), "supported_in_api":true,
        "input_modalities":codex_input_modalities(model.get("input_modalities")),
        "supports_reasoning_summaries":summaries, "supports_reasoning_summary_parameter":summaries,
        "default_reasoning_summary":if summaries {"auto"} else {"none"},
        "support_verbosity":false, "default_verbosity":null, "supports_search_tool":true,
        "supports_image_detail_original":model.get("supports_image_detail_original") == Some(&Value::Bool(true)),
        "supports_parallel_tool_calls":boolean_capability(model,provider,"parallel_tools"),
        "apply_patch_tool_type":null, "multi_agent_version":null, "supported_reasoning_levels":[],
    });
    entry
        .as_object_mut()
        .expect("object")
        .extend(fields.as_object().expect("object").clone());
    if !levels.is_empty() {
        entry["default_reasoning_level"] = json!(levels[(levels.len() - 1) / 2]);
        entry["supported_reasoning_levels"] = json!(
            levels
                .iter()
                .map(|level| json!({"effort":level,"description":effort_description(level)}))
                .collect::<Vec<_>>()
        );
    }
    let context = integer(model.get("context_window")).min(100_000_000);
    if context != 0 {
        entry["context_window"] = json!(context);
        entry["max_context_window"] = json!(context);
        entry["auto_compact_token_limit"] = json!(context * 4 / 5);
    }
    // The Python entry is first decorated without user overrides, then again
    // during family/route presentation. Preserve both passes for exact labels.
    entry["display_name"] = json!(display_name(&entry, &json!({})));
    entry["description"] = json!(description_with_context(&entry, &json!({})));
    entry
}
fn boolean_capability(model: &Value, provider: &Value, field: &str) -> bool {
    for source in [model, provider] {
        if let Some(value) = source.get(field).and_then(Value::as_bool).or_else(|| {
            source
                .get("capabilities")
                .and_then(|caps| caps.get(field))
                .and_then(Value::as_bool)
        }) {
            return value;
        }
    }
    false
}
fn effort_description(level: &str) -> &str {
    match level {
        "minimal" => "Fast responses with minimal reasoning",
        "low" => "Fast responses with lighter reasoning",
        "medium" => "Balances speed and reasoning depth",
        "high" => "Greater reasoning depth for complex tasks",
        "xhigh" => "Extra high reasoning depth for hard tasks",
        "max" => "Maximum reasoning depth",
        "ultra" => "Maximum reasoning with automatic task delegation",
        _ => level,
    }
}
fn apply_presentation(config: &Value, entry: &mut Value) {
    let empty = json!({});
    let family = config
        .get("catalog_family_presentations")
        .and_then(|values| values.get(text(entry, "_emp_family")))
        .filter(|value| value.as_object().is_some_and(|value| !value.is_empty()))
        .filter(|_| entry.get("_emp_family_verified") == Some(&Value::Bool(true)));
    let mut presentation = family
        .or_else(|| {
            config
                .get("catalog_presentations")
                .and_then(|values| values.get(text(entry, "slug")))
        })
        .filter(|value| value.is_object())
        .unwrap_or(&empty)
        .clone();
    if family.is_some() {
        presentation["_family_scoped"] = true.into();
    }
    entry["display_name"] = json!(display_name(entry, &presentation));
    entry["description"] = json!(description_with_context(entry, &presentation));
    let policy = text(&presentation, "reasoning_summary");
    let supports = entry
        .get("supports_reasoning_summary_parameter")
        .and_then(Value::as_bool)
        .unwrap_or(entry.get("supports_reasoning_summaries") == Some(&Value::Bool(true)));
    if policy == "hide" || (policy == "show" && !supports) {
        entry["default_reasoning_summary"] = json!("none");
    } else if policy == "show" && supports {
        entry["default_reasoning_summary"] = json!("auto");
    }
}
fn display_name(model: &Value, presentation: &Value) -> String {
    let alias = text(presentation, "catalog_alias");
    let name = if !alias.is_empty() {
        let source = if presentation.get("_family_scoped") == Some(&Value::Bool(true)) {
            text(model, "_emp_source_label")
        } else {
            ""
        };
        if source.is_empty() {
            alias.to_owned()
        } else {
            format!("{source} · {alias}")
        }
    } else {
        strip_context(
            nonempty(model, "display_name")
                .or_else(|| nonempty(model, "slug"))
                .or_else(|| nonempty(model, "id"))
                .unwrap_or_default(),
        )
    };
    if presentation.get("show_context") == Some(&Value::Bool(false)) {
        return name;
    }
    let context = usable_context(model);
    let label = if context == 0 {
        "?".to_owned()
    } else {
        compact_context(context)
    };
    format!("[{label:>5}]  {name}")
}
fn description_with_context(model: &Value, presentation: &Value) -> String {
    let description = text(model, "description").trim();
    let mut description = description.to_owned();
    if let Some(index) = description.rfind("Context ")
        && context_number(&description[index + 8..])
    {
        let before = description[..index].trim_end();
        description = before
            .strip_suffix('·')
            .unwrap_or(before)
            .trim_end()
            .to_owned();
    }
    let alias = text(presentation, "catalog_alias");
    if !alias.is_empty() && description != alias && !description.starts_with(&format!("{alias} · "))
    {
        description = if description.is_empty() {
            alias.to_owned()
        } else {
            format!("{alias} · {description}")
        };
    }
    let context = usable_context(model);
    if presentation.get("show_context") == Some(&Value::Bool(false)) || context == 0 {
        return description;
    }
    let context = format!("Context {}", compact_context(context));
    if description.is_empty() {
        context
    } else {
        format!("{description} · {context}")
    }
}
fn strip_context(name: &str) -> String {
    let mut name = strip_context_prefix(name);
    if let Some(before) = name.strip_suffix(']')
        && let Some((head, token)) = before.rsplit_once('[')
        && context_number(token.trim_start())
        && head.ends_with(char::is_whitespace)
    {
        name = head.trim_end();
    }
    name.to_owned()
}
pub(crate) fn strip_context_prefix(mut name: &str) -> &str {
    if let Some(rest) = name.strip_prefix('[')
        && let Some((token, tail)) = rest.split_once(']')
        && context_number(token.trim_start())
        && tail.starts_with(char::is_whitespace)
    {
        name = tail.trim_start();
    }
    name
}
fn context_number(token: &str) -> bool {
    if token == "?" {
        return true;
    }
    let number = token
        .strip_suffix('K')
        .or_else(|| token.strip_suffix('M'))
        .unwrap_or(token);
    let mut parts = number.split('.');
    let integer = parts.next().unwrap_or_default();
    if integer.is_empty() || !integer.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if let Some(fraction) = parts.next()
        && (fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    parts.next().is_none()
}
pub(crate) fn usable_context(model: &Value) -> i64 {
    let context = integer(model.get("context_window"));
    if context <= 0 {
        return 0;
    }
    let percentage = model
        .get("effective_context_window_percent")
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .filter(|value| *value != 0.0)
        .unwrap_or(100.0);
    if percentage > 0.0 && percentage <= 100.0 {
        ((context as f64 * percentage / 100.0).round_ties_even() as i64).max(1)
    } else {
        context
    }
}
fn compact_context(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        let value = format!("{:.2}", tokens as f64 / 1_000_000.0);
        format!("{}M", value.trim_end_matches('0').trim_end_matches('.'))
    } else if tokens >= 1_000 {
        format!("{}K", (tokens as f64 / 1000.0).round_ties_even() as i64)
    } else {
        tokens.to_string()
    }
}
fn integer(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(v)) => v
            .as_i64()
            .unwrap_or_else(|| v.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(v)) => v.trim().parse().unwrap_or(0),
        Some(Value::Bool(v)) => i64::from(*v),
        _ => 0,
    }
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
fn array(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn strings(value: Option<&Value>) -> BTreeSet<String> {
    array(value)
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
