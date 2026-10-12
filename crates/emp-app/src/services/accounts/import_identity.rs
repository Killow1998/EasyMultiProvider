//! Name-only browser imports resolve authenticated identity before local references.
use super::*;

pub(super) fn resolve(state: &ServerState, body: &Value) -> Result<Value, String> {
    if body.get("id").is_some() {
        return Ok(body.clone());
    }
    let auth = body.get("auth_json").ok_or("auth_json is required")?;
    let auth = validate_auth_json(auth).map_err(|error| error.to_string())?;
    let name = body["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .ok_or("name is required")?;
    let config = state
        .backend
        .configuration
        .read()
        .map_err(|_| "internal server error")?;
    let accounts = config["accounts"].as_array().cloned().unwrap_or_default();
    let existing = accounts.iter().find(|account| {
        account["auth_file"]
            .as_str()
            .and_then(|path| {
                state
                    .backend
                    .configuration
                    .vault
                    .read_encrypted_json(Path::new(path))
                    .ok()
            })
            .is_some_and(|old| emp_state::same_account_auth(&old, &auth))
    });
    let mut resolved = existing.cloned().unwrap_or_else(|| serde_json::json!({}));
    if existing.is_none() {
        let reserved = accounts
            .iter()
            .chain(config["providers"].as_array().into_iter().flatten());
        let used = reserved
            .flat_map(|record| [record["id"].as_str(), record["prefix"].as_str()])
            .flatten()
            .collect::<std::collections::BTreeSet<_>>();
        let id = loop {
            let candidate = format!(
                "account-{}",
                crate::util::random_hex(4).map_err(|_| "unable to allocate account reference")?
            );
            if !used.contains(candidate.as_str()) {
                break candidate;
            }
        };
        resolved["id"] = Value::String(id.clone());
        resolved["prefix"] = Value::String(id);
    }
    resolved["name"] = Value::String(name.trim().to_owned());
    resolved["auth_json"] = auth;
    Ok(resolved)
}
