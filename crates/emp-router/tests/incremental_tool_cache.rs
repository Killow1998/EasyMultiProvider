//! Loaded tool schemas must not invalidate a supported endpoint's prefix.
use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_router::project_external_payload;
use serde_json::{Value, json};

fn route(base: &str, protocol: Protocol) -> ResolvedRoute {
    ResolvedRoute::new(
        "fixture/model",
        "model",
        RouteSource::ExplicitModel,
        json!({"id":"fixture", "base_url":base, "api_key":"fixture-only"})
            .as_object()
            .unwrap()
            .clone(),
        json!({"id":"fixture/model", "upstream_id":"model"})
            .as_object()
            .unwrap()
            .clone(),
        protocol,
        match protocol {
            Protocol::Responses => Dialect::PortableResponses,
            Protocol::ChatCompletions => Dialect::ChatCompletions,
            Protocol::AnthropicMessages => Dialect::AnthropicMessages,
            Protocol::Auto => unreachable!(),
        },
        "fixture",
        format!("sha256:{}", "1".repeat(64)),
        "default",
    )
    .expect("valid fixture route")
}

fn search_round(body: &mut Value, namespace: &str, index: usize) {
    let call = format!("search-{index}");
    body["input"].as_array_mut().unwrap().extend([
        json!({"type":"tool_search_call", "execution":"client", "call_id":call, "arguments":{"query":namespace}}),
        json!({"type":"tool_search_output", "execution":"client", "call_id":call, "status":"completed", "tools":[
            {"type":"namespace", "name":namespace, "tools":[{"type":"function", "name":"read", "parameters":{"type":"object"}}]}
        ]}),
        json!({"type":"function_call", "namespace":namespace, "name":"read", "call_id":format!("read-{index}"), "arguments":"{}"}),
        json!({"type":"function_call_output", "call_id":format!("read-{index}"), "output":"fixture-result"}),
        json!({"type":"message", "role":"user", "content":"next"}),
    ]);
}

#[test]
fn three_rounds_keep_official_tool_prefix_and_leave_other_protocols_compatible() {
    for (base, protocol, in_place) in [
        ("https://api.openai.com/v1", Protocol::Responses, true),
        (
            "https://api.openai.com/v1/responses",
            Protocol::Responses,
            true,
        ),
        ("https://fixture.invalid/v1", Protocol::Responses, false),
        (
            "https://api.openai.com/v1",
            Protocol::ChatCompletions,
            false,
        ),
        (
            "https://fixture.invalid",
            Protocol::AnthropicMessages,
            false,
        ),
    ] {
        let route = route(base, protocol);
        let mut body = json!({"model":"fixture/model", "instructions":"Fixed agent instructions",
            "tools":[{"type":"tool_search", "execution":"client", "parameters":{"type":"object"}}],
            "input":[{"type":"message", "role":"user", "content":"start"}]});
        let first = project_external_payload(&route, &body).unwrap();
        let mut previous = first.clone();
        for round in 0..3 {
            if round > 0 {
                search_round(&mut body, if round == 1 { "files" } else { "docs" }, round);
            }
            let original = body.clone();
            let projected = project_external_payload(&route, &body).unwrap();
            assert_eq!(body, original, "projection cannot change saved history");
            if in_place {
                assert_eq!(
                    projected["tools"], first["tools"],
                    "loaded tools cannot move into the prefix"
                );
                assert_eq!(projected["instructions"], first["instructions"]);
                let input = projected["input"].as_array().unwrap();
                let earlier = previous["input"].as_array().unwrap();
                assert_eq!(&input[..earlier.len()], earlier);
                let loaded = input
                    .iter()
                    .filter(|item| item["type"] == "additional_tools")
                    .collect::<Vec<_>>();
                assert_eq!(loaded.len(), round);
                for item in &loaded {
                    assert_eq!(item["role"], "developer");
                    assert!(
                        item["tools"][0]["name"]
                            .as_str()
                            .unwrap()
                            .starts_with("emp_tool_")
                    );
                    let name = &item["tools"][0]["name"];
                    assert!(
                        input
                            .iter()
                            .any(|call| call["type"] == "function_call" && call["name"] == *name)
                    );
                }
                if loaded.len() == 2 {
                    assert_ne!(loaded[0]["tools"][0]["name"], loaded[1]["tools"][0]["name"]);
                }
            } else {
                assert_eq!(projected["tools"].as_array().unwrap().len(), round + 1);
                assert!(!projected.to_string().contains("\"additional_tools\""));
            }
            previous = projected;
        }
    }
}

#[test]
fn input_catalog_loading_preserves_position_only_on_supported_endpoints() {
    let body = json!({"model":"fixture/model", "input":[
        {"type":"message", "role":"user", "content":"before"},
        {"type":"additional_tools", "role":"developer", "tools":[
            {"type":"function", "name":"lookup", "parameters":{"type":"object"}}
        ]},
        {"type":"message", "role":"user", "content":"after"}
    ]});
    let projected = project_external_payload(
        &route("https://api.openai.com/v1", Protocol::Responses),
        &body,
    )
    .unwrap();
    assert_eq!(projected["input"][1]["type"], "additional_tools");
    assert_eq!(projected["input"][1]["tools"][0]["name"], "lookup");
    assert_eq!(projected["tools"].as_array().map(Vec::len).unwrap_or(0), 0);
    for base in [
        "https://api.openai.com.example/v1",
        "http://api.openai.com/v1",
        "https://fixture.invalid/v1",
        "https://api.openai.com:8443/v1",
    ] {
        let projected = project_external_payload(&route(base, Protocol::Responses), &body).unwrap();
        assert_eq!(projected["tools"][0]["name"], "lookup");
        assert_eq!(projected["input"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn in_place_loading_keeps_allowed_tool_restrictions_and_conflict_validation() {
    let route = route("https://api.openai.com/v1", Protocol::Responses);
    let mut body = json!({"model":"fixture/model", "tools":[], "input":[
        {"type":"additional_tools", "role":"developer", "tools":[
            {"type":"function", "name":"allowed", "parameters":{"type":"object"}},
            {"type":"function", "name":"blocked", "parameters":{"type":"object"}}
        ]}
    ], "tool_choice":{"type":"allowed_tools", "mode":"required", "tools":[{"type":"function", "name":"allowed"}]}});
    let projected = project_external_payload(&route, &body).unwrap();
    assert_eq!(projected["input"][0]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(projected["input"][0]["tools"][0]["name"], "allowed");
    assert_eq!(projected["tool_choice"], "required");
    body["tools"] = json!([{"type":"function", "name":"allowed", "parameters":{"type":"object", "properties":{"different":{"type":"string"}}}}]);
    assert!(project_external_payload(&route, &body).is_err());
}
