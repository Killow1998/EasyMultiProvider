//! Request-owned identities for external protocols without Codex tool namespaces.
use crate::collaboration::python_json;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq)]
struct Identity {
    namespace: Option<String>,
    name: String,
    search: bool,
}

#[derive(Debug, Default)]
pub struct ExternalTools {
    identities: BTreeMap<String, Identity>,
    search_name: Option<String>,
    search_indices: BTreeSet<String>,
    search_ids: BTreeSet<String>,
}

impl ExternalTools {
    fn has_request_state(&self) -> bool {
        !self.identities.is_empty()
            || self.search_name.is_some()
            || !self.search_indices.is_empty()
            || !self.search_ids.is_empty()
    }

    fn input_requires_preparation(input: &Value) -> bool {
        const TOOL_BRIDGE_ITEMS: &[&str] = &[
            "additional_tools",
            "tool_search_call",
            "tool_search_output",
            "function_call",
            "custom_tool_call",
            "function_call_output",
            "custom_tool_call_output",
        ];
        matches!(
            input.get("type").and_then(Value::as_str),
            Some(kind) if TOOL_BRIDGE_ITEMS.contains(&kind)
        )
    }

    fn body_requires_preparation(body: &Value) -> bool {
        match body.get("tools") {
            None => {}
            Some(Value::Array(tools)) if tools.is_empty() => {}
            Some(_) => return true,
        }
        if body.get("tool_choice").is_some() {
            return true;
        }
        match body.get("input") {
            Some(Value::Object(_)) => true,
            Some(Value::Array(items)) => items.iter().any(Self::input_requires_preparation),
            _ => false,
        }
    }

    /// Borrow ordinary request bodies that require no tool namespace bridge.
    /// Tool-bearing requests retain the existing owned projection and identity
    /// bookkeeping used by response restoration.
    pub fn prepare_or_borrow<'a>(
        &mut self,
        body: &'a Value,
    ) -> Result<Cow<'a, Value>, &'static str> {
        if !self.has_request_state() && !Self::body_requires_preparation(body) {
            return Ok(Cow::Borrowed(body));
        }
        self.prepare(body).map(Cow::Owned)
    }

    fn name(
        &mut self,
        name: &Value,
        namespace: Option<&Value>,
        search: bool,
    ) -> Result<String, &'static str> {
        let name = name
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("request projection failed: invalid tool name")?;
        let namespace = match namespace {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => (!s.is_empty()).then_some(s.as_str()),
            _ => return Err("request projection failed: invalid tool namespace"),
        };
        let identity = Identity {
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
            search,
        };
        let alias = if namespace.is_some() || search {
            let digest = format!(
                "{:x}",
                Sha256::digest(python_json(&json!([namespace, name, search])).as_bytes())
            );
            let label = format!(
                "{}{name}",
                namespace.map(|s| format!("{s}_")).unwrap_or_default()
            );
            let label = label
                .chars()
                .take(22)
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>();
            format!("emp_tool_{label}_{}", &digest[..32])
        } else {
            name.to_owned()
        };
        if self
            .identities
            .get(&alias)
            .is_some_and(|previous| previous != &identity)
        {
            return Err("request projection failed: tool name collision");
        }
        self.identities.insert(alias.clone(), identity);
        if search {
            self.search_name = Some(alias.clone());
        }
        Ok(alias)
    }

    fn definitions(
        &mut self,
        tools: &Value,
        namespace: Option<&str>,
        description: &str,
    ) -> Result<Vec<Value>, &'static str> {
        let tools = tools
            .as_array()
            .ok_or("request projection failed: invalid tools")?;
        let mut result = Vec::new();
        for raw in tools {
            let mut tool = raw
                .as_object()
                .cloned()
                .ok_or("request projection failed: invalid tool definition")?;
            match raw["type"].as_str() {
                Some("namespace") => {
                    let name = raw["name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .filter(|_| namespace.is_none())
                        .ok_or("request projection failed: invalid tool namespace")?;
                    result.extend(self.definitions(
                        &raw["tools"],
                        Some(name),
                        raw["description"].as_str().unwrap_or(""),
                    )?);
                    continue;
                }
                Some("tool_search") => {
                    if raw["execution"] != "client" {
                        return Err("external tool search requires client execution");
                    }
                    result.push(json!({"type":"function", "name":self.name(&json!("tool_search"),None,true)?,
                        "description":raw.get("description").cloned().unwrap_or(json!("")),
                        "parameters":raw.get("parameters").cloned().unwrap_or(json!({}))}));
                    continue;
                }
                Some("web_search") => {
                    // Codex executes this server-side tool. An external
                    // provider cannot execute it, so omit its schema.
                    continue;
                }
                Some("function" | "custom") => {
                    let definition = if tool.get("function").is_some_and(Value::is_object) {
                        tool.get_mut("function").unwrap().as_object_mut().unwrap()
                    } else {
                        &mut tool
                    };
                    if let Some(namespace) = namespace {
                        let label = format!(
                            "{namespace}.{}",
                            definition.get("name").and_then(Value::as_str).unwrap_or("")
                        );
                        let own = definition
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let joined = [label.as_str(), description, own]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n");
                        definition.insert("description".into(), json!(joined));
                    }
                    let alias = self.name(
                        definition.get("name").unwrap_or(&Value::Null),
                        namespace.map(|s| json!(s)).as_ref(),
                        false,
                    )?;
                    definition.insert("name".into(), json!(alias));
                    definition.remove("defer_loading");
                    tool.remove("defer_loading");
                }
                _ => {}
            }
            result.push(Value::Object(tool));
        }
        Ok(result)
    }

    pub fn prepare(&mut self, body: &Value) -> Result<Value, &'static str> {
        let mut result = body.clone();
        let mut definitions =
            self.definitions(body.get("tools").unwrap_or(&json!([])), None, "")?;
        let items = match body.get("input") {
            Some(Value::Array(items)) => Some(items.clone()),
            Some(Value::Object(_)) => Some(vec![body["input"].clone()]),
            _ => None,
        };
        if let Some(items) = items {
            let mut converted = Vec::new();
            for mut item in items {
                match item["type"].as_str() {
                    Some("additional_tools" | "tool_search_output") => {
                        let loaded =
                            self.definitions(item.get("tools").unwrap_or(&json!([])), None, "")?;
                        definitions.extend(loaded.clone());
                        if item["type"] == "additional_tools" {
                            continue;
                        }
                        if item["execution"] != "client"
                            || item["call_id"].as_str().is_none_or(str::is_empty)
                        {
                            return Err("request projection failed: invalid tool search output");
                        }
                        item = json!({"type":"function_call_output", "call_id":item["call_id"],
                            "output":format!("{{\"status\": {}, \"tools\": {}}}", python_json(&item["status"]), python_json(&json!(loaded)))});
                    }
                    Some("tool_search_call") => {
                        if item["execution"] != "client"
                            || item["call_id"].as_str().is_none_or(str::is_empty)
                            || !item["arguments"].is_object()
                        {
                            return Err("request projection failed: invalid tool search call");
                        }
                        item = json!({"type":"function_call", "call_id":item["call_id"],
                            "name":self.name(&json!("tool_search"),None,true)?, "arguments":python_json(&item["arguments"])});
                    }
                    Some("function_call" | "custom_tool_call" | "function_call_output")
                        if item.get("name").is_some_and(|v| v != "" && !v.is_null()) =>
                    {
                        let namespace = item.as_object_mut().unwrap().remove("namespace");
                        item["name"] =
                            json!(self.name(&item["name"], namespace.as_ref(), false)?);
                    }
                    _ => {}
                }
                converted.push(item);
            }
            result["input"] = json!(converted);
        }
        if body.get("tools").is_some() || !definitions.is_empty() {
            let mut unique: Vec<(String, Value)> = Vec::new();
            for tool in definitions {
                let definition = tool.get("function").unwrap_or(&tool);
                let key = definition
                    .get("name")
                    .filter(|v| !v.is_null())
                    .map(Value::to_string)
                    .unwrap_or_else(|| python_json(&tool));
                if let Some((_, previous)) = unique.iter().find(|(name, _)| name == &key) {
                    if previous != &tool {
                        return Err("request projection failed: conflicting tool definitions");
                    }
                } else {
                    unique.push((key, tool));
                }
            }
            result["tools"] = json!(unique.into_iter().map(|(_, tool)| tool).collect::<Vec<_>>());
        }
        if let Some(mut choice) = result.get("tool_choice").filter(|v| v.is_object()).cloned() {
            if choice["type"] == "allowed_tools" {
                if let Some(choices) = choice["tools"].as_array_mut() {
                    for selected in choices {
                        self.prepare_choice(selected)?;
                    }
                }
                let allowed = choice["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v["name"].as_str())
                    .collect::<BTreeSet<_>>();
                if let Some(tools) = result["tools"].as_array_mut() {
                    tools.retain(|tool| {
                        tool.get("function").unwrap_or(tool)["name"]
                            .as_str()
                            .is_some_and(|name| allowed.contains(name))
                    });
                }
                result["tool_choice"] = choice.get("mode").cloned().unwrap_or(json!("auto"));
            } else {
                self.prepare_choice(&mut choice)?;
                result["tool_choice"] = choice;
            }
        }
        Ok(result)
    }

    fn prepare_choice(&mut self, item: &mut Value) -> Result<(), &'static str> {
        if item["type"] == "tool_search" {
            item["type"] = json!("function");
            item["name"] = json!(self.name(&json!("tool_search"), None, true)?);
        } else if item["name"].as_str().is_some_and(|s| !s.is_empty()) {
            let namespace = item.as_object_mut().unwrap().remove("namespace");
            item["name"] = json!(self.name(&item["name"], namespace.as_ref(), false)?);
        }
        Ok(())
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_owned(body: &Value) {
        let mut tools = ExternalTools::default();
        assert!(matches!(tools.prepare_or_borrow(body), Ok(Cow::Owned(_))));
    }

    #[test]
    fn ordinary_message_array_is_borrowed_and_restoration_is_a_noop() {
        let body = json!({
            "input":[{
                "type":"message", "role":"user",
                "content":[{"type":"input_text","text":"x".repeat(1024 * 1024)}]
            }],
            "tools":[]
        });
        let mut tools = ExternalTools::default();
        let prepared = tools.prepare_or_borrow(&body).expect("ordinary body");
        assert!(matches!(prepared, Cow::Borrowed(value) if std::ptr::eq(value, &body)));

        let response = json!({"output":[{
            "type":"function_call", "name":"plain_tool", "arguments":"{}"
        }]});
        assert_eq!(
            tools
                .restore_response(response.clone())
                .expect("restore response"),
            response
        );
        let event = json!({
            "type":"response.output_item.added", "output_index":0,
            "item":{"type":"function_call","name":"plain_tool","arguments":"{}"}
        });
        assert_eq!(
            tools.restore_event(event.clone()).expect("restore event"),
            Some(event)
        );
        let arguments_event = json!({
            "type":"response.function_call_arguments.delta",
            "output_index":0,"item_id":"fc_plain"
        });
        assert_eq!(
            tools
                .restore_event(arguments_event.clone())
                .expect("restore function call arguments"),
            Some(arguments_event)
        );
    }

    #[test]
    fn object_input_and_tool_metadata_force_owned_preparation() {
        let bodies = [
            json!({"input":{"type":"message","role":"user","content":"hello"}}),
            json!({"tools":[{"type":"function","name":"plain_tool"}]}),
            json!({"tool_choice":"auto"}),
        ];
        for body in &bodies {
            expect_owned(body);
        }

        let mut tools = ExternalTools::default();
        let prepared = tools
            .prepare_or_borrow(&bodies[0])
            .expect("object input preparation");
        assert!(matches!(prepared, Cow::Owned(_)));
        assert_eq!(prepared["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn every_tool_bridge_input_shape_uses_owned_preparation() {
        let bodies = [
            json!({"input":[{"type":"additional_tools","tools":[]}]}),
            json!({"input":[{"type":"tool_search_call","execution":"client","call_id":"c-search","arguments":{}}]}),
            json!({"input":[{"type":"tool_search_output","execution":"client","call_id":"c-output","status":"ok","tools":[]}]}),
            json!({"input":[{"type":"function_call","call_id":"c-function","name":"plain_tool","arguments":"{}"}]}),
            json!({"input":[{"type":"custom_tool_call","call_id":"c-custom","name":"plain_tool","input":"{}"}]}),
            json!({"input":[{"type":"function_call_output","call_id":"c-function","name":"plain_tool","output":"ok"}]}),
            json!({"input":[{"type":"custom_tool_call_output","call_id":"c-custom","output":"ok"}]}),
        ];
        for body in &bodies {
            expect_owned(body);
        }
    }

    #[test]
    fn namespaced_tool_preparation_restores_response_identity() {
        let body = json!({
            "tools":[{"type":"namespace","name":"math","tools":[{
                "type":"function","name":"add","parameters":{"type":"object"}
            }]}],
            "input":[{"type":"function_call","call_id":"call-1","name":"add",
                "namespace":"math","arguments":"{}"}]
        });
        let mut tools = ExternalTools::default();
        let prepared = tools.prepare_or_borrow(&body).expect("namespaced request");
        let Cow::Owned(prepared) = prepared else {
            panic!("namespaced request must be projected");
        };
        let alias = prepared["input"][0]["name"].as_str().unwrap();
        assert_ne!(alias, "add");
        let restored = tools
            .restore_response(json!({"output":[{
                "type":"function_call","call_id":"call-1","name":alias,"arguments":"{}"
            }]}))
            .expect("restore namespaced tool");
        assert_eq!(restored["output"][0]["name"], "add");
        assert_eq!(restored["output"][0]["namespace"], "math");
    }

    #[test]
    fn invalid_tool_definitions_keep_prepare_errors() {
        let mut tools = ExternalTools::default();
        assert_eq!(
            tools.prepare_or_borrow(&json!({"tools":null,"input":"hello"})),
            Err("request projection failed: invalid tools")
        );
    }

    #[test]
    fn codex_server_web_search_does_not_block_external_cli_tools() {
        let body = json!({
            "input":"hello",
            "tools":[
                {"type":"function","name":"exec","parameters":{"type":"object"}},
                {"type":"tool_search","execution":"client","description":"Find tools","parameters":{"type":"object"}},
                {"type":"web_search","external_web_access":true}
            ]
        });
        let mut tools = ExternalTools::default();
        let prepared = tools.prepare(&body).expect("Codex CLI tools");
        assert_eq!(prepared["tools"].as_array().unwrap().len(), 2);
        assert_eq!(prepared["tools"][0]["name"], "exec");
        assert_eq!(prepared["tools"][1]["type"], "function");
        assert_ne!(prepared["tools"][1]["name"], "tool_search");
        assert_eq!(body["tools"].as_array().unwrap().len(), 3);
    }
}
