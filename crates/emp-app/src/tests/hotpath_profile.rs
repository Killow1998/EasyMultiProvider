//! Opt-in measurements with synthetic state and real loopback HTTP boundaries.
//! Run explicitly with --features hotpath --ignored --nocapture (one test thread).
use super::*;
use crate::services::{catalog, events, request_outcome};
use std::hint::black_box;
use std::time::Instant;
mod upstream;

fn measure(name: &str, iterations: usize, mut operation: impl FnMut()) {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        operation();
        samples.push(started.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    println!(
        "PROFILE {}",
        json!({"scenario":name,"samples":iterations,
        "p50_ns":samples[iterations / 2],"p95_ns":samples[(iterations - 1) * 95 / 100]})
    );
}

fn response(server: &ServerHandle, body: &[u8]) -> String {
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(stream, "POST /v1/responses HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\nAuthorization: Bearer fixture-caller\r\nConnection: close\r\n\r\n",
        server.local_addr(), body.len(), session_header(server)).unwrap();
    stream.write_all(body).unwrap();
    let mut wire = String::new();
    stream.read_to_string(&mut wire).unwrap();
    wire
}

#[test]
#[ignore = "explicit local profiling workload; no live models or credentials"]
#[hotpath::main(percentiles = [50, 95, 99])]
fn isolated_hotpath_workload() {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let native_path = root.join("codex/auth.json");
    std::fs::create_dir_all(native_path.parent().unwrap()).unwrap();
    let catalog_path = root.join("native-catalog.json");
    let native_models: Vec<_> = (0..40)
        .map(|n| {
            json!({
                "slug":format!("fixture-{n}"),"display_name":format!("Fixture {n}"),
                "context_window":128000,"visibility":"list","supported_in_api":true
            })
        })
        .collect();
    std::fs::write(&catalog_path, json!({"models":native_models}).to_string()).unwrap();
    let upstream = upstream::Upstream::start();
    let config_path = root.join("config.json");
    std::fs::write(&config_path, json!({
        "native_catalog_path":catalog_path,"auto_enable_on_start":false,
        "codex_base_url":upstream.base_url(),
        "providers":[
            {"id":"native","base_url":upstream.base_url(),"protocol":"responses","auth_mode":"forward"},
            {"id":"demo","base_url":upstream.base_url(),"protocol":"chat_completions","auth_mode":"api_key","api_key":"fixture-key"}
        ],
        "models":[
            {"id":"native/alias","provider":"native","upstream_id":"upstream","enabled":true},
            {"id":"demo/model","provider":"demo","upstream_id":"upstream","context_window":2000000,"enabled":true}
        ]
    }).to_string()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "missing-profile-codex",
        native_path,
    )
    .unwrap();

    // Include imported-account credential reads and duplicate-account detection.
    let mut accounts = Vec::new();
    for n in 0..4 {
        let account_dir = root.join(format!("account-{n}"));
        std::fs::create_dir(&account_dir).unwrap();
        let auth_path = account_dir.join("auth.json");
        let auth = json!({"tokens":{"access_token":"fixture-access","account_id":format!("fixture-owner-{n}")}});
        server
            .state
            .backend
            .configuration
            .vault
            .write_encrypted_json(&auth_path, &auth)
            .unwrap();
        let owner =
            emp_codex::native_catalog_owner(&emp_codex::account_auth_headers(&auth).unwrap());
        std::fs::write(
            account_dir.join("models_cache.json"),
            json!({"models":native_models,"account_owner":owner,"base_url":upstream.base_url()})
                .to_string(),
        )
        .unwrap();
        accounts.push(
            json!({"id":format!("fixture-{n}"),"prefix":format!("fixture-{n}"),
            "name":format!("Fixture {n}"),"auth_file":auth_path,"enabled":true}),
        );
    }
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["accounts"] = json!(accounts);
    measure("catalog_four_accounts", 100, || {
        black_box(catalog::response_catalog_etag(&server.state).unwrap());
    });

    let delta = json!({"type":"response.output_text.delta","delta":"hello 世界: , \"quoted\"\n"});
    measure("sse_small_delta", 10000, || {
        black_box(events::sse_frame("response.output_text.delta", &delta).unwrap());
    });
    let tool = json!({"type":"response.function_call_arguments.delta","delta":"x".repeat(16384)});
    measure("sse_large_tool", 1000, || {
        black_box(events::sse_frame("response.function_call_arguments.delta", &tool).unwrap());
    });
    let long_input = "synthetic history 世界\n".repeat(40000);
    let long_body =
        json!({"model":"demo/model","input":[{"role":"user","content":long_input}],"stream":true});
    measure("request_bytes_long_history", 100, || {
        black_box(request_outcome::profile_request_bytes(&long_body));
    });

    // Alternate routes and short/long history through the actual HTTP server.
    for (name, model, input, streaming) in [
        ("http_native_short", "native/alias", "hello", false),
        ("http_external_sse", "demo/model", "hello", true),
        (
            "http_external_long_sse",
            "demo/model",
            long_input.as_str(),
            true,
        ),
    ] {
        let body = serde_json::to_vec(&json!({"model":model,
            "input":[{"role":"user","content":input}],"stream":streaming}))
        .unwrap();
        measure(name, 10, || {
            let wire = response(&server, &body);
            assert!(wire.starts_with("HTTP/1.1 200 OK"), "{wire}");
            let (_, response) = wire.split_once("\r\n\r\n").unwrap();
            if streaming {
                assert!(response.contains("response.completed"));
                assert!(response.contains("response.output_text.delta"));
                assert!(!response.contains("response.failed"));
            } else {
                let value: Value = serde_json::from_str(response).unwrap();
                assert_eq!(value["status"], "completed");
                assert_eq!(value["future"], json!({"keep":true}));
            }
        });
    }
    assert_eq!(upstream.requests(), 30);
    server.shutdown().unwrap();
}
