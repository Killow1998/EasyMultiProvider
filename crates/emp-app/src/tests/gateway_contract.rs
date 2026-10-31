//! Authenticated API forwarding uses instance keys, never upstream credentials.
use super::*;

#[test]
fn gateway_key_survives_login_changes_and_cannot_open_management_endpoints() {
    let (directory, server) = test_server();
    let key = server.state.backend.gateway_key.clone();
    let auth_path = &server.state.backend.accounts.native_auth_path;
    let original = br#"{"auth_mode":"apikey","OPENAI_API_KEY":"native-fixture"}"#;
    emp_state::atomic_write_private_state(auth_path, original).unwrap();
    let authorization = format!("Authorization: Bearer {key}");
    let forwarded = "Host: 192.168.1.20:14200";
    assert!(
        request(&server, "/v1/models", &[&authorization, forwarded]).starts_with("HTTP/1.1 200")
    );
    assert!(request(&server, "/v1/models", &[forwarded]).starts_with("HTTP/1.1 403"));
    assert!(
        request(
            &server,
            "/v1/models",
            &[&authorization, forwarded, "Origin: https://foreign.invalid"]
        )
        .starts_with("HTTP/1.1 403")
    );
    for path in ["/api/gateway", "/api/config", "/api/support-report"] {
        assert!(request(&server, path, &[&authorization]).starts_with("HTTP/1.1 401"));
        assert!(request(&server, path, &[&authorization, forwarded]).starts_with("HTTP/1.1 403"));
    }
    assert!(
        post(
            &server,
            "/api/decisions/models",
            br#"{}"#,
            &[&authorization]
        )
        .starts_with("HTTP/1.1 401")
    );
    let response = request(&server, "/api/gateway", &[&session_header(&server)]);
    assert!(response.contains("Cache-Control: no-store"));
    let connection: Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(connection["api_key"], key);
    assert_eq!(std::fs::read(auth_path).unwrap(), original);
    emp_state::atomic_write_private_state(
        auth_path,
        br#"{"tokens":{"access_token":"another-subscription","account_id":"fixture"}}"#,
    )
    .unwrap();
    assert!(
        request(&server, "/v1/models", &[&authorization, forwarded]).starts_with("HTTP/1.1 200")
    );
    assert_eq!(
        emp_state::gateway_key::load_or_create(&directory.path().join("state/gateway-key"))
            .unwrap(),
        key
    );
    assert!(!request(&server, "/", &[]).contains(&key));
    assert!(!request(&server, "/api/config", &[&session_header(&server)]).contains(&key));
    server.shutdown().unwrap();
}

#[test]
fn jev_decisions_forward_the_body_and_provider_key_without_entering_the_model_catalog() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let endpoint = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let (send, receive) = mpsc::channel();
    let worker = thread::spawn(move || {
        for status in [200, 429] {
            let (mut stream, _) = listener.accept().unwrap();
            let incoming = receive_upstream_request(&mut stream);
            send.send(incoming).unwrap();
            let body = if status == 200 {
                br#"{"model":"typesafe/jev-1.14","answers":{"effort":"low"},"usage":{"input_tokens":5}}"#.as_slice()
            } else {
                br#"{"error":{"message":"fixture quota","code":429}}"#.as_slice()
            };
            stream
                .write_all(&crate::http::response::response(
                    &format!("HTTP/1.1 {status} {}", status_text(status)),
                    "application/json",
                    body,
                    if status == 429 {
                        &[("Retry-After", "7")]
                    } else {
                        &[]
                    },
                ))
                .unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.json");
    std::fs::write(&config, serde_json::to_vec(&json!({
        "providers":[{"id":"openrouter","base_url":endpoint,"auth_mode":"api_key","api_key":"provider-fixture"}],
        "models":[
            {"id":"openrouter/jev","provider":"openrouter","upstream_id":"typesafe/jev-1.14","enabled":true},
            {"id":"openrouter/native-jev","provider":"openrouter","upstream_id":"jev-1.14.0","enabled":true},
            {"id":"openrouter/latest-jev","provider":"openrouter","upstream_id":"~typesafe/jev-latest","enabled":true}
        ]
    })).unwrap()).unwrap();
    let server =
        ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config).unwrap();
    let key = &server.state.backend.gateway_key;
    let authorization = format!("Authorization: Bearer {key}");
    let headers = [
        &authorization[..],
        "Host: localhost:14200",
        "X-EMP-Provider: openrouter",
    ];
    let body = br#"{"model":"typesafe/jev-1.14","state":{"task":"public fixture"},"questions":{"effort":{"type":"choice","instructions":"Choose the reasoning effort","criteria":{"low":"Routine work","xhigh":"Complex reasoning"}}}}"#;
    assert!(post(&server, "/v1/decisions", body, &[]).starts_with("HTTP/1.1 401"));
    assert!(
        post(&server, "/v1/decisions", br#"{"model":3}"#, &headers).starts_with("HTTP/1.1 400")
    );
    assert!(post(&server, "/v1/decisions", body, &[&authorization]).starts_with("HTTP/1.1 400"));
    for expected_status in [200, 429] {
        let response = post(&server, "/v1/decisions", body, &headers);
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected_status}")),
            "{response}"
        );
        assert!(!response.contains(key));
        if expected_status == 429 {
            assert!(response.contains("Retry-After: 7\r\n"));
        }
        let (path, upstream_headers, payload) =
            receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(path, "/api/alpha/decisions");
        assert_eq!(payload, serde_json::from_slice::<Value>(body).unwrap());
        assert_eq!(upstream_headers["authorization"], "Bearer provider-fixture");
        assert!(!upstream_headers.values().any(|value| value.contains(key)));
    }
    assert!(!request(&server, "/v1/models", &headers).contains("jev"));
    server.shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn decision_settings_persist_and_route_native_payloads_with_per_service_defaults() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (send, receive) = mpsc::channel();
    let worker = thread::spawn(move || {
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            let incoming = receive_upstream_request(&mut stream);
            let body = if incoming.0 == "/openai/v1/decisions" {
                br#"{"model":"gpt-6-luna","answers":[{"name":"urgent","type":"predicate","probability":0.2}],"usage":{"input_tokens":7}}"#.as_slice()
            } else {
                br#"{"answers":{"urgent":{"type":"noul","noul":0.2}},"usage":{"input_tokens":7}}"#
                    .as_slice()
            };
            send.send(incoming).unwrap();
            stream
                .write_all(&crate::http::response::response(
                    "HTTP/1.1 200 OK",
                    "application/json",
                    body,
                    &[],
                ))
                .unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.json");
    let providers = [
        ("openai", "/openai/v1/decisions", "openai", "gpt-6-luna"),
        (
            "router",
            "/router/api/alpha/decisions",
            "openrouter",
            "typesafe/jev-1.13",
        ),
        (
            "typesafe",
            "/typesafe/v1/systemone",
            "systemone",
            "jev-latest",
        ),
        (
            "router-systemone",
            "/router/api/v1",
            "systemone",
            "jev-1.14.0",
        ),
    ]
    .map(|(id, path, _, _)| {
        json!({"id":id, "base_url":format!("{origin}{path}"),
        "auth_mode":"api_key", "api_key":format!("key-{id}")})
    });
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "auto_enable_on_start":false, "providers":providers,
            "models":[{"id":"openai/luna", "provider":"openai", "upstream_id":"gpt-6-luna"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let server =
        ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config).unwrap();
    let management = session_header(&server);
    let public = request(&server, "/api/config", &[&management]);
    let mut candidate: Value =
        serde_json::from_str(public.split_once("\r\n\r\n").unwrap().1).unwrap();
    for (provider, (protocol, model)) in candidate["providers"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip([
            ("openai", "gpt-6-luna"),
            ("openrouter", "typesafe/jev-1.13"),
            ("systemone", "jev-latest"),
            ("systemone", "jev-1.14.0"),
        ])
    {
        provider["decision_api"] = json!({"protocol":protocol,"model":model});
    }
    candidate["decision_provider"] = json!("openai");
    let saved = post(
        &server,
        "/api/config",
        &serde_json::to_vec(&candidate).unwrap(),
        &[&management],
    );
    assert!(saved.starts_with("HTTP/1.1 200"), "{saved}");
    assert!(!saved.contains("key-openai"));
    let mut invalid = candidate.clone();
    invalid["providers"][0]["decision_api"]["protocol"] = json!("chat");
    assert!(
        post(
            &server,
            "/api/config",
            &serde_json::to_vec(&invalid).unwrap(),
            &[&management]
        )
        .starts_with("HTTP/1.1 400")
    );
    invalid["providers"][0]["decision_api"] = json!({"protocol":"openai", "model":""});
    assert!(
        post(
            &server,
            "/api/config",
            &serde_json::to_vec(&invalid).unwrap(),
            &[&management]
        )
        .starts_with("HTTP/1.1 400")
    );
    let key = server.state.backend.gateway_key.clone();
    server.shutdown().unwrap();

    let server =
        ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config).unwrap();
    assert_eq!(server.state.backend.gateway_key, key);
    let authorization = format!("Authorization: Bearer {key}");
    let openai = json!({"input":[{"role":"user","content":[{"type":"input_text","text":"Public fixture"},{"type":"input_image","image_url":"data:image/png;base64,AA=="}]}],
        "questions":[{"type":"predicate","name":"urgent","instructions":"Is this urgent?"}]});
    let jev = json!({"state":{"task":"public fixture"},"questions":{"urgent":{"type":"noul","instructions":"Is this urgent?"}}});
    for (provider, path, mut body, model, upstream_path) in [
        (
            None,
            "/v1/decisions",
            openai.clone(),
            "gpt-6-luna",
            "/openai/v1/decisions",
        ),
        (
            None,
            "/v1/decisions",
            {
                let mut body = openai;
                body["model"] = json!("explicit-model");
                body
            },
            "explicit-model",
            "/openai/v1/decisions",
        ),
        (
            Some("router"),
            "/v1/decisions",
            jev.clone(),
            "typesafe/jev-1.13",
            "/router/api/alpha/decisions",
        ),
        (
            Some("typesafe"),
            "/v1/systemone",
            jev.clone(),
            "jev-latest",
            "/typesafe/v1/systemone",
        ),
        (
            Some("router-systemone"),
            "/v1/systemone",
            jev,
            "jev-1.14.0",
            "/router/api/v1/systemone",
        ),
    ] {
        let provider_header = provider.map(|id| format!("X-EMP-Provider: {id}"));
        let mut headers = vec![
            authorization.as_str(),
            "Host: localhost:14200",
            "TypeSafe-Danger-Zone-Long-Context: true",
            "TypeSafe-Danger-Zone-Images: true",
        ];
        if let Some(header) = &provider_header {
            headers.push(header);
        }
        let result = post(&server, path, &serde_json::to_vec(&body).unwrap(), &headers);
        assert!(result.starts_with("HTTP/1.1 200"), "{result}");
        let (path, upstream_headers, payload) =
            receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(path, upstream_path);
        body["model"] = json!(model);
        assert_eq!(payload, body);
        assert_eq!(
            upstream_headers["authorization"],
            format!("Bearer key-{}", provider.unwrap_or("openai"))
        );
        assert_eq!(
            upstream_headers["typesafe-danger-zone-long-context"],
            "true"
        );
        assert_eq!(upstream_headers["typesafe-danger-zone-images"], "true");
        assert!(!upstream_headers.contains_key("x-emp-provider"));
        let result: Value = serde_json::from_str(result.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(result["answers"].is_array(), provider.is_none());
    }
    let catalog = request(
        &server,
        "/v1/models",
        &[&authorization, "Host: localhost:14200"],
    );
    assert!(catalog.contains("openai/luna"), "{catalog}");
    server.shutdown().unwrap();
    worker.join().unwrap();
}
