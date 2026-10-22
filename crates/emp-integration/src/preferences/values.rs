//! Serialize only the owned settings; neighboring memory permissions stay untouched.
use super::{IntegrationError, KEY, MEMORY_KEYS};
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item};

pub(super) fn feature(document: &DocumentMut) -> Result<Option<&Item>, IntegrationError> {
    let Some(features) = document.get("features") else {
        return Ok(None);
    };
    let table = features
        .as_table_like()
        .ok_or(IntegrationError("Codex features must be a table"))?;
    let setting = table.get(KEY);
    if setting.is_some_and(|item| !item.is_bool() && !item.is_table_like()) {
        return Err(IntegrationError(
            "invalid Codex background compaction settings",
        ));
    }
    Ok(setting)
}

pub(super) fn set_value(
    document: &mut DocumentMut,
    value: Option<Item>,
    memory: bool,
) -> Result<(), IntegrationError> {
    if memory {
        if document
            .get("memories")
            .is_some_and(|item| !item.is_table_like())
        {
            return Err(IntegrationError("Codex memories must be a table"));
        }
        for key in MEMORY_KEYS {
            if let Some(item) = value.as_ref().and_then(|item| item.get(key)) {
                document["memories"][key] = item.clone();
            } else if let Some(table) = document
                .get_mut("memories")
                .and_then(Item::as_table_like_mut)
            {
                table.remove(key);
            }
        }
    } else if let Some(value) = value {
        document["features"][KEY] = value;
    } else if let Some(table) = document
        .get_mut("features")
        .and_then(Item::as_table_like_mut)
    {
        table.remove(KEY);
    }
    Ok(())
}

pub(super) fn encode(document: &DocumentMut, memory: bool) -> Result<Value, IntegrationError> {
    let item = if memory {
        let mut table = toml_edit::Table::new();
        if let Some(memories) = document.get("memories") {
            let memories = memories
                .as_table_like()
                .ok_or(IntegrationError("Codex memories must be a table"))?;
            for key in MEMORY_KEYS {
                if let Some(value) = memories.get(key) {
                    if !value.is_str() {
                        return Err(IntegrationError("Codex memory models must be strings"));
                    }
                    table.insert(key, value.clone());
                }
            }
        }
        if table.is_empty() {
            return Ok(Value::Null);
        }
        Item::Table(table)
    } else {
        let Some(item) = feature(document)? else {
            return Ok(Value::Null);
        };
        item.clone()
    };
    let mut wrapper = DocumentMut::new();
    wrapper["value"] = item;
    Ok(json!(wrapper.to_string()))
}

pub(super) fn decode(value: &Value) -> Result<Option<Item>, IntegrationError> {
    match value {
        Value::Null => Ok(None),
        Value::String(raw) => raw
            .parse::<DocumentMut>()
            .ok()
            .and_then(|mut wrapper| wrapper.remove("value"))
            .filter(|item| item.is_bool() || item.is_table_like())
            .map(Some)
            .ok_or(IntegrationError("invalid Codex preference lease value")),
        _ => Err(IntegrationError("invalid Codex preference lease value")),
    }
}

pub(super) fn same(left: &Value, right: &Value) -> Result<bool, IntegrationError> {
    fn normalize(item: &mut Item) -> Result<(), IntegrationError> {
        let decor = match item {
            Item::Value(value) => value.decor_mut(),
            Item::Table(table) => table.decor_mut(),
            _ => return Err(IntegrationError("invalid Codex preference lease value")),
        };
        // Rendering an inline table can add padding, while comments remain significant.
        if decor
            .prefix()
            .and_then(|value| value.as_str())
            .is_none_or(|value| value.trim().is_empty())
        {
            decor.set_prefix("");
        }
        if decor
            .suffix()
            .and_then(|value| value.as_str())
            .is_none_or(|value| value.trim().is_empty())
        {
            decor.set_suffix("");
        }
        if let Some(table) = item.as_table_like_mut() {
            for (_, value) in table.iter_mut() {
                normalize(value)?;
            }
        }
        Ok(())
    }
    fn canonical(value: &Value) -> Result<Option<String>, IntegrationError> {
        let Some(mut item) = decode(value)? else {
            return Ok(None);
        };
        normalize(&mut item)?;
        let mut document = DocumentMut::new();
        document["value"] = item;
        Ok(Some(document.to_string()))
    }
    Ok(canonical(left)? == canonical(right)?)
}
