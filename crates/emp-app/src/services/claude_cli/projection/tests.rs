use super::response::cli_usage;
use super::*;
use crate::services::claude_cli::ClaudeCliError;
use emp_core::ResolvedRoute;

#[test]
fn removes_only_responses_reasoning_items_and_preserves_user_fields() {
    let source = json!({
        "reasoning":{"effort":"high","encrypted_content":"ordinary setting"},
        "input":[
            {"type":"reasoning","encrypted_content":"private state"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"keep"}]},
            {"type":"function_call","call_id":"call-1","name":"read","arguments":"{\"reasoning\":\"argument\",\"encrypted_content\":\"argument value\",\"type\":\"thinking\"}"},
            {"type":"function_call_output","call_id":"call-1","output":{"reasoning":"result field","encrypted_content":"result value","content":[{"type":"thinking","text":"ordinary result"}]}}
        ],
        "tools":[{"type":"function","name":"schema","parameters":{"properties":{"reasoning":{"type":"string"},"encrypted_content":{"type":"string"}}}}]
    });
    let encoded = transcript(&source).expect("transcript");
    let value: Value = serde_json::from_slice(&encoded).expect("JSON");
    assert_eq!(value["reasoning"]["effort"], "high");
    assert_eq!(value["reasoning"]["encrypted_content"], "ordinary setting");
    assert_eq!(value["input"].as_array().unwrap().len(), 3);
    assert_eq!(value["input"][2]["output"]["reasoning"], "result field");
    assert_eq!(
        value["input"][2]["output"]["encrypted_content"],
        "result value"
    );
    assert_eq!(
        value["input"][2]["output"]["content"][0]["type"],
        "thinking"
    );
    assert_eq!(
        value["input"][1]["arguments"],
        source["input"][2]["arguments"]
    );
    assert_eq!(
        value["tools"][0]["parameters"]["properties"]["reasoning"]["type"],
        "string"
    );
    assert!(!value.to_string().contains("private state"));
}

#[test]
fn rejects_typed_multimodal_input_but_keeps_schema_and_tool_result_json() {
    let unsupported = json!({
        "input":[{"type":"message","role":"user","content":[
            {"type":"input_text","text":"hello"},
            {"type":"input_image","image_url":"data:image/png;base64,not-a-real-image"}
        ]}],
        "tools":[{"type":"function","name":"file","parameters":{"properties":{"image":{"type":"string"},"audio":{"type":"string"}}}}]
    });
    assert_eq!(transcript(&unsupported), Err("unsupported_input_modality"));

    let text_only = json!({
        "input":[
            {"type":"function_call_output","call_id":"call-1","output":{"type":"input_image","ordinary_result":true}},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"still text"}]}
        ],
        "tools":[{"type":"function","name":"schema","parameters":{"properties":{"input_file":{"type":"string"}}}}]
    });
    let encoded = transcript(&text_only).expect("ordinary JSON is preserved");
    let projected: Value = serde_json::from_slice(&encoded).expect("JSON transcript");
    assert_eq!(projected["input"][0]["output"]["type"], "input_image");
    assert_eq!(
        projected["tools"][0]["parameters"]["properties"]["input_file"]["type"],
        "string"
    );

    let role_only_message = json!({
        "input":[{"role":"user","content":[
            {"type":"input_image","image_url":"data:image/png;base64,not-a-real-image"}
        ]}]
    });
    assert_eq!(
        transcript(&role_only_message),
        Err("unsupported_input_modality")
    );

    let tool_output_content = json!({
        "input":[{"type":"function_call_output","call_id":"call-1","output":[
            {"type":"input_file","file_id":"file_fake"}
        ]}]
    });
    assert_eq!(
        transcript(&tool_output_content),
        Err("unsupported_input_modality")
    );
}

#[test]
fn cli_usage_reuses_anthropic_cache_projection_and_keeps_unknowns_unknown() {
    let route = test_route();
    let request = json!({"model":"m","input":"hello","tools":[]});
    let cli_output = json!({
        "structured_output":{"answer":"answer","tool_calls":[]},
        "usage":{
            "input_tokens":12,
            "cache_read_input_tokens":7000,
            "cache_creation_input_tokens":3000,
            "cache_creation":{"ephemeral_1h_input_tokens":2500},
            "output_tokens":3
        }
    });
    let response =
        response_from_cli(&cli_output, &route, &request, 200).expect("project cached usage");
    assert_eq!(
        response.body["usage"],
        json!({
            "input_tokens":10012,
            "input_tokens_details":{
                "cached_tokens":7000,
                "cache_creation_tokens":3000,
                "cache_creation_1h_tokens":2500
            },
            "output_tokens":3,
            "total_tokens":10015
        })
    );

    let missing_usage = json!({
        "structured_output":{"answer":"answer","tool_calls":[]}
    });
    let response = response_from_cli(&missing_usage, &route, &request, 200)
        .expect("project response without usage");
    assert!(response.body["usage"].is_null());

    assert_eq!(cli_usage(&json!({"usage":{}})), json!({}));
}

#[test]
fn proposal_is_projected_through_external_tool_identity_bridge() {
    let route = test_route();
    let request = json!({"model":"m","input":[{"type":"message","role":"user","content":"go"}],"tools":[{"type":"namespace","name":"fs","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}]});
    let cli_output = json!({
        "structured_output":{
            "answer":"",
            "tool_calls":[{"type":"function_call","name":"read","namespace":"fs","arguments":{"path":"/tmp/a"},"input":""}]
        },
        "usage":{"input_tokens":4,"output_tokens":3}
    });
    let response = response_from_cli(&cli_output, &route, &request, 200).expect("projection");
    assert_eq!(response.body["output"][0]["type"], "function_call");
    assert_eq!(response.body["output"][0]["namespace"], "fs");
    assert_eq!(response.body["output"][0]["name"], "read");
    assert_eq!(
        response.body["output"][0]["arguments"],
        "{\"path\":\"/tmp/a\"}"
    );
}

#[test]
fn structured_output_rejects_extra_fields_and_tools_disallowed_by_choice() {
    let route = test_route();
    let request = json!({
        "model":"m",
        "input":"go",
        "tools":[{"type":"function","name":"read","parameters":{"type":"object"}}],
        "tool_choice":"none"
    });
    let cli_output = json!({
        "structured_output":{
            "answer":"",
            "tool_calls":[{"type":"function_call","name":"read","namespace":"","arguments":{"path":"/tmp/a"},"input":""}]
        }
    });
    assert!(matches!(
        response_from_cli(&cli_output, &route, &request, 200),
        Err(ClaudeCliError::Failure("claude_cli_unknown_tool_proposal"))
    ));

    let invalid = json!({
        "structured_output":{"answer":"ok","tool_calls":[],"unexpected":true}
    });
    assert!(matches!(
        response_from_cli(&invalid, &route, &request, 200),
        Err(ClaudeCliError::Failure(
            "claude_cli_invalid_structured_output"
        ))
    ));
}

#[test]
fn discovered_tools_enforce_choice_namespace_and_kind_on_return() {
    for source in ["additional_tools", "tool_search_output"] {
        for declared in ["function", "custom"] {
            let mut request = json!({
                "tools":[{"type":"function","name":"other","parameters":{"type":"object"}}],
                "input":[{"type":source,"execution":"client","call_id":"search-1","tools":[
                    {"type":"namespace","name":"fs","tools":[
                        {"type":declared,"name":"read","parameters":{"type":"object"}}
                    ]}
                ]}]
            });
            for (selected, accepted) in [
                ("function", "function_call"),
                ("custom", "custom_tool_call"),
            ] {
                let choice = json!({"type":selected,"name":"read","namespace":"fs"});
                for selection in [
                    choice.clone(),
                    json!({"type":"allowed_tools","tools":[choice]}),
                ] {
                    request["tool_choice"] = selection;
                    for (name, namespace, kind) in [
                        ("read", "fs", "function_call"),
                        ("read", "fs", "custom_tool_call"),
                        ("read", "wrong", accepted),
                        ("other", "", "function_call"),
                    ] {
                        let output = json!({"structured_output":{"answer":"","tool_calls":[{
                            "type":kind,"name":name,"namespace":namespace,"arguments":{},"input":"patch"
                        }]}});
                        let result = response_from_cli(&output, &test_route(), &request, 200);
                        if name == "read"
                            && namespace == "fs"
                            && kind == accepted
                            && selected == declared
                        {
                            let response =
                                result.expect("selected discovered tool remains available");
                            assert_eq!(response.body["output"][0]["type"], accepted);
                            assert_eq!(response.body["output"][0]["name"], "read");
                            assert_eq!(response.body["output"][0]["namespace"], "fs");
                        } else {
                            assert!(matches!(
                                result,
                                Err(ClaudeCliError::Failure("claude_cli_unknown_tool_proposal"))
                            ));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn restores_only_unambiguous_available_tool_spellings() {
    let request = json!({"input":"go","tools":[
        {"type":"namespace","name":"functions","tools":[
            {"type":"function","name":"exec_command","parameters":{"type":"object"}}
        ]}
    ]});
    for name in [
        "exec_command",
        "functions.exec_command",
        "functions__exec_command",
    ] {
        let output = json!({"structured_output":{"answer":"","tool_calls":[
            {"type":"function_call","name":name,"namespace":"","arguments":{"cmd":"pwd"},"input":""}
        ]}});
        let response = response_from_cli(&output, &test_route(), &request, 200).unwrap();
        assert_eq!(response.body["output"][0]["namespace"], "functions");
        assert_eq!(response.body["output"][0]["name"], "exec_command");
        assert_eq!(response.body["output"][0]["arguments"], "{\"cmd\":\"pwd\"}");
    }
}

#[test]
fn refuses_ambiguous_unknown_wrong_scope_and_wrong_kind_tools() {
    let request = json!({"input":"go","tools":[
        {"type":"namespace","name":"a","tools":[{"type":"function","name":"read"}]},
        {"type":"namespace","name":"b","tools":[{"type":"function","name":"read"}]}
    ]});
    for (name, namespace, kind) in [
        ("read", "", "function_call"),
        ("missing", "a", "function_call"),
        ("read", "wrong", "function_call"),
        ("read", "a", "custom_tool_call"),
    ] {
        let output = json!({"structured_output":{"answer":"","tool_calls":[
            {"type":kind,"name":name,"namespace":namespace,"arguments":{},"input":""}
        ]}});
        assert!(matches!(
            response_from_cli(&output, &test_route(), &request, 200),
            Err(ClaudeCliError::Failure("claude_cli_unknown_tool_proposal"))
        ));
    }
}

#[test]
fn unknown_tool_error_is_not_a_retryable_gateway_failure() {
    assert_eq!(
        crate::services::claude_cli::error::failure_details("claude_cli_unknown_tool_proposal").0,
        422
    );
}

#[test]
fn nullable_effort_means_no_override_and_explicit_values_keep_their_meaning() {
    for body in [
        json!({}),
        json!({"reasoning":null}),
        json!({"reasoning":{}}),
        json!({"reasoning":{"effort":null}}),
    ] {
        assert_eq!(effort(&body), Ok(None), "optional effort: {body}");
    }
    for value in ["low", "medium", "high", "xhigh", "max"] {
        assert_eq!(
            effort(&json!({"reasoning":{"effort":value}})),
            Ok(Some(value))
        );
    }
    assert_eq!(
        effort(&json!({"reasoning":{"effort":"none"}})),
        Err("unsupported_reasoning_effort_none")
    );
    for value in [
        json!("minimal"),
        json!("disabled"),
        json!("unknown"),
        json!(""),
        json!(0),
        json!(false),
        json!([]),
        json!({}),
    ] {
        assert_eq!(
            effort(&json!({"reasoning":{"effort":value}})),
            Err("unsupported_reasoning_effort")
        );
    }
}

#[test]
fn selected_effort_is_never_silently_downgraded_for_the_model() {
    let old = json!({"supports_reasoning":true,"reasoning_levels":["low","medium","high","max"]});
    let new =
        json!({"supports_reasoning":true,"reasoning_levels":["low","medium","high","xhigh","max"]});
    let old = old.as_object().unwrap();
    let new = new.as_object().unwrap();
    for level in ["low", "medium", "high", "max"] {
        assert_eq!(
            effort_for_model(&json!({"reasoning":{"effort":level}}), old),
            Ok(Some(level))
        );
    }
    assert_eq!(
        effort_for_model(&json!({"reasoning":{"effort":"xhigh"}}), old),
        Err("unsupported_model_reasoning_effort")
    );
    assert_eq!(
        effort_for_model(&json!({"reasoning":{"effort":"xhigh"}}), new),
        Ok(Some("xhigh"))
    );
    assert_eq!(effort_for_model(&json!({}), new), Ok(None));
    assert_eq!(
        effort_for_model(
            &json!({"reasoning":{"effort":"high"}}),
            json!({"supports_reasoning":false}).as_object().unwrap()
        ),
        Err("unsupported_model_reasoning_effort")
    );
    let restricted = json!({"reasoning_levels":["low"]});
    assert!(
        effort_for_model(
            &json!({"reasoning":{"effort":"high"}}),
            restricted.as_object().unwrap()
        )
        .is_err()
    );
}

#[test]
fn effort_diagnostics_report_only_known_enums_or_types() {
    assert_eq!(effort_diagnostic(&json!({})), "omitted");
    assert_eq!(
        effort_diagnostic(&json!({"reasoning":{"effort":null}})),
        "null"
    );
    for value in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
        assert_eq!(
            effort_diagnostic(&json!({"reasoning":{"effort":value}})),
            value
        );
    }
    for (value, label) in [
        (json!("private arbitrary input"), "unknown_string"),
        (json!(false), "boolean"),
        (json!(0), "number"),
        (json!(["private input"]), "array"),
        (json!({"private":"input"}), "object"),
    ] {
        assert_eq!(
            effort_diagnostic(&json!({"reasoning":{"effort":value}})),
            label
        );
    }
}

#[test]
fn cli_result_and_effort_mapping_reject_unusable_values() {
    let stdout = serde_json::to_vec(&json!({
        "type":"result",
        "structured_output":{"answer":"ok","tool_calls":[]}
    }))
    .expect("CLI result");
    let parsed = parse_cli_result(&stdout).expect("structured CLI result");
    assert_eq!(parsed["structured_output"]["answer"], "ok");
    assert_eq!(
        parse_cli_result(br#"{"type":"result","is_error":true,"structured_output":{}}"#),
        Err("claude_cli_result_error")
    );
    let jsonl = br#"{"type":"system","subtype":"init"}
{"type":"result","subtype":"success","structured_output":{"answer":"ok","tool_calls":[]}}
"#;
    assert_eq!(
        parse_cli_result(jsonl).unwrap()["structured_output"]["answer"],
        "ok"
    );
    let observed = parse_cli_result(br#"{"type":"assistant","message":{"model":"actual-revision"}}
{"type":"result","structured_output":{"answer":"ok","tool_calls":[]},"modelUsage":{"billing-only-name":{}}}
"#).unwrap();
    assert_eq!(
        observed["_emp_model_observation"]["response_model"],
        "actual-revision"
    );
    let billing_only = parse_cli_result(br#"{"type":"result","structured_output":{"answer":"ok","tool_calls":[]},"modelUsage":{"billing-only-name":{}}}"#).unwrap();
    assert!(billing_only["_emp_model_observation"]["response_model"].is_null());
    assert_eq!(
        effort(&json!({"reasoning":{"effort":"low"}})),
        Ok(Some("low"))
    );
    assert_eq!(
        effort(&json!({"reasoning":{"effort":"xhigh"}})),
        Ok(Some("xhigh"))
    );
    assert_eq!(
        effort(&json!({"reasoning":{"effort":"disabled"}})),
        Err("unsupported_reasoning_effort")
    );
}

fn test_route() -> ResolvedRoute {
    let mut provider = serde_json::Map::new();
    provider.insert("id".to_owned(), json!("p"));
    provider.insert("base_url".to_owned(), json!("http://127.0.0.1/v1"));
    provider.insert("auth_mode".to_owned(), json!("api_key"));
    provider.insert("api_key".to_owned(), json!("key"));
    provider.insert("protocol".to_owned(), json!("anthropic_messages"));
    let mut model = serde_json::Map::new();
    model.insert("id".to_owned(), json!("m"));
    model.insert("provider".to_owned(), json!("p"));
    ResolvedRoute::new(
        "m",
        "upstream-m",
        emp_core::RouteSource::ExplicitModel,
        provider,
        model,
        emp_core::Protocol::AnthropicMessages,
        emp_core::Dialect::PortableResponses,
        "p",
        format!("sha256:{}", "a".repeat(64)),
        "deployment",
    )
    .expect("route")
}
