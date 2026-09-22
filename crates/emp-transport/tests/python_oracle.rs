use emp_transport::{
    FailureClass, FailurePhase, HttpClientPolicy, HttpFailureInput, HttpMethod, MemoryStatus,
    ProxyEnvironment, ProxyPolicy, RequestLimits, RequestLimitsConfig, SseJsonParser,
    TimeoutPolicy, TransportKind, UpstreamFailure, decode_content, http_failure,
    normalize_error_class, protocol_fallback_allowed, public_failure_message, retry_allowed,
    sse_json_events, status_error_class,
};
use flate2::Compression;
use flate2::write::{GzEncoder, ZlibEncoder};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn sse_parser_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let cases = json!([
        ["data: {\"type\":\"single\"}\n\n"],
        ["data: {\"type\":", "\"split\"}\r\n\r\n"],
        [": keepalive\ndata: {\"value\":\ndata: [1,2]}\n\n"],
        ["data: [DONE]\n\ndata: {\"type\":\"after_done\"}\n\n"],
        ["event: ignored\nid: 3\ndata: {\"type\":\"tail\"}"]
    ]);
    let script = r#"
import json, sys
from easy_multi_provider.transport import sse_json_events
cases = json.load(sys.stdin)
json.dump([list(sse_json_events([chunk.encode() for chunk in case])) for case in cases],
          sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python SSE oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(serde_json::to_string(&cases).unwrap().as_bytes())
        .expect("write fixtures");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python SSE oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    let rust = cases
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            let chunks = case
                .as_array()
                .unwrap()
                .iter()
                .map(|chunk| chunk.as_str().unwrap().as_bytes());
            Value::Array(
                sse_json_events(chunks)
                    .expect("Rust projection")
                    .into_iter()
                    .map(Value::Object)
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), oracle);
}

#[test]
fn split_multibyte_utf8_is_supported() {
    let wire = "data: {\"text\":\"思考\"}\n\n".as_bytes();
    let split = wire.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
    let mut parser = SseJsonParser::new();
    assert!(parser.push(&wire[..split]).unwrap().is_empty());
    let mut events = parser.push(&wire[split..]).unwrap();
    events.extend(parser.finish().unwrap());
    assert_eq!(
        events,
        vec![json!({"text": "思考"}).as_object().unwrap().clone()]
    );
}

#[test]
fn request_admission_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let script = r#"
import json
from easy_multi_provider.request_limits import RequestLimits

limits = RequestLimits(
    baseline=64, maximum=256, available_memory=lambda: 2048,
    growth_quantum=64, memory_headroom=0,
)
first = limits.request("http")
second = limits.request("websocket")
first.ensure(64)
first.ensure(65)
first.ensure(129)
errors = []
try:
    second.ensure(65)
except Exception as exc:
    errors.append({
        "reason": exc.reason, "limit": exc.limit,
        "available_bytes": exc.available_bytes,
        "required_memory_bytes": exc.required_memory_bytes,
    })
first.release()
second.ensure(65)
third = limits.request("http")
try:
    third.ensure(257)
except Exception as exc:
    errors.append({
        "reason": exc.reason, "limit": exc.limit,
        "available_bytes": exc.available_bytes,
        "required_memory_bytes": exc.required_memory_bytes,
    })
snapshot = limits.snapshot()
snapshot["run_id"] = "run-test"
for notice in snapshot["notices"]:
    notice["timestamp"] = 0
print(json.dumps({"snapshot": snapshot, "errors": errors},
                 ensure_ascii=False, separators=(",", ":")))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .output()
        .expect("spawn Python admission oracle");
    assert!(
        output.status.success(),
        "Python admission oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle_line = output
        .stdout
        .split(|byte| *byte == b'\n')
        .rev()
        .find(|line| !line.is_empty())
        .expect("admission oracle output");
    let oracle: Value = serde_json::from_slice(oracle_line).expect("admission oracle JSON");

    let limits = RequestLimits::new(
        RequestLimitsConfig {
            baseline: 64,
            maximum: 256,
            growth_quantum: 64,
            memory_headroom: 0,
        },
        || Some(MemoryStatus::available(2048)),
        || 0,
        "run-test",
    )
    .unwrap();
    let mut first = limits.request(TransportKind::Http);
    let mut second = limits.request(TransportKind::WebSocket);
    first.ensure(64).unwrap();
    first.ensure(65).unwrap();
    first.ensure(129).unwrap();
    let memory = second.ensure(65).expect_err("memory pressure");
    first.release();
    second.ensure(65).unwrap();
    let mut third = limits.request(TransportKind::Http);
    let hard = third.ensure(257).expect_err("hard limit");
    let rust = json!({
        "snapshot": limits.snapshot().unwrap(),
        "errors": [
            {
                "reason": memory.reason.as_str(), "limit": memory.limit,
                "available_bytes": memory.available_bytes,
                "required_memory_bytes": memory.required_memory_bytes,
            },
            {
                "reason": hard.reason.as_str(), "limit": hard.limit,
                "available_bytes": hard.available_bytes,
                "required_memory_bytes": hard.required_memory_bytes,
            }
        ]
    });
    assert_eq!(rust, oracle);
}

fn gzip(value: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(value).unwrap();
    encoder.finish().unwrap()
}

fn deflate(value: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(value).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn content_decoding_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let raw = b"portable compressed history ".repeat(20);
    let gzipped = gzip(&raw);
    let cases = json!([
        {"encoding": "identity", "body": raw},
        {"encoding": "gzip", "body": gzipped},
        {"encoding": "deflate", "body": deflate(&raw)},
        {"encoding": "zstd", "body": zstd::stream::encode_all(raw.as_slice(), 0).unwrap()},
        {"encoding": "gzip, zstd", "body": zstd::stream::encode_all(gzip(&raw).as_slice(), 0).unwrap()}
    ]);
    let script = r#"
import json, sys
from easy_multi_provider.transport import decode_content
cases = json.load(sys.stdin)
json.dump([
    list(decode_content(bytes(case["body"]), case["encoding"], 1048576))
    for case in cases
], sys.stdout, separators=(",", ":"))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python content decoder oracle");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&cases).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().expect("wait for decoder oracle");
    assert!(
        output.status.success(),
        "Python decoder oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("decoder oracle JSON");
    let rust = cases
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            let bytes = case["body"]
                .as_array()
                .unwrap()
                .iter()
                .map(|byte| byte.as_u64().unwrap() as u8)
                .collect::<Vec<_>>();
            Value::Array(
                decode_content(bytes, case["encoding"].as_str().unwrap(), 1_048_576, None)
                    .unwrap()
                    .into_iter()
                    .map(|byte| Value::from(u64::from(byte)))
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), oracle);
}

#[test]
fn failure_policy_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let script = r#"
import json
from easy_multi_provider.transport_failures import (
    UpstreamFailure, _http_failure_reason, normalize_error_class,
    protocol_fallback_allowed, public_failure_message, retry_allowed,
    status_error_class,
)
statuses = [None, 200, 401, 403, 402, 404, 405, 415, 501, 408, 429, 500, 502, 503, 504]
http_cases = [
    [401, "secret"], [402, "secret"], [413, "secret"],
    [400, "context window exceeded"], [429, "quota exhausted"],
    [429, "provider capacity overloaded"], [429, "busy"],
    [500, "secret"], [502, "secret"], [503, "secret"], [504, "secret"],
]
public_cases = [
    ["proxy_unavailable", "private", 503], ["dns_failure", None, 503],
    ["tls_failure", None, 502], ["network", None, 503],
    ["connect_timeout", None, 504], ["rate_limit", None, 429],
    ["auth", None, 401], ["stream_incomplete", None, 502],
    ["upstream_5xx", "private", 502],
    ["malformed_terminal", "sse_invalid_json", 502],
]
classes = ["connect_timeout", "first_event_timeout", "network", "proxy_reset",
           "rate_limit", "upstream_504", "stream_incomplete"]
result = {
    "status": [status_error_class(status) for status in statuses],
    "http_reason": [_http_failure_reason(status, detail) for status, detail in http_cases],
    "protocol": [
        protocol_fallback_allowed(status, output, terminal)
        for status, output, terminal in ([404, False, False], [404, True, False],
                                         [404, False, True], [502, False, False])
    ],
    "retry": [
        retry_allowed(UpstreamFailure(name), attempt, replayable, output)
        for name, attempt, replayable, output in
        [(name, attempt, replayable, output) for name in classes
         for attempt in (0, 1) for replayable in (False, True) for output in (False, True)]
    ],
    "public": [public_failure_message(*case) for case in public_cases],
    "normalize": [
        normalize_error_class(" Rate Limit "),
        normalize_error_class("private secret", "router_error"),
    ],
}
print(json.dumps(result, ensure_ascii=False, separators=(",", ":")))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .output()
        .expect("spawn Python failure-policy oracle");
    assert!(
        output.status.success(),
        "Python failure-policy oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("failure-policy oracle JSON");

    let statuses = [
        None,
        Some(200),
        Some(401),
        Some(403),
        Some(402),
        Some(404),
        Some(405),
        Some(415),
        Some(501),
        Some(408),
        Some(429),
        Some(500),
        Some(502),
        Some(503),
        Some(504),
    ];
    let http_cases = [
        (401, "secret"),
        (402, "secret"),
        (413, "secret"),
        (400, "context window exceeded"),
        (429, "quota exhausted"),
        (429, "provider capacity overloaded"),
        (429, "busy"),
        (500, "secret"),
        (502, "secret"),
        (503, "secret"),
        (504, "secret"),
    ];
    let public_cases = [
        (FailureClass::ProxyUnavailable, Some("private"), 503),
        (FailureClass::DnsFailure, None, 503),
        (FailureClass::TlsFailure, None, 502),
        (FailureClass::Network, None, 503),
        (FailureClass::ConnectTimeout, None, 504),
        (FailureClass::RateLimit, None, 429),
        (FailureClass::Auth, None, 401),
        (FailureClass::StreamIncomplete, None, 502),
        (FailureClass::Upstream5xx, Some("private"), 502),
        (
            FailureClass::MalformedTerminal,
            Some("sse_invalid_json"),
            502,
        ),
    ];
    let classes = [
        FailureClass::ConnectTimeout,
        FailureClass::FirstEventTimeout,
        FailureClass::Network,
        FailureClass::ProxyReset,
        FailureClass::RateLimit,
        FailureClass::Upstream504,
        FailureClass::StreamIncomplete,
    ];
    let mut retries = Vec::new();
    for class in classes {
        for attempt in [0, 1] {
            for replayable in [false, true] {
                for output in [false, true] {
                    retries.push(retry_allowed(
                        &UpstreamFailure::new(class, 502, FailurePhase::TerminalValidation),
                        attempt,
                        replayable,
                        output,
                        false,
                    ));
                }
            }
        }
    }
    let rust = json!({
        "status": statuses.map(status_error_class).map(FailureClass::as_str),
        "http_reason": http_cases.map(|(status, detail)| {
            http_failure(HttpFailureInput {
                status, detail, proxy_evidence: false, retry_after_seconds: None,
            }).failure_reason.unwrap()
        }),
        "protocol": [
            protocol_fallback_allowed(404, false, false),
            protocol_fallback_allowed(404, true, false),
            protocol_fallback_allowed(404, false, true),
            protocol_fallback_allowed(502, false, false),
        ],
        "retry": retries,
        "public": public_cases.map(|(class, reason, status)| {
            public_failure_message(class, reason, status)
        }),
        "normalize": [
            normalize_error_class(Some(" Rate Limit "), FailureClass::StreamError).as_str(),
            normalize_error_class(Some("private secret"), FailureClass::RouterError).as_str(),
        ],
    });
    assert_eq!(rust, oracle);
}

#[test]
fn http_proxy_selection_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let cases = json!([
        {"url": "http://127.0.0.1:8080/v1", "settings": {"https": "http://proxy.example:8080"}},
        {"url": "https://upstream.example/v1", "settings": {"wss": "http://wss-proxy.example:8081", "https": "http://https-proxy.example:8082"}},
        {"url": "https://upstream.example/v1", "settings": {"socks": "http://socks.example:1080", "https": "http://https-proxy.example:8082"}},
        {"url": "http://upstream.example/v1", "settings": {"https": "http://shared.example:8080", "http": "http://http-only.example:8081"}},
        {"url": "https://internal.example/v1", "settings": {"https": "http://proxy.example:8080", "no": "internal.example"}},
        {"url": "https://api.example.test/v1", "settings": {"all": "http://fallback.example:3128", "no": ".example.test"}},
        {"url": "https://upstream.example/v1", "settings": {"all": "http://user:pass@fallback.example:3128"}}
    ]);
    let script = r#"
import json, sys
from unittest.mock import patch
from easy_multi_provider.network_proxy import proxy_for_url, proxy_identity
result = []
for case in json.load(sys.stdin):
    with patch("easy_multi_provider.network_proxy.current_proxies", return_value=case["settings"]):
        selected = proxy_for_url(case["url"])
        result.append({"proxy": selected is not None, "identity": proxy_identity(case["url"])})
json.dump(result, sys.stdout, separators=(",", ":"))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python proxy oracle");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&cases).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Python proxy oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();

    let actual = Value::Array(
        cases
            .as_array()
            .unwrap()
            .iter()
            .map(|case| {
                let settings = case["settings"].as_object().unwrap();
                let value = |name: &str| {
                    settings
                        .get(name)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                let no_proxy = value("no")
                    .map(|value| value.split(',').map(str::to_owned).collect())
                    .unwrap_or_default();
                let environment = ProxyEnvironment {
                    http: value("http"),
                    https: value("https"),
                    ws: value("ws"),
                    wss: value("wss"),
                    socks: value("socks"),
                    all: value("all"),
                    no_proxy,
                };
                let plan = HttpClientPolicy::new(
                    ProxyPolicy::from_environment(environment),
                    TimeoutPolicy::default(),
                )
                .plan(
                    HttpMethod::Post,
                    case["url"].as_str().unwrap(),
                    Default::default(),
                    true,
                )
                .unwrap();
                json!({
                    "proxy": plan.route.proxy_origin.is_proxy(),
                    "identity": plan.route.proxy_origin.pool_token(),
                })
            })
            .collect(),
    );
    assert_eq!(actual, expected);
}
