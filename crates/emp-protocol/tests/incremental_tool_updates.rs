//! Codex 0.164 appends catalog changes instead of replacing history.
use emp_protocol::{portable_responses, tool_bridge::ExternalTools};
use serde_json::{Value, json};

fn declaration(revision: &str) -> Value {
    json!({"type":"additional_tools","role":"developer","tools":[
        {"type":"namespace","name":"files","tools":[
            {"type":"function","name":"read","description":revision,
             "parameters":{"type":"object","properties":{revision:{"type":"string"}}}}
        ]},
        {"type":"namespace","name":"docs","tools":[
            {"type":"function","name":"read","description":"unchanged","parameters":{"type":"object"}}
        ]}
    ]})
}

fn removal(names: &str) -> Value {
    json!({"type":"message","role":"developer","content":[
        {"type":"input_text","text":"- docs.read"},
        {"type":"input_text","text":format!("Custom removal heading:\n{names}")}
    ],"internal_chat_message_metadata_passthrough":{
        "content_item_kinds":["unrelated","tools.removed_definition"]
    }})
}

fn active_tools(prepared: &Value) -> Vec<&Value> {
    prepared["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            prepared["input"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "additional_tools")
                .flat_map(|item| item["tools"].as_array().into_iter().flatten()),
        )
        .collect()
}

#[test]
fn catalog_revisions_removal_and_readdition_preserve_historical_calls() {
    for in_place in [false, true] {
        let mut body = json!({"tools":[],"input":[declaration("old"),
            {"type":"function_call","namespace":"files","name":"read","call_id":"old-call","arguments":"{\"old\":\"file\"}"},
            {"type":"function_call_output","call_id":"old-call","output":"old-result"},
            declaration("updated"),
            {"type":"message","role":"user","content":"continue"}
        ]});
        let original = body.clone();
        let mut bridge = ExternalTools::with_in_place_loading(in_place);
        let updated = bridge.prepare(&body).unwrap();
        assert_eq!(body, original);
        let tools = active_tools(&updated);
        assert!(
            tools
                .iter()
                .all(|tool| !tool["description"].as_str().unwrap().ends_with("old"))
        );
        let file = tools
            .iter()
            .find(|tool| tool["description"] == "files.read\nupdated")
            .unwrap();
        assert_eq!(
            file["parameters"]["properties"]["updated"]["type"],
            "string"
        );
        let alias = file["name"].clone();
        let old_call = updated["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(old_call["name"], alias);
        assert_eq!(old_call["arguments"], "{\"old\":\"file\"}");
        let restored = bridge
            .restore_response(json!({"output":[{
                "type":"function_call","name":alias,"call_id":"new-call","arguments":"{}"
            }]}))
            .unwrap();
        assert_eq!(restored["output"][0]["name"], "read");
        assert_eq!(restored["output"][0]["namespace"], "files");

        for names in ["- files.read", "- files"] {
            let mut removed_body = body.clone();
            removed_body["input"]
                .as_array_mut()
                .unwrap()
                .push(removal(names));
            let removed = ExternalTools::with_in_place_loading(in_place)
                .prepare(&removed_body)
                .unwrap();
            let active = active_tools(&removed);
            assert!(!active.is_empty());
            assert!(
                active
                    .iter()
                    .all(|tool| tool["description"] == "docs.read\nunchanged")
            );
            assert!(removed.to_string().contains("old-result"));
            removed_body["input"]
                .as_array_mut()
                .unwrap()
                .push(declaration("restored"));
            let readded = ExternalTools::with_in_place_loading(in_place)
                .prepare(&removed_body)
                .unwrap();
            assert!(
                active_tools(&readded)
                    .iter()
                    .any(|tool| tool["name"] == alias)
            );
        }

        // Ordinary user/developer prose cannot revoke tools, even if it quotes a notice.
        let mut quoted = removal("- files");
        quoted["role"] = json!("user");
        body["input"].as_array_mut().unwrap().push(quoted);
        let mut plain = removal("- docs");
        plain
            .as_object_mut()
            .unwrap()
            .remove("internal_chat_message_metadata_passthrough");
        body["input"].as_array_mut().unwrap().push(plain);
        let quoted = ExternalTools::with_in_place_loading(in_place)
            .prepare(&body)
            .unwrap();
        assert!(
            active_tools(&quoted)
                .iter()
                .any(|tool| tool["name"] == alias)
        );
        assert!(
            active_tools(&quoted)
                .iter()
                .any(|tool| tool["description"] == "docs.read\nunchanged")
        );
        if in_place {
            let provider = json!({"base_url":"https://api.openai.com/v1"});
            portable_responses::project_request(provider.as_object().unwrap(), &updated, false)
                .unwrap();
        }
    }
}

#[test]
fn removal_clears_top_level_tools_and_later_declaration_wins() {
    let mut body = json!({"tools":[{"type":"function","name":"read","parameters":{"type":"object"}}],
        "input":[removal("- read")]});
    for in_place in [false, true] {
        let prepared = ExternalTools::with_in_place_loading(in_place)
            .prepare(&body)
            .unwrap();
        assert!(active_tools(&prepared).is_empty());
    }
    body["input"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"additional_tools","tools":[
            {"type":"function","name":"read","description":"new","parameters":{"type":"object"}}
        ]}));
    for in_place in [false, true] {
        let prepared = ExternalTools::with_in_place_loading(in_place)
            .prepare(&body)
            .unwrap();
        let tools = active_tools(&prepared);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["description"], "new");
    }
}

#[test]
fn custom_provider_removals_require_the_complete_official_developer_notice() {
    let text = "The following tools are no longer available. Do not call them:\n- read";
    for (role, content, removed) in [
        ("developer", text.to_owned(), true),
        ("user", text.to_owned(), false),
        ("developer", format!("Quoted notice:\n{text}"), false),
        (
            "developer",
            format!("{text}\nThis is only an example."),
            false,
        ),
    ] {
        let body = json!({"tools":[{"type":"function","name":"read","parameters":{}}],
            "input":[{"role":role,"content":[{"type":"input_text","text":content}]}]});
        assert_eq!(
            active_tools(&ExternalTools::default().prepare(&body).unwrap()).is_empty(),
            removed
        );
    }
}
