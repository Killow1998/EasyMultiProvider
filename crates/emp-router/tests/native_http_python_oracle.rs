use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_router::native_http::{NativeHttpError, NativeRouter};
use emp_router::native_request::{NativeAuth, request_headers};
use emp_transport::{HttpClient, HttpClientPolicy, decode_content};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
struct RecordedRequest {
    headers: BTreeMap<String, String>,
    body: Value,
}

struct NativeUpstream {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl NativeUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind native upstream");
        listener
            .set_nonblocking(true)
            .expect("nonblocking upstream");
        let address = listener.local_addr().expect("upstream address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let attempts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests);
                            let attempts = Arc::clone(&attempts);
                            thread::spawn(move || serve(stream, requests, attempts));
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for NativeUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join upstream");
        }
    }
}

fn receive(mut stream: &TcpStream) -> RecordedRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request timeout");
    let mut wire = Vec::new();
    let header_end = loop {
        if let Some(position) = wire.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).expect("read request head");
        assert!(count > 0, "request ended before headers");
        wire.extend_from_slice(&chunk[..count]);
    };
    let head = String::from_utf8(wire[..header_end].to_vec()).expect("ASCII request head");
    assert!(
        head.starts_with("POST /v1/responses HTTP/1.1\r\n"),
        "{head}"
    );
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let length = headers["content-length"].parse::<usize>().unwrap();
    while wire.len() < header_end + length {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).expect("read request body");
        assert!(count > 0, "request ended before body");
        wire.extend_from_slice(&chunk[..count]);
    }
    let encoding = headers
        .get("content-encoding")
        .map(String::as_str)
        .unwrap_or("");
    let decoded = decode_content(
        wire[header_end..header_end + length].to_vec(),
        encoding,
        4 * 1024 * 1024,
        None,
    )
    .expect("decode upstream body");
    RecordedRequest {
        headers,
        body: serde_json::from_slice(&decoded).expect("upstream request JSON"),
    }
}

fn serve(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    attempts: Arc<Mutex<HashMap<String, usize>>>,
) {
    let request = receive(&stream);
    let case = request.body["_case"]
        .as_str()
        .expect("fixture case")
        .to_owned();
    requests.lock().unwrap().push(request.clone());
    let attempt = {
        let mut attempts = attempts.lock().unwrap();
        let value = attempts.entry(case.clone()).or_default();
        let current = *value;
        *value += 1;
        current
    };
    if case == "network_once" && attempt == 0 {
        return;
    }
    let (status, body) = match case.as_str() {
        "account_refresh" if attempt == 0 => (
            401,
            json!({"error":{"message":"expired selected credential"}}),
        ),
        "account_refresh_failed" | "forward_401" => {
            (401, json!({"error":{"message":"unauthorized"}}))
        }
        "reasoning_fallback" if request.body.get("reasoning_effort").is_some() => (
            400,
            json!({"error":{"message":"unknown field reasoning_effort"}}),
        ),
        "rate_limit" => (429, json!({"error":{"message":"rate limited"}})),
        "gateway_timeout" => (504, json!({"error":{"message":"gateway timeout"}})),
        "context" => (
            400,
            json!({"error":{"code":"context_length_exceeded","message":"maximum context length exceeded"}}),
        ),
        "large_error" => (
            400,
            json!({"message":format!("{} context length exceeded","x".repeat(5000))}),
        ),
        _ => (
            200,
            json!({
                "id":"resp_fixture", "object":"response", "status":"completed",
                "model":"upstream", "output":[], "future":{"opaque":[1,true,"x"]}
            }),
        ),
    };
    let encoded = serde_json::to_vec(&body).unwrap();
    let retry = if case == "rate_limit" {
        "Retry-After: 1.2\r\n"
    } else {
        ""
    };
    write!(
        stream,
        "HTTP/1.1 {status} reason\r\nContent-Type: application/json\r\nContent-Length: {}\r\nOpenAI-Model: upstream\r\nX-Codex-Turn-State: fixture-turn\r\nX-Models-Etag: upstream-etag\r\n{retry}Connection: close\r\n\r\n",
        encoded.len()
    )
    .expect("write response head");
    stream.write_all(&encoded).expect("write response body");
}

fn fixtures() -> Value {
    json!([
        {"case":"success", "auth":"forward"},
        {"case":"account_refresh", "auth":"account"},
        {"case":"account_refresh_failed", "auth":"account", "refresh_fails":true},
        {"case":"forward_401", "auth":"forward"},
        {"case":"reasoning_fallback", "auth":"forward", "reasoning_effort":"low"},
        {"case":"rate_limit", "auth":"forward"},
        {"case":"gateway_timeout", "auth":"forward"},
        {"case":"context", "auth":"forward"},
        {"case":"large_error", "auth":"forward"},
        {"case":"network_once", "auth":"forward"},
        {"case":"collaboration_plain", "auth":"forward", "plaintext":true},
        {"case":"collaboration_encrypted", "auth":"forward", "plaintext":true, "encrypted":true}
    ])
}

fn python_results(python: &str, base_url: &str, fixtures: &Value) -> Value {
    let script = r#"
import json, sys
from unittest.mock import patch
import easy_multi_provider.router as router
from easy_multi_provider.server import _router_error_body

base_url, fixtures = sys.argv[1], json.loads(sys.argv[2])
results=[]
for fixture in fixtures:
    case=fixture['case']; account=fixture['auth']=='account'; refreshes=[0]
    provider={'id':'native','base_url':base_url,'protocol':'responses',
              'auth_mode':'account' if account else 'forward'}
    if account: provider['account']={'id':'fixture'}
    model={'id':'requested'}
    if fixture.get('plaintext'): model['_emp_plaintext_collaboration']=True
    body={'model':'requested','input':'hello','stream':False,'_case':case}
    if 'reasoning_effort' in fixture: body['reasoning_effort']=fixture['reasoning_effort']
    if fixture.get('plaintext'):
        body['tools']=[{'type':'namespace','name':'collaboration','tools':[{
            'type':'function','name':'spawn_agent','parameters':{'type':'object','properties':{
                'message':{'type':'string','encrypted':True},'task_name':{'type':'string'}}}}]}]
        call={'type':'function_call','namespace':'collaboration','name':'spawn_agent',
              'call_id':'call_fixture','arguments':'opaque'}
        if fixture.get('encrypted'): call['encrypted_function_args']=['message']
        body['input']=[call]
    incoming={'Authorization':'Bearer caller','chatgpt-account-id':'caller-owner',
              'thread-id':'thread-fixture','x-openai-subagent':'subagent-fixture',
              'X-EMP-Request-ID':'0123456789abcdef'}
    def selected(*args, **kwargs):
        suffix='rotated' if refreshes[0] else 'selected'
        return {'Authorization':'Bearer '+suffix,'chatgpt-account-id':suffix+'-owner'}
    def refresh(*args, **kwargs):
        if fixture.get('refresh_fails'): raise ValueError('synthetic refresh failure')
        refreshes[0]+=1
    try:
        with patch.object(router,'auth_headers',side_effect=selected), patch.object(router,'refresh_account_quota',side_effect=refresh):
            status, media, raw=router.forward_responses(provider,body,model,incoming,upstream_model='upstream')
        result={'status':status,'content_type':media,'body':json.loads(raw),
                'headers':provider.get('_emp_response_headers',{}),'refreshes':refreshes[0]}
    except router.RouterError as exc:
        payload=_router_error_body(exc)
        headers=dict(getattr(exc,'response_headers',{}))
        if payload.get('error',{}).get('retry_after_seconds') is not None:
            headers['Retry-After']=str(payload['error']['retry_after_seconds'])
        result={'status':exc.status,'content_type':'application/json','body':payload,
                'headers':headers,'refreshes':refreshes[0]}
    results.append(result)
json.dump(results,sys.stdout,sort_keys=True)
"#;
    let output = Command::new(python)
        .args(["-c", script, base_url, &fixtures.to_string()])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("run Python native HTTP oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python oracle JSON")
}

fn route(base_url: &str, account: bool) -> ResolvedRoute {
    let provider = json!({
        "id":"native", "base_url":base_url, "protocol":"responses",
        "auth_mode":if account {"account"} else {"forward"},
        "account":if account {json!({"id":"fixture"})} else {Value::Null}
    });
    ResolvedRoute::new(
        "requested",
        "upstream",
        RouteSource::ExplicitModel,
        provider.as_object().unwrap().clone(),
        json!({"id":"requested","upstream_id":"upstream"})
            .as_object()
            .unwrap()
            .clone(),
        Protocol::Responses,
        Dialect::CodexNative,
        "native",
        format!("sha256:{}", "1".repeat(64)),
        "default",
    )
    .unwrap()
}

fn lower_headers(headers: BTreeMap<String, String>) -> Value {
    Value::Object(
        headers
            .into_iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), Value::String(value)))
            .collect::<Map<_, _>>(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_complete_http_matches_python_retries_errors_and_wire_requests() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        eprintln!("EMP_PYTHON_INTEROP is unset; native HTTP live oracle skipped");
        return;
    };
    let fixtures = fixtures();
    let python_upstream = NativeUpstream::start();
    let expected = python_results(&python, &python_upstream.base_url(), &fixtures);
    let python_requests = python_upstream.requests();

    let rust_upstream = NativeUpstream::start();
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = NativeRouter::new(&client);
    let incoming = BTreeMap::from([
        ("Authorization".to_owned(), "Bearer caller".to_owned()),
        ("chatgpt-account-id".to_owned(), "caller-owner".to_owned()),
        ("thread-id".to_owned(), "thread-fixture".to_owned()),
        (
            "x-openai-subagent".to_owned(),
            "subagent-fixture".to_owned(),
        ),
        ("X-EMP-Request-ID".to_owned(), "0123456789abcdef".to_owned()),
    ]);
    let mut actual = Vec::new();
    for fixture in fixtures.as_array().unwrap() {
        let account = fixture["auth"] == "account";
        let route = route(&rust_upstream.base_url(), account);
        let mut body =
            json!({"model":"requested","input":"hello","stream":false,"_case":fixture["case"]})
                .as_object()
                .unwrap()
                .clone();
        if let Some(effort) = fixture.get("reasoning_effort") {
            body.insert("reasoning_effort".to_owned(), effort.clone());
        }
        if fixture["plaintext"] == true {
            body.insert("tools".to_owned(), json!([{"type":"namespace","name":"collaboration","tools":[{
                "type":"function","name":"spawn_agent","parameters":{"type":"object","properties":{
                    "message":{"type":"string","encrypted":true},"task_name":{"type":"string"}}}}]}]));
            let mut call = json!({"type":"function_call","namespace":"collaboration","name":"spawn_agent",
                "call_id":"call_fixture","arguments":"opaque"});
            if fixture["encrypted"] == true {
                call["encrypted_function_args"] = json!(["message"]);
            }
            body.insert("input".to_owned(), json!([call]));
        }
        let mut refreshes = 0_u64;
        let result = router
            .execute_complete(
                &route,
                &body,
                fixture["plaintext"] == true,
                true,
                |refresh| {
                    if refresh {
                        if fixture["refresh_fails"] == true {
                            return Err(NativeHttpError::router(503, "synthetic refresh failure"));
                        }
                        refreshes += 1;
                    }
                    let selected = BTreeMap::from([
                        (
                            "Authorization".to_owned(),
                            format!(
                                "Bearer {}",
                                if refreshes > 0 { "rotated" } else { "selected" }
                            ),
                        ),
                        (
                            "chatgpt-account-id".to_owned(),
                            format!(
                                "{}-owner",
                                if refreshes > 0 { "rotated" } else { "selected" }
                            ),
                        ),
                    ]);
                    request_headers(
                        if account {
                            NativeAuth::Account(&selected)
                        } else {
                            NativeAuth::Forward
                        },
                        &incoming,
                        false,
                    )
                    .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
                },
            )
            .await;
        let value = match result {
            Ok(result) => json!({
                "status":result.status,"content_type":result.content_type,
                "body":serde_json::from_slice::<Value>(&result.body).unwrap(),
                "headers":lower_headers(result.headers),"refreshes":refreshes
            }),
            Err(error) => json!({
                "status":error.status,"content_type":"application/json","body":error.body,
                "headers":lower_headers(error.headers),"refreshes":refreshes
            }),
        };
        actual.push(value);
    }
    let mut expected = expected.as_array().unwrap().clone();
    for result in &mut expected {
        let headers = result["headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect::<Map<_, _>>();
        result["headers"] = Value::Object(headers);
    }
    assert_eq!(actual, expected);

    let rust_requests = rust_upstream.requests();
    assert_eq!(rust_requests.len(), python_requests.len());
    for (index, (actual, expected)) in rust_requests.iter().zip(&python_requests).enumerate() {
        assert_eq!(actual.body, expected.body, "request body {index}");
        for name in [
            "authorization",
            "chatgpt-account-id",
            "thread-id",
            "x-openai-subagent",
            "content-encoding",
        ] {
            assert_eq!(
                actual.headers.get(name),
                expected.headers.get(name),
                "header {name}, request {index}"
            );
        }
        assert_eq!(
            actual.headers.get("content-encoding").map(String::as_str),
            Some("zstd")
        );
        assert_eq!(actual.body["model"], "upstream");
    }
}
