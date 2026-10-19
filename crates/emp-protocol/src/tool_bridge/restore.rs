//! Restores request-owned tool identities on complete and streamed responses.
use super::ExternalTools;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

impl ExternalTools {
    fn restore_item(&self, item: &mut Value, partial: bool) -> Result<(), &'static str> {
        if !matches!(
            item["type"].as_str(),
            Some("function_call" | "custom_tool_call")
        ) {
            return Ok(());
        }
        let Some(identity) = item["name"]
            .as_str()
            .and_then(|name| self.identities.get(name))
        else {
            return Ok(());
        };
        if !identity.search {
            item["name"] = json!(identity.name);
            if let Some(namespace) = &identity.namespace {
                item["namespace"] = json!(namespace);
            }
            if item["type"] == "function_call"
                && identity.namespace.as_deref() == Some("collaboration")
                && matches!(
                    identity.name.as_str(),
                    "spawn_agent" | "send_message" | "followup_task"
                )
            {
                // Codex distinguishes readable agent messages using an explicit
                // empty list; omitting it makes even plaintext look encrypted.
                if item.get("encrypted_function_args").is_some_and(|value| {
                    !value.is_null() && !value.as_array().is_some_and(Vec::is_empty)
                }) {
                    return Err("external collaboration returned unexpected encrypted arguments");
                }
                item["encrypted_function_args"] = json!([]);
            }
            return Ok(());
        }
        let arguments = if partial {
            json!({})
        } else {
            item["arguments"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .filter(Value::is_object)
                .ok_or("external tool search returned invalid arguments")?
        };
        item["type"] = json!("tool_search_call");
        item["execution"] = json!("client");
        item["arguments"] = arguments;
        item.as_object_mut().unwrap().remove("name");
        if let Some(id) = item["id"].as_str() {
            item["id"] = json!(format!("tsc_{:x}", Sha256::digest(id.as_bytes()))[..36]);
        }
        Ok(())
    }

    pub fn restore_response(&self, mut response: Value) -> Result<Value, &'static str> {
        if let Some(items) = response.get_mut("output").and_then(Value::as_array_mut) {
            for item in items {
                self.restore_item(item, false)?;
            }
        }
        Ok(response)
    }

    pub fn restore_event(&mut self, mut event: Value) -> Result<Option<Value>, &'static str> {
        let kind = event["type"].as_str().unwrap_or("").to_owned();
        if let Some(item) = event.get("item")
            && self
                .search_name
                .as_deref()
                .is_some_and(|name| item["name"] == name)
        {
            if let Some(index) = event.get("output_index").filter(|v| !v.is_null()) {
                self.search_indices.insert(index.to_string());
            }
            if let Some(id) = item["id"].as_str().filter(|s| !s.is_empty()) {
                self.search_ids.insert(id.to_owned());
            }
        }
        if let Some(item) = event.get_mut("item") {
            self.restore_item(item, kind == "response.output_item.added")?;
        }
        if kind.starts_with("response.function_call_arguments.")
            && (event
                .get("output_index")
                .is_some_and(|v| self.search_indices.contains(&v.to_string()))
                || event["item_id"]
                    .as_str()
                    .is_some_and(|id| self.search_ids.contains(id)))
        {
            return Ok(None);
        }
        if let Some(response) = event.get_mut("response").filter(|v| v.is_object()) {
            *response = self.restore_response(response.take())?;
        }
        Ok(Some(event))
    }
}
