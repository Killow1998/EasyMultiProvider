//! Codex 0.162 Responses Lite catalogs and client Tool Search share one bridge.
use emp_protocol::{
    anthropic_projection::responses_to_anthropic, responses_to_chat, tool_bridge::ExternalTools,
};
use serde_json::{Value, json};

fn namespace(name: &str, description: &str) -> Value {
    json!({"type":"namespace","name":name,"description":description,"tools":[
        {"type":"function","name":"read","description":"Read","parameters":{"type":"object"},"defer_loading":true}
    ]})
}

#[test]
fn incremental_catalogs_search_history_and_namespaces_round_trip_across_external_protocols() {
    let files = namespace("files", "Files");
    let docs = namespace("docs", "Docs");
    let body = json!({"model":"selected","input":[
        {"type":"additional_tools","id":"at_catalog","role":"developer","tools":[files.clone(),
            {"type":"tool_search","execution":"client","parameters":{"type":"object"}}]},
        {"type":"tool_search_call","execution":"client","call_id":"search","arguments":{"query":"read"}},
        {"type":"tool_search_output","execution":"client","call_id":"search","status":"completed","tools":[files.clone(),docs]},
        {"type":"function_call","call_id":"read-file","namespace":"files","name":"read","arguments":"{}"},
        {"type":"function_call_output","call_id":"read-file","output":"fixture-result"}
    ]});
    let original = body.clone();
    let mut bridge = ExternalTools::default();
    let prepared = bridge.prepare(&body).unwrap();
    let tools = prepared["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3, "identical repeated definitions are merged");
    let file = tools[0]["name"].as_str().unwrap();
    let search = tools[1]["name"].as_str().unwrap();
    let doc = tools[2]["name"].as_str().unwrap();
    assert_ne!(
        file, doc,
        "same function in different namespaces stays distinct"
    );
    assert_eq!(prepared["input"][0]["name"], search);
    assert_eq!(prepared["input"][2]["name"], file);
    let result: Value =
        serde_json::from_str(prepared["input"][1]["output"].as_str().unwrap()).unwrap();
    assert_eq!(result["tools"][1]["name"], doc);
    assert!(tools.iter().all(|tool| tool.get("defer_loading").is_none()));
    let chat = responses_to_chat(&prepared, "upstream").unwrap();
    let portable =
        emp_protocol::portable_responses::project_request(&Default::default(), &prepared, false)
            .unwrap();
    assert_eq!(portable["tools"].as_array().unwrap().len(), 3);
    assert_eq!(portable["input"][2]["name"], file);
    let anthropic = responses_to_anthropic(&prepared, "upstream").unwrap();
    assert_eq!(chat["tools"].as_array().unwrap().len(), 3);
    assert_eq!(anthropic["tools"].as_array().unwrap().len(), 3);
    assert_eq!(chat["tools"][0]["function"]["name"], file);
    assert_eq!(anthropic["tools"][0]["name"], file);
    for (index, name, kind, namespace) in [
        (0, search, "tool_search_call", None),
        (1, doc, "function_call", Some("docs")),
    ] {
        let item = json!({"type":"function_call","id":format!("fc_{index}"),"call_id":format!("c_{index}"),"name":name,"arguments":"{}"});
        let restored = bridge.restore_event(json!({"type":"response.output_item.added","output_index":index,"item":item.clone()})).unwrap().unwrap();
        assert_eq!(restored["item"]["type"], kind);
        assert_eq!(restored["item"]["namespace"].as_str(), namespace);
        let delta = bridge.restore_event(json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":format!("fc_{index}"),"delta":"{}"})).unwrap();
        assert_eq!(
            delta.is_none(),
            index == 0,
            "only search argument deltas are suppressed"
        );
        let done = bridge
            .restore_event(
                json!({"type":"response.output_item.done","output_index":index,"item":item}),
            )
            .unwrap()
            .unwrap();
        assert_eq!(done["item"]["type"], kind);
        if index == 0 {
            assert_eq!(done["item"]["arguments"], json!({}));
        }
    }
    assert_eq!(
        body, original,
        "saved history is never rewritten by projection"
    );
    let mut conflicting = body.clone();
    conflicting["input"][2]["tools"][0]["tools"][0]["parameters"] = json!({"type":"string"});
    assert!(ExternalTools::default().prepare(&conflicting).is_err());
}
