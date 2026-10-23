//! Actual upstream requests after a Codex catalog revision and removal.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn updated_catalog_reaches_all_external_protocols() {
    let server = UpstreamServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = ExternalRouter::new(&client);
    let declaration = |revision| {
        json!({"type":"additional_tools","role":"developer","tools":[
            {"type":"namespace","name":"files","tools":[
                {"type":"function","name":"lookup","description":revision,"parameters":{"type":"object"}}
            ]}
        ]})
    };
    let mut request = body("upstream");
    request["tools"] =
        json!([{ "type":"function","name":"retired","parameters":{"type":"object"}}]);
    request["input"] = json!([
        declaration("old"),
        {"type":"function_call","namespace":"files","name":"lookup","call_id":"old-call","arguments":"{}"},
        {"type":"function_call_output","call_id":"old-call","output":"retained history"},
        declaration("updated"),
        {"type":"message","role":"developer","content":[{"type":"input_text","text":"Removed:\n- retired"}],
         "internal_chat_message_metadata_passthrough":{"content_item_kinds":["tools.removed_definition"]}},
        {"role":"user","content":"Continue"}
    ]);
    for protocol in [
        Protocol::ChatCompletions,
        Protocol::AnthropicMessages,
        Protocol::Responses,
    ] {
        let response = router
            .execute_complete(
                &route(&server.base_url(), protocol, "upstream"),
                &request,
                &BTreeMap::new(),
                &projection_ids(),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body["output_text"], "answer");
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    for sent in requests {
        let tools = sent.body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        let tool = tools[0].get("function").unwrap_or(&tools[0]);
        assert_eq!(tool["description"], "files.lookup\nupdated");
        assert!(sent.body.to_string().contains("retained history"));
        assert!(sent.body.to_string().contains("old-call"));
    }
}
