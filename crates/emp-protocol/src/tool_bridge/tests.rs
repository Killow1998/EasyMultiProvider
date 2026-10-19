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
    assert!(
        restored["output"][0]
            .get("encrypted_function_args")
            .is_none()
    );
}

#[test]
fn external_collaboration_calls_are_marked_as_plaintext_for_codex() {
    for name in ["spawn_agent", "send_message", "followup_task"] {
        let mut tools = ExternalTools::default();
        let prepared = tools
            .prepare(&json!({"tools":[{
            "type":"namespace","name":"collaboration","tools":[{
                "type":"function","name":name,"parameters":{"type":"object"}
            }]
        }],"input":"delegate"}))
            .unwrap();
        let call = json!({"type":"function_call","call_id":"call-agent",
            "name":prepared["tools"][0]["name"],
            "arguments":"{\"message\":\"readable task\"}"});
        let restored = tools
            .restore_response(json!({"output":[call.clone()]}))
            .unwrap();
        assert_eq!(restored["output"][0]["namespace"], "collaboration");
        assert_eq!(
            restored["output"][0]["encrypted_function_args"],
            json!([]),
            "Codex otherwise treats the external model's plaintext as ciphertext"
        );
        for kind in ["response.output_item.added", "response.output_item.done"] {
            let event = tools
                .restore_event(json!({"type":kind,"item":call.clone()}))
                .unwrap()
                .unwrap();
            assert_eq!(event["item"]["encrypted_function_args"], json!([]));
        }
        let mut invalid = call.clone();
        invalid["encrypted_function_args"] = json!(["message"]);
        assert!(
            tools.restore_response(json!({"output":[invalid]})).is_err(),
            "external providers cannot claim to generate native ciphertext"
        );
    }
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
