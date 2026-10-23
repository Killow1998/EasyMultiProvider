use super::*;

#[test]
fn removed_cli_tools_are_rejected_until_redeclared_with_the_current_kind() {
    let declaration = |kind, description| {
        json!({"type":"additional_tools","tools":[
            {"type":"namespace","name":"files","tools":[
                {"type":kind,"name":"read","description":description,"parameters":{"type":"object"}}
            ]}
        ]})
    };
    let mut request =
        json!({"input":[declaration("function", "old"), declaration("function", "updated")]});
    let mut output = json!({"structured_output":{"answer":"","tool_calls":[{
        "type":"function_call","namespace":"files","name":"read","arguments":{},"input":"patch"
    }]}});
    assert!(response_from_cli(&output, &test_route(), &request, 200).is_ok());
    request["input"].as_array_mut().unwrap().push(json!({"role":"developer","content":[{
        "type":"input_text","text":"The following tools are no longer available. Do not call them:\n- files.read"
    }]}));
    assert!(matches!(
        response_from_cli(&output, &test_route(), &request, 200),
        Err(ClaudeCliError::Failure("claude_cli_unknown_tool_proposal"))
    ));
    request["input"]
        .as_array_mut()
        .unwrap()
        .push(declaration("custom", "restored"));
    assert!(matches!(
        response_from_cli(&output, &test_route(), &request, 200),
        Err(ClaudeCliError::Failure("claude_cli_unknown_tool_proposal"))
    ));
    output["structured_output"]["tool_calls"][0]["type"] = json!("custom_tool_call");
    let response = response_from_cli(&output, &test_route(), &request, 200).unwrap();
    assert_eq!(response.body["output"][0]["type"], "custom_tool_call");
    assert_eq!(response.body["output"][0]["name"], "read");
    assert_eq!(response.body["output"][0]["namespace"], "files");
}
