//! Changing collaboration policy must replace an inherited upstream tool catalog.
use super::*;

#[test]
fn collaboration_is_opt_in_and_switching_policy_recovers_the_socket_catalog() {
    let upstream = Upstream::start(1024 * 1024);
    let (_directory, server, model) = server(&upstream, false);
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["providers"] = json!([{
        "id":"unused", "base_url":"http://127.0.0.1:1/v1", "protocol":"responses",
        "auth_mode":"api_key", "api_key":"fixture-only"
    }]);
    let input = json!([
        {"type":"additional_tools", "role":"developer", "id":"at_fixture", "tools":[
            {"type":"namespace", "name":"collaboration", "tools":[
                {"type":"function", "name":"send_message", "parameters":{
                    "type":"object", "properties":{"message":{"type":"string", "encrypted":true}}
                }}
            ]}
        ]},
        {"role":"user", "content":"synthetic task"}
    ]);
    let mut hash = Sha256::new();
    append_input(&mut hash, &json!({"input":input}));
    let native_hash = format!("{:x}", hash.finalize());
    let mut client = connect(&server, "collaboration-policy-fixture");
    let mut prior = completed(turn(
        &mut client,
        &json!({
            "type":"response.create", "model":model, "input":input
        }),
    ));
    for enabled in ["on", "on_demand"] {
        server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap()["external_collaboration"] = json!(enabled);
        let recovery = turn(
            &mut client,
            &json!({
                "type":"response.create", "model":model, "previous_response_id":prior, "input":[]
            }),
        );
        assert_eq!(recovery["error"]["code"], "previous_response_not_found");
        assert_eq!(recovery["error"]["failure_reason"], "route_mismatch");
        prior = completed(turn(
            &mut client,
            &json!({
                "type":"response.create", "model":model, "input":input
            }),
        ));
    }
    client.close();
    server.shutdown().unwrap();
    let stats = upstream.traffic.lock().unwrap();
    assert_eq!(stats.ws_connections, 3);
    assert_eq!(stats.http_requests, 0);
    assert_eq!(
        stats.generated_hashes.len(),
        3,
        "policy recovery never generates a response"
    );
    assert_eq!(
        stats.generated_hashes[0], native_hash,
        "API providers must not alter native tools by default"
    );
    assert_ne!(
        stats.generated_hashes[1], native_hash,
        "explicit compatibility changes the upstream schema"
    );
    assert_eq!(
        stats.generated_hashes[2], native_hash,
        "turning compatibility off restores the exact native input"
    );
}
