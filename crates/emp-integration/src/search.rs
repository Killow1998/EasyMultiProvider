//! Compare-and-restore ownership of Codex's optional standalone search fields.
use crate::{IntegrationError, absolute};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item};
const SCHEMA: &str = "easy-multi-provider.search-lease";
const ROOT: &str = "web_search";
const FEATURE: &str = "standalone_web_search";
const DOTTED: &str = "features.standalone_web_search";

pub struct SearchFeatureManager {
    config: PathBuf,
    lease: PathBuf,
}
impl SearchFeatureManager {
    pub fn new(config: PathBuf, lease: PathBuf) -> Self {
        Self { config, lease }
    }
    fn read_config(&self) -> Result<(String, DocumentMut), IntegrationError> {
        reject_link(&self.config)?;
        let raw = match std::fs::read_to_string(&self.config) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return Err(IntegrationError("unable to parse Codex TOML config")),
        };
        let header_len = raw
            .split_inclusive('\n')
            .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
            .map(str::len)
            .sum();
        let (header, body) = raw.split_at(header_len);
        let parsed = body
            .parse()
            .map_err(|_| IntegrationError("unable to parse Codex TOML config"))?;
        Ok((header.to_owned(), parsed))
    }
    fn read_lease(&self) -> Result<Option<Value>, IntegrationError> {
        reject_link(&self.lease)?;
        let raw = match std::fs::read(&self.lease) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(IntegrationError("unable to read search integration lease")),
        };
        let value: Value = serde_json::from_slice(&raw)
            .map_err(|_| IntegrationError("unable to read search integration lease"))?;
        if value["schema"] != SCHEMA || value["version"] != 1 {
            return Err(IntegrationError("unsupported search integration lease"));
        }
        if value["config_path"] != absolute(&self.config)?.to_string_lossy().as_ref() {
            return Err(IntegrationError(
                "search integration lease targets another config",
            ));
        }
        if !matches!(value["status"].as_str(), Some("active" | "restored")) {
            return Err(IntegrationError("invalid search integration lease status"));
        }
        for side in ["original", "applied"] {
            let states = &value[side];
            if !states["features_table_present"].is_boolean() {
                return Err(IntegrationError("invalid search integration lease"));
            }
            for field in [ROOT, DOTTED] {
                let record = &states[field];
                if record.as_object().is_none_or(|object| object.len() != 2)
                    || !record["present"].is_boolean()
                    || !record
                        .as_object()
                        .is_some_and(|object| object.contains_key("value"))
                {
                    return Err(IntegrationError("invalid search integration lease"));
                }
                if (record["present"] == false && !record["value"].is_null())
                    || (record["present"] == true
                        && if field == ROOT {
                            !record["value"].is_string()
                        } else {
                            !record["value"].is_boolean()
                        })
                {
                    return Err(IntegrationError("invalid search integration lease"));
                }
            }
        }
        Ok(Some(value))
    }
    fn write_config(&self, header: &str, parsed: &DocumentMut) -> Result<(), IntegrationError> {
        let body = parsed.to_string();
        let separator = if !header.is_empty() && !header.ends_with('\n') && !body.is_empty() {
            "\n"
        } else {
            ""
        };
        emp_state::filesystem::atomic_write_config(
            &self.config,
            format!("{header}{separator}{body}").as_bytes(),
        )
        .map_err(|_| IntegrationError("unable to write Codex search fields"))
    }
    fn write_lease(&self, lease: &Value) -> Result<(), IntegrationError> {
        let bytes = serde_json::to_vec_pretty(lease)
            .map_err(|_| IntegrationError("unable to write search integration lease"))?;
        emp_state::filesystem::atomic_write_private_state(&self.lease, &bytes)
            .map_err(|_| IntegrationError("unable to write search integration lease"))
    }
    /// The application holds the integration operation lock around this call.
    pub fn apply(&self, enabled: bool) -> Result<(), IntegrationError> {
        if !enabled {
            return self.restore();
        }
        let (header, mut parsed) = self.read_config()?;
        let current = states(&parsed)?;
        let desired = json!({ROOT:field(Some(json!("live"))),DOTTED:field(Some(json!(true))),"features_table_present":current["features_table_present"]});
        if let Some(lease) = self.read_lease()?
            && lease["status"] == "active"
        {
            if current == lease["applied"] {
                return Ok(());
            }
            if current != lease["original"] {
                return Err(IntegrationError("Codex search fields changed outside EMP"));
            }
        }
        apply_states(&mut parsed, &desired)?;
        self.write_config(&header, &parsed)?;
        let (_, verified) = self.read_config()?;
        let applied = states(&verified)?;
        if applied[ROOT] != desired[ROOT] || applied[DOTTED] != desired[DOTTED] {
            return Err(IntegrationError("unable to apply Codex standalone search"));
        }
        self.write_lease(&json!({"schema":SCHEMA,"version":1,"config_path":absolute(&self.config)?.to_string_lossy(),"status":"active","original":current,"applied":applied}))
    }
    pub fn restore(&self) -> Result<(), IntegrationError> {
        let Some(mut lease) = self.read_lease()? else {
            return Ok(());
        };
        if lease["status"] == "restored" {
            return Ok(());
        }
        let (header, mut parsed) = self.read_config()?;
        let current = states(&parsed)?;
        if current != lease["original"] {
            if current != lease["applied"] {
                return Err(IntegrationError("Codex search fields changed outside EMP"));
            }
            apply_states(&mut parsed, &lease["original"])?;
            self.write_config(&header, &parsed)?;
            if states(&self.read_config()?.1)? != lease["original"] {
                return Err(IntegrationError("unable to restore Codex search fields"));
            }
        }
        lease["status"] = json!("restored");
        self.write_lease(&lease)
    }
}
fn reject_link(path: &Path) -> Result<(), IntegrationError> {
    if path.is_symlink() {
        Err(IntegrationError(
            "search integration path must not be a symlink",
        ))
    } else {
        Ok(())
    }
}
fn field(value: Option<Value>) -> Value {
    json!({"present":value.is_some(),"value":value})
}
fn states(parsed: &DocumentMut) -> Result<Value, IntegrationError> {
    let root = parsed
        .get(ROOT)
        .map(|item| {
            item.as_str()
                .map(|value| json!(value))
                .ok_or(IntegrationError("Codex web_search must be a string"))
        })
        .transpose()?;
    let feature = match parsed.get("features") {
        None => None,
        Some(item) => item
            .as_table_like()
            .ok_or(IntegrationError("Codex features must be a table"))?
            .get(FEATURE)
            .map(|item| {
                item.as_bool()
                    .map(|value| json!(value))
                    .ok_or(IntegrationError(
                        "Codex standalone_web_search must be boolean",
                    ))
            })
            .transpose()?,
    };
    Ok(
        json!({ROOT:field(root),DOTTED:field(feature),"features_table_present":parsed.contains_key("features")}),
    )
}
fn replace(item: Option<&Item>, mut value: toml_edit::Value) -> Item {
    if let Some(old) = item.and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    }
    Item::Value(value)
}
fn apply_states(parsed: &mut DocumentMut, states: &Value) -> Result<(), IntegrationError> {
    if states[ROOT]["present"] == true {
        let replacement = replace(
            parsed.get(ROOT),
            toml_edit::Value::from(states[ROOT]["value"].as_str().unwrap_or_default()),
        );
        parsed[ROOT] = replacement;
    } else {
        parsed.as_table_mut().remove(ROOT);
    }
    if states[DOTTED]["present"] == true {
        if !parsed.contains_key("features") {
            parsed["features"] = Item::Table(toml_edit::Table::new());
        }
        let table = parsed
            .get_mut("features")
            .and_then(Item::as_table_like_mut)
            .ok_or(IntegrationError("Codex features must be a table"))?;
        let replacement = replace(
            table.get(FEATURE),
            toml_edit::Value::from(states[DOTTED]["value"].as_bool().unwrap_or(false)),
        );
        table.insert(FEATURE, replacement);
    } else if let Some(table) = parsed.get_mut("features").and_then(Item::as_table_like_mut) {
        table.remove(FEATURE);
    }
    if states["features_table_present"] == false
        && parsed
            .get("features")
            .and_then(Item::as_table_like)
            .is_some_and(|table| table.is_empty())
    {
        parsed.as_table_mut().remove("features");
    }
    Ok(())
}
