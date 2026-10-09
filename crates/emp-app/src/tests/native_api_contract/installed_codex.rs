//! Real Codex app-server steers against isolated EMP and synthetic upstreams.
use super::support::*;
use super::*;
use std::process::{Child, ChildStdin, Command, Stdio};

struct Codex {
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<Value>,
    reader: Option<JoinHandle<()>>,
}

impl Codex {
    fn start(home: &Path) -> Self {
        let mut command =
            Command::new(std::env::var_os("EMP_TEST_CODEX_CLI").unwrap_or_else(|| "codex".into()));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .args(["app-server", "--listen", "stdio://"])
            .env("CODEX_HOME", home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("xdg"))
            .env("EMP_TEST_KEY", "fixture-only")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .current_dir(home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(home.join("stderr.log")).unwrap())
            .spawn()
            .expect("installed Codex app-server");
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str(&line) else {
                    continue;
                };
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
            reader: Some(reader),
        }
    }

    fn send(&mut self, value: Value) {
        writeln!(self.input, "{value}").unwrap();
        self.input.flush().unwrap();
    }

    fn wait(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let value = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("Codex event deadline");
            if predicate(&value) {
                return value;
            }
        }
    }

    fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"id":id,"method":method,"params":params}));
        let value = self.wait(|value| value["id"] == id);
        assert!(value.get("error").is_none(), "Codex RPC error: {value}");
        value["result"].clone()
    }
}

impl Drop for Codex {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as libc::pid_t), libc::SIGTERM);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
#[ignore = "requires installed Codex CLI and isolated loopback access; run explicitly"]
fn installed_codex_steers_an_external_stream_and_finishes_the_same_turn() {
    steering(false);
}

#[test]
#[ignore = "requires installed Codex CLI and isolated loopback access; run explicitly"]
fn installed_codex_interrupts_a_native_stream_and_continues_the_same_turn() {
    steering(true);
}

fn steering(native: bool) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let home = root.join("codex");
    std::fs::create_dir(&home).unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, first) = mpsc::channel();
    let (release, steered) = mpsc::channel();
    let upstream = thread::spawn(move || {
        if native {
            native_upstream(listener, sender)
        } else {
            http_upstream(listener, sender, steered)
        }
    });
    let mut config = json!({"providers":[{"id":"demo","name":"Demo","base_url":format!("http://{address}/v1"),"protocol":"responses","auth_mode":if native {"forward"} else {"api_key"},"api_key":"fixture-only"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream","enabled":true}]});
    let catalog = emp_codex::merged_catalog::build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let mut native_catalog = json!({"models":[]});
    if native {
        let mut entry = catalog["models"][0].clone();
        entry["use_responses_lite"] = json!(true);
        native_catalog["models"] = json!([entry]);
    }
    config["native_catalog_path"] = json!(home.join("native-models.json"));
    std::fs::write(
        home.join("native-models.json"),
        serde_json::to_vec(&native_catalog).unwrap(),
    )
    .unwrap();
    let catalog = emp_codex::merged_catalog::build_catalog(
        &config,
        &native_catalog,
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    std::fs::write(
        home.join("models.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let server = ServerHandle::start_with_config(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &root.join("config.json"),
    )
    .unwrap();
    let settings = format!(
        "model = \"demo/model\"\nmodel_provider = \"emp_fixture\"\nmodel_catalog_json = {}\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\ninstant_interrupt = true\nresponses_websockets = true\nresponses_websockets_v2 = true\n[model_providers.emp_fixture]\nname = \"EMP fixture\"\nbase_url = \"http://{}/v1\"\nwire_api = \"responses\"\nenv_key = \"EMP_TEST_KEY\"\nrequires_openai_auth = false\nsupports_websockets = true\nhttp_headers = {{ \"X-EMP-Session\" = {} }}\n",
        serde_json::to_string(&home.join("models.json").to_string_lossy()).unwrap(),
        server.local_addr(),
        serde_json::to_string(&server.session_token()).unwrap()
    );
    std::fs::write(home.join("config.toml"), settings).unwrap();
    let mut codex = Codex::start(&home);
    let version = codex.rpc(1, "initialize", json!({"clientInfo":{"name":"emp_steering_fixture","title":"EMP steering fixture","version":"1"},"capabilities":{"experimentalApi":true}}));
    eprintln!("Codex initialize: {version}");
    codex.send(json!({"method":"initialized"}));
    let started = codex.rpc(2, "thread/start", json!({"model":"demo/model","modelProvider":"emp_fixture","cwd":home,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true}));
    let id = started["thread"]["id"].as_str().unwrap();
    let turn = codex.rpc(
        3,
        "turn/start",
        json!({"threadId":id,"input":[{"type":"text","text":"first fixture message"}]}),
    );
    let turn_id = turn["turn"]["id"].as_str().unwrap();
    first
        .recv_timeout(Duration::from_secs(15))
        .expect("Codex first inference");
    codex.wait(|value| value["method"] == "item/agentMessage/delta");
    let before = crate::tests::internal_events_contract::journal(&root);
    assert!(
        before
            .iter()
            .filter(|event| event["event"] == "request_started")
            .all(|event| event["fields"]["transport"] == "websocket"),
        "warmup must not force Codex into HTTP fallback"
    );
    // This error is produced by Codex's RPC validation, before reaching EMP.
    codex.send(json!({"id":5,"method":"turn/steer","params":{"threadId":id,"expectedTurnId":"wrong-turn","input":[{"type":"text","text":"rejected steering"}]}}));
    let rejected = codex.wait(|value| value["id"] == 5);
    assert!(
        rejected["error"].is_object(),
        "Codex must reject the wrong turn: {rejected}"
    );
    assert_eq!(
        before
            .iter()
            .filter(|event| event["event"] == "request_started")
            .count(),
        crate::tests::internal_events_contract::journal(&root)
            .iter()
            .filter(|event| event["event"] == "request_started")
            .count()
    );
    let steering = codex.rpc(4, "turn/steer", json!({"threadId":id,"expectedTurnId":turn_id,"input":[{"type":"text","text":"steered fixture message"}]}));
    assert_eq!(steering["turnId"], turn_id);
    if !native {
        release.send(()).unwrap();
    }
    let completed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        codex.wait(|value| value["method"] == "turn/completed")
    }))
    .unwrap_or_else(|error| {
        eprintln!(
            "Fixture stderr: {}",
            std::fs::read_to_string(home.join("stderr.log")).unwrap()
        );
        eprintln!(
            "Fixture controls: {:?}",
            crate::tests::internal_events_contract::journal(&root)
                .iter()
                .filter(|event| event["event"] == "websocket_control")
                .collect::<Vec<_>>()
        );
        std::panic::resume_unwind(error)
    });
    assert_eq!(
        completed["params"]["turn"]["status"], "completed",
        "{completed}"
    );
    assert_eq!(completed["params"]["turn"]["id"], turn_id);
    drop(codex);
    let bodies = upstream.join().unwrap();
    assert!(
        bodies[1]["input"]
            .to_string()
            .contains("steered fixture message")
    );
    if native {
        assert_eq!(bodies[1]["previous_response_id"], "resp_codex_first");
    } else {
        assert!(
            bodies[1].get("previous_response_id").is_none(),
            "HTTP adapter requires full-history recovery"
        );
    }
    server.shutdown().unwrap();
    let journal = crate::tests::internal_events_contract::journal(&root);
    assert_eq!(
        journal
            .iter()
            .any(|event| event["event"] == "websocket_control"
                && event["fields"]["forwarded"] == true),
        native
    );
    assert!(
        !journal
            .iter()
            .any(|event| event["event"] == "response_error"
                && event["fields"]["error_code"] == "invalid_request")
    );
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "upstream accept deadline");
        match listener.accept() {
            Ok((socket, _)) => {
                socket
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn partial() -> [Value; 2] {
    [
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_partial","type":"message","role":"assistant","phase":"commentary","content":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_partial","output_index":0,"content_index":0,"delta":"fixture partial"}),
    ]
}

fn complete() -> [Value; 4] {
    let item = json!({"id":"msg_after","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"steering accepted","annotations":[]}]});
    [
        json!({"type":"response.output_item.added","output_index":0,"item":item}),
        json!({"type":"response.output_text.delta","item_id":"msg_after","output_index":0,"content_index":0,"delta":"steering accepted"}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":{"id":"resp_codex_after","status":"completed","output":[item],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12},"end_turn":true}}),
    ]
}

fn http_upstream(
    listener: TcpListener,
    first: mpsc::Sender<()>,
    steered: mpsc::Receiver<()>,
) -> Vec<Value> {
    let mut bodies = Vec::new();
    for index in 0..2 {
        let mut socket = accept(&listener);
        bodies.push(receive_native_request(&mut socket).body);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let id = if index == 0 {
            "resp_codex_first"
        } else {
            "resp_codex_after"
        };
        writeln!(socket, "data: {}\n", json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}})).unwrap();
        if index == 0 {
            for event in partial() {
                writeln!(socket, "data: {event}\n").unwrap();
            }
            socket.flush().unwrap();
            first.send(()).unwrap();
            steered.recv_timeout(Duration::from_secs(15)).unwrap();
            // Ordinary Responses models drain the current response before steering.
            writeln!(socket, "data: {}\n", json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[],"end_turn":false}})).unwrap();
        } else {
            for event in complete() {
                writeln!(socket, "data: {event}\n").unwrap();
            }
        }
        socket.flush().unwrap();
    }
    bodies
}

fn native_upstream(listener: TcpListener, first: mpsc::Sender<()>) -> Vec<Value> {
    let mut socket = accept(&listener);
    let raw = read_request_head(&mut socket).unwrap();
    let request = parse_request(&raw.head).unwrap();
    let accept = websocket_accept(request.header("Sec-WebSocket-Key").unwrap()).unwrap();
    write!(socket, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
    socket.flush().unwrap();
    let mut ws = WebSocketConnection::new(&mut socket);
    let warmup = frame(&mut ws);
    assert_eq!(warmup["generate"], false);
    ws.send_json(&json!({"type":"response.completed","response":{"id":"resp_codex_warmup","status":"completed","output":[]}})).unwrap();
    let initial = frame(&mut ws);
    assert_eq!(initial["previous_response_id"], "resp_codex_warmup");
    ws.send_json(&json!({"type":"response.created","response":{"id":"resp_codex_first","status":"in_progress","output":[]}})).unwrap();
    for event in partial() {
        ws.send_json(&event).unwrap();
    }
    first.send(()).unwrap();
    let control = frame(&mut ws);
    assert_eq!(
        control,
        json!({"type":"response.interrupt","response_id":"resp_codex_first","mode":"discard_partial_items"})
    );
    ws.send_json(&json!({"type":"response.incomplete","response":{"id":"resp_codex_first","status":"incomplete","output":[],"incomplete_details":{"reason":"interrupted"},"end_turn":false}})).unwrap();
    let next = frame(&mut ws);
    for event in complete() {
        ws.send_json(&event).unwrap();
    }
    vec![initial, next]
}

fn frame(ws: &mut WebSocketConnection<'_, TcpStream>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "Codex upstream frame deadline");
        match ws.poll_text().unwrap() {
            emp_transport::WebSocketPoll::Text(text) => {
                return serde_json::from_str(&text).unwrap();
            }
            emp_transport::WebSocketPoll::Pending => {}
            other => panic!("unexpected upstream frame: {other:?}"),
        }
    }
}
