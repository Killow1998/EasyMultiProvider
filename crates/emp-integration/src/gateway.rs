//! The EMP Key provider uses Codex's OpenAI actor contract, including image tools.
use crate::{FieldState, IntegrationError};
use toml_edit::{DocumentMut, Item, Table};

pub(super) fn state(document: &DocumentMut) -> Result<FieldState, IntegrationError> {
    let Some(provider) = document
        .get("model_providers")
        .and_then(|providers| providers.get("emp"))
    else {
        return Ok(FieldState::absent());
    };
    if !provider.is_table_like() {
        return Err(IntegrationError("EMP model provider is not a table"));
    }
    let mut encoded = DocumentMut::new();
    encoded["emp"] = provider.clone();
    Ok(FieldState::value(encoded.to_string()))
}

pub(super) fn definition(base_url: &str, key: &str) -> FieldState {
    let mut document = DocumentMut::new();
    let mut provider = Table::new();
    for (name, value) in [
        ("name", "EMP"),
        ("base_url", base_url),
        ("wire_api", "responses"),
        ("experimental_bearer_token", key),
    ] {
        provider[name] = toml_edit::value(value);
    }
    provider["requires_openai_auth"] = toml_edit::value(false);
    provider["supports_websockets"] = toml_edit::value(true);
    provider["supports_standalone_web_search"] = toml_edit::value(true);
    let mut headers = toml_edit::InlineTable::new();
    headers.insert("x-openai-actor-authorization", "emp".into());
    provider["http_headers"] = toml_edit::value(headers);
    document["emp"] = Item::Table(provider);
    FieldState::value(document.to_string())
}

pub(super) fn set(document: &mut DocumentMut, state: &FieldState) -> Result<(), IntegrationError> {
    if !state.present {
        if let Some(providers) = document.get_mut("model_providers") {
            providers
                .as_table_like_mut()
                .ok_or(IntegrationError("model providers is not a table"))?
                .remove("emp");
            if providers
                .as_table()
                .is_some_and(|table| table.is_implicit() && table.is_empty())
            {
                document.remove("model_providers");
            }
        }
        return Ok(());
    }
    let mut encoded = state
        .value
        .as_deref()
        .unwrap_or_default()
        .parse::<DocumentMut>()
        .map_err(|_| IntegrationError("invalid EMP provider lease"))?;
    let mut provider = encoded
        .remove("emp")
        .filter(Item::is_table_like)
        .ok_or(IntegrationError("invalid EMP provider lease"))?;
    let providers = document.entry("model_providers").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    if providers.is_inline_table()
        && let Item::Table(table) = provider
    {
        provider = toml_edit::value(table.into_inline_table());
    }
    providers
        .as_table_like_mut()
        .ok_or(IntegrationError("model providers is not a table"))?
        .insert("emp", provider);
    Ok(())
}
