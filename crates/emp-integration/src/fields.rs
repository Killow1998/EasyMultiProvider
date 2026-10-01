//! Managed TOML fields and lease comparison; no filesystem or runtime access.
use crate::{FieldState, IntegrationError, LeaseRecord, MANAGED_FIELDS};
use std::collections::BTreeMap;

pub(super) fn states(document: &str) -> Result<BTreeMap<String, FieldState>, IntegrationError> {
    let parsed = document
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| IntegrationError("unable to parse Codex TOML config"))?;
    MANAGED_FIELDS
        .into_iter()
        .map(|name| {
            let state = match parsed.get(name) {
                None => FieldState::absent(),
                Some(value) => FieldState::value(
                    value
                        .as_str()
                        .ok_or(IntegrationError("managed TOML field is not a string"))?,
                ),
            };
            Ok((name.to_owned(), state))
        })
        .collect()
}

pub(super) fn set_states(
    document: String,
    desired: &BTreeMap<String, FieldState>,
) -> Result<String, IntegrationError> {
    // Root comments are independent of the first key/table in tomlkit. Keep
    // them outside the edit tree so removing a managed key cannot remove them.
    let header_len: usize = document
        .split_inclusive('\n')
        .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
        .map(str::len)
        .sum();
    let (header, body) = document.split_at(header_len);
    let mut parsed = body
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| IntegrationError("unable to parse Codex TOML config"))?;
    let mut inserted = false;
    for name in MANAGED_FIELDS {
        let state = &desired[name];
        if !state.present {
            parsed.as_table_mut().remove(name);
            continue;
        }
        let mut replacement = toml_edit::Value::from(state.value.as_deref().unwrap_or_default());
        if let Some(current) = parsed.get(name).and_then(toml_edit::Item::as_value) {
            *replacement.decor_mut() = current.decor().clone();
        } else {
            inserted = true;
        }
        parsed[name] = toml_edit::Item::Value(replacement);
    }
    if inserted {
        // Python's tomlkit retains this separator when managed fields are removed.
        for (_, item) in parsed.iter_mut() {
            let table = match item {
                toml_edit::Item::Table(table) => Some(table),
                toml_edit::Item::ArrayOfTables(tables) => tables.iter_mut().next(),
                _ => None,
            };
            if let Some(table) = table {
                let prefix = table
                    .decor()
                    .prefix()
                    .and_then(|raw| raw.as_str())
                    .unwrap_or("");
                if !prefix.starts_with(['\r', '\n']) {
                    let prefix = format!("\n{prefix}");
                    table.decor_mut().set_prefix(prefix);
                }
                break;
            }
        }
    }
    let rendered = parsed.to_string();
    let newline = if !header.is_empty() && !header.ends_with('\n') && !rendered.is_empty() {
        "\n"
    } else {
        ""
    };
    Ok(format!("{header}{newline}{rendered}"))
}

pub(super) fn relation(current: &BTreeMap<String, FieldState>, lease: &LeaseRecord) -> String {
    let original = lease
        .fields
        .iter()
        .map(|(name, recovery)| (name.clone(), recovery.original.clone()))
        .collect::<BTreeMap<_, _>>();
    let applied = lease
        .fields
        .iter()
        .map(|(name, recovery)| (name.clone(), recovery.applied.clone()))
        .collect::<BTreeMap<_, _>>();
    if current == &original {
        "original"
    } else if current == &applied {
        "applied"
    } else if MANAGED_FIELDS
        .iter()
        .all(|name| current[*name] == original[*name] || current[*name] == applied[*name])
    {
        "mixed"
    } else {
        "other"
    }
    .to_owned()
}

pub(super) fn conflict_names(
    current: &BTreeMap<String, FieldState>,
    lease: &LeaseRecord,
) -> Vec<String> {
    let mut values = MANAGED_FIELDS
        .iter()
        .filter(|name| {
            current[**name] != lease.fields[**name].original
                && current[**name] != lease.fields[**name].applied
        })
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    if values.is_empty() && relation(current, lease) == "mixed" {
        values.push("mixed_state".to_owned());
    }
    values
}

pub(super) fn validate_value(value: &str) -> Result<(), IntegrationError> {
    if value.contains(['\n', '\r']) {
        Err(IntegrationError(
            "managed value must be a single-line string",
        ))
    } else {
        Ok(())
    }
}
