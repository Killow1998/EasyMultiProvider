//! Resolve friendly account labels across route categories without changing usage.
use super::*;

pub(super) fn enrich(state: &ServerState, config: &Value, payload: &mut Value) {
    let mut labels = BTreeMap::new();
    for record in state
        .backend
        .usage
        .ledger
        .account_labels()
        .unwrap_or_default()
    {
        labels.insert(
            (
                record["category"].as_str().unwrap_or("").to_owned(),
                record["owner"].as_str().unwrap_or("").to_owned(),
            ),
            record,
        );
    }
    for provider in config["providers"].as_array().into_iter().flatten() {
        let id = provider["id"].as_str().unwrap_or("");
        labels.insert(
            ("external".into(), id.into()),
            json!({"name":provider["name"].as_str().filter(|s|!s.is_empty()).unwrap_or(id),"local_id":id}),
        );
    }
    for account in config["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        if let Some(headers) = account_catalog_headers(account, &state.backend.configuration.vault)
        {
            let owner = account_owner(&headers);
            if owner.is_empty() {
                continue;
            }
            let id = account["id"].as_str().unwrap_or("");
            let name = account["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(id);
            let email = account
                .get("quota")
                .and_then(|value| value["account_label"].as_str())
                .unwrap_or("");
            labels
                .entry(("subscription".into(), owner))
                .and_modify(|record| {
                    record["name"] = json!(name);
                    record["local_id"] = json!(id);
                    if !email.is_empty() {
                        record["email"] = json!(email);
                    }
                })
                .or_insert_with(|| json!({"name":name,"local_id":id,"email":email}));
        }
    }
    if let Some(headers) = native_subscription_headers(&state.backend.accounts.native_auth_path) {
        let owner = account_owner(&headers);
        if !owner.is_empty() {
            let email = state
                .backend
                .accounts
                .native_quota
                .lock()
                .ok()
                .and_then(|q| {
                    q.as_ref()
                        .and_then(|v| v["account_label"].as_str().map(str::to_owned))
                })
                .unwrap_or_default();
            labels
                .entry(("native".into(), owner))
                .and_modify(|record| {
                    record["name"] = json!("Native");
                    record["local_id"] = json!("@native");
                    if !email.is_empty() {
                        record["email"] = json!(email);
                    }
                })
                .or_insert_with(|| json!({"name":"Native","local_id":"@native","email":email}));
        }
    }
    for field in ["groups", "series"] {
        for row in payload[field].as_array_mut().into_iter().flatten() {
            let category = row["category"].as_str().unwrap_or("").to_owned();
            let owner = row["owner"].as_str().unwrap_or("").to_owned();
            let record = labels.get(&(category.clone(), owner.clone())).or_else(|| {
                if owner.starts_with("account:") {
                    labels
                        .iter()
                        .find(|((_, id), _)| id == &owner)
                        .map(|(_, v)| v)
                } else {
                    None
                }
            });
            row["owner_name"] = record
                .map(|record| record["name"].clone())
                .unwrap_or(json!(""));
            row["owner_email"] = record
                .map(|record| record["email"].clone())
                .unwrap_or(json!(""));
            row["owner_local_id"] = record
                .map(|record| record["local_id"].clone())
                .unwrap_or(json!(""));
            if matches!(category.as_str(), "native" | "subscription") {
                row["account_identity_confirmed"] = json!(owner.starts_with("account:"));
            }
        }
    }
}
