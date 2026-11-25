use super::modules::{BuiltModule, ModuleEmp, ModuleRelease, eventually};
use super::{Emp, Response, SseClient, Upstream, Workspace, http, io_timeout, write_file};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use emp_test_support::{Deadline, OwnedChild, test_timeout};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

pub const PASSWORD: &str = "team-e2e-password";
pub const SHARED_MODEL: &str = "team-shared-fixture-model";
const PERSONAL_MODEL: &str = "team-personal-fixture-model";

pub fn poll<T>(description: &str, mut observe: impl FnMut(&Deadline) -> Option<T>) -> T {
    let deadline = Deadline::after(test_timeout(Duration::from_secs(30)));
    eventually(&deadline, description, || observe(&deadline))
}

fn listener() -> TcpListener {
    loop {
        let socket = TcpListener::bind(("127.0.0.1", 0)).expect("Team loopback listener");
        if socket.local_addr().expect("Team address").port() > 4300 {
            return socket;
        }
    }
}

fn assert_reaped(pid: u32) {
    poll("Team process reaped", |deadline| {
        let mut command = Command::new("ps");
        command.args(["-A", "-o", "pid="]);
        let output = OwnedChild::output(command, *deadline).expect("process inventory");
        assert!(output.status.success(), "ps failed: {}", output.status);
        (!String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|found| found.parse::<u32>() == Ok(pid)))
        .then_some(())
    });
}

pub struct TeamServer {
    child: Option<OwnedChild>,
    directory: TempDir,
    pub port: u16,
}

impl TeamServer {
    fn start() -> Self {
        let socket = listener();
        let port = socket.local_addr().expect("Team address").port();
        drop(socket);
        let mut server = Self {
            directory: emp_test_support::fixture_dir().expect("Team server directory"),
            port,
            child: None,
        };
        server.restart();
        server
    }

    pub fn restart(&mut self) {
        assert!(self.child.is_none(), "Team server is already running");
        let binary = Path::new(env!("CARGO_BIN_EXE_EMP"))
            .parent()
            .expect("build directory")
            .join(format!("empmod-team{}", std::env::consts::EXE_SUFFIX));
        let mut command = Command::new(&binary);
        command
            .arg("serve")
            .arg("--state")
            .arg(self.directory.path())
            .arg("--listen")
            .arg(format!("127.0.0.1:{}", self.port))
            .env("HOME", self.directory.path())
            .env("EMP_TEAM_SPACE_PASSWORD", PASSWORD)
            .env("EMP_TEAM_ADMIN", "deployment")
            .env("EMP_TEAM_ADMIN_PASSWORD", PASSWORD)
            .env("EMP_TEAM_NAME", "M03 workspace")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        self.child = Some(
            OwnedChild::spawn(command)
                .unwrap_or_else(|error| panic!("{}: {error}", binary.display())),
        );
        let child = self.child.as_ref().expect("Team server");
        let deadline = Deadline::after(test_timeout(Duration::from_secs(30)));
        loop {
            let line = child
                .stderr_line(deadline)
                .expect("real Team server ready")
                .expect("Team server closed output before readiness");
            eprintln!("{line}");
            if line.starts_with("EMP workspace listening on ") {
                break;
            }
        }
        let health = http(self.port, "GET", "/healthz", &[], b"");
        assert_eq!(health.status, 200, "{}", health.text());
        assert_eq!(health.json(), json!({"ok":true,"mode":"workspace"}));
        eprintln!(
            "Team server PID={} sha256={:x} port={}",
            self.child.as_ref().expect("Team server").id(),
            Sha256::digest(std::fs::read(&binary).expect("Team binary")),
            self.port
        );
    }

    pub fn stop(&mut self) {
        let mut child = self.child.take().expect("running Team server");
        let pid = child.id();
        child.terminate().expect("stop and reap Team server");
        let deadline = Deadline::after(super::io_timeout());
        while let Some(line) = child.stderr_line(deadline).expect("Team stderr stopped") {
            eprintln!("{line}");
        }
        assert_reaped(pid);
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    pub events: Vec<Value>,
    pub inserted: u64,
}

pub struct TeamRelay {
    port: u16,
    stopping: Arc<AtomicBool>,
    drop_reply: Arc<AtomicBool>,
    hold_reply: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
    reports: Arc<Mutex<Vec<Report>>>,
    received: mpsc::Receiver<Report>,
    worker: Option<JoinHandle<()>>,
}

impl TeamRelay {
    fn start(server: u16, upstream: &Upstream) -> Self {
        let provider = url::Url::parse(&upstream.base_url())
            .expect("provider URL")
            .port()
            .expect("provider port");
        let socket = listener();
        let port = socket.local_addr().expect("relay address").port();
        let stopping = Arc::new(AtomicBool::new(false));
        let drop_reply = Arc::new(AtomicBool::new(false));
        let hold_reply = Arc::new(Mutex::new(None::<mpsc::Receiver<()>>));
        let reports = Arc::new(Mutex::new(Vec::new()));
        let (accepted, received) = mpsc::channel();
        let stopped = Arc::clone(&stopping);
        let discard = Arc::clone(&drop_reply);
        let hold = Arc::clone(&hold_reply);
        let observed = Arc::clone(&reports);
        let worker = thread::spawn(move || {
            thread::scope(|scope| {
                for stream in socket.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let stream = stream.expect("Team relay connection");
                    let discard = Arc::clone(&discard);
                    let hold = Arc::clone(&hold);
                    let observed = Arc::clone(&observed);
                    let accepted = accepted.clone();
                    scope.spawn(move || {
                        let deadline = Deadline::after(io_timeout());
                        let mut stream = deadline.io(stream);
                        let (line, headers, body) = super::http::read_message(&mut stream, false);
                        let method = line.split_whitespace().next().expect("Team HTTP method");
                        let path = line.split_whitespace().nth(1).expect("Team HTTP path");
                        if method == "GET" && path.split('?').next() == Some("/v1/models") {
                            let model = if headers.get("chatgpt-account-id").map(String::as_str)
                                == Some("m03-subscription")
                            {
                                SHARED_MODEL
                            } else {
                                PERSONAL_MODEL
                            };
                            let catalog = catalog(model).to_string();
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{catalog}",
                                catalog.len()
                            )
                            .expect("provider catalog reply");
                            return;
                        }
                        let destination = if path.starts_with("/v1/") {
                            provider
                        } else {
                            server
                        };
                        let Ok(backend) = TcpStream::connect(("127.0.0.1", destination)) else {
                            return;
                        };
                        let mut backend = Deadline::after(io_timeout()).io(backend);
                        write!(
                            backend,
                            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{destination}\r\n"
                        )
                        .expect("forward Team request");
                        for (name, value) in &headers {
                            if !matches!(name.as_str(), "host" | "connection" | "content-length") {
                                write!(backend, "{name}: {value}\r\n").expect("Team request header");
                            }
                        }
                        write!(
                            backend,
                            "Content-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .and_then(|()| backend.write_all(&body))
                        .expect("Team request body");
                        let deadline = Deadline::after(io_timeout());
                        backend.set_deadline(&deadline);
                        if destination == provider {
                            stream.set_deadline(&deadline);
                            let _ = std::io::copy(&mut backend, &mut stream);
                            return;
                        }
                        let (status, headers, reply) = super::http::read_message(&mut backend, true);
                        if path == "/workspace/v1/report"
                            && status.split_whitespace().nth(1) == Some("200")
                        {
                            let request: Value = serde_json::from_slice(&body).expect("report JSON");
                            let receipt: Value = serde_json::from_slice(&reply).expect("real receipt");
                            let report = Report {
                                events: request["events"]
                                    .as_array()
                                    .expect("report events")
                                    .clone(),
                                inserted: receipt["inserted"].as_u64().expect("inserted events"),
                            };
                            let gate = hold.lock().expect("report gate").take();
                            let lose_reply = discard.swap(false, Ordering::AcqRel);
                            observed.lock().expect("wire reports").push(report.clone());
                            let _ = accepted.send(report);
                            if let Some(gate) = gate {
                                let _ = gate.recv_timeout(io_timeout().duration());
                            }
                            if lose_reply {
                                return;
                            }
                        }
                        let mut head = format!("{status}\r\n");
                        for (name, value) in headers {
                            if !matches!(
                                name.as_str(),
                                "connection" | "content-length" | "transfer-encoding"
                            ) {
                                head.push_str(&format!("{name}: {value}\r\n"));
                            }
                        }
                        head.push_str(&format!(
                            "Content-Length: {}\r\nConnection: close\r\n\r\n",
                            reply.len()
                        ));
                        stream.set_deadline(&Deadline::after(io_timeout()));
                        let _ = stream
                            .write_all(head.as_bytes())
                            .and_then(|()| stream.write_all(&reply));
                    });
                }
            })
        });
        Self {
            port,
            stopping,
            drop_reply,
            hold_reply,
            reports,
            received,
            worker: Some(worker),
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn drop_next_report_reply(&self) {
        self.drop_reply.store(true, Ordering::Release);
    }

    pub fn hold_next_report_reply(&self) -> mpsc::Sender<()> {
        let (release, gate) = mpsc::channel();
        *self.hold_reply.lock().expect("report gate") = Some(gate);
        release
    }

    pub fn next_report(&self) -> Report {
        self.received
            .recv_timeout(test_timeout(Duration::from_secs(30)).duration())
            .expect("report reached real Team server")
    }

    pub fn assert_single_delivery(&self, id: &Value) {
        let reports = self.reports.lock().expect("wire reports");
        let count = reports
            .iter()
            .flat_map(|report| &report.events)
            .filter(|event| &event["id"] == id)
            .count();
        assert_eq!(count, 1, "duplicate writer delivered {id}: {reports:?}");
    }

    fn finish(mut self) -> thread::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> thread::Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        self.stopping.store(true, Ordering::Release);
        drop(
            self.hold_reply
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        worker.join()
    }
}

impl Drop for TeamRelay {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            let _ = writeln!(std::io::stderr(), "Team relay worker panicked");
        }
    }
}

pub fn team(emp: &Emp, action: &str, body: Value, expected: u16) -> Value {
    team_before(
        emp,
        action,
        body,
        expected,
        &Deadline::after(super::io_timeout()),
    )
}

fn team_before(emp: &Emp, action: &str, body: Value, expected: u16, deadline: &Deadline) -> Value {
    let response = emp.api_before(
        "POST",
        &format!("/api/team/{action}"),
        Some(&body),
        deadline,
    );
    assert_eq!(response.status, expected, "{action}: {}", response.text());
    response.json()
}

pub fn report(emp: &Emp, path: &str) -> Value {
    report_before(emp, path, &Deadline::after(super::io_timeout()))
}

pub fn report_before(emp: &Emp, path: &str, deadline: &Deadline) -> Value {
    let response = emp.get_before(path, deadline);
    assert_eq!(response.status, 200, "{path}: {}", response.text());
    response.json()
}

fn catalog(model: &str) -> Value {
    json!({"models":[{"slug":model,"display_name":"M03 fixture", "supported_in_api":true,
        "visibility":"list","context_window":256000}]})
}

pub struct Team {
    pub devices: Vec<ModuleEmp>,
    pub server: TeamServer,
    pub relay: TeamRelay,
    pub release: ModuleRelease,
    pub current: BuiltModule,
    pub subscription: Upstream,
    pub personal: Upstream,
    pub workspaces: Vec<Workspace>,
    pub manager: String,
    pub member: String,
    pub token: String,
}

impl Team {
    pub fn start() -> Self {
        let current = BuiltModule::current("team");
        let server = TeamServer::start();
        let subscription = Upstream::start();
        let relay = TeamRelay::start(server.port, &subscription);
        let release = ModuleRelease::start(std::slice::from_ref(&current));
        let personal = Upstream::start();
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + 86400;
        let claims = json!({"exp":expiry,"https://api.openai.com/auth":{
            "chatgpt_account_id":"m03-subscription","chatgpt_plan_type":"pro"}});
        let token = format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let mut fixture = Self {
            devices: Vec::new(),
            server,
            relay,
            release,
            current,
            subscription,
            personal,
            workspaces: Vec::new(),
            manager: String::new(),
            member: String::new(),
            token,
        };
        for index in 0..2 {
            let workspace = Workspace::new();
            workspace.write_config(json!({"codex_base_url":format!("{}/v1",fixture.relay.base()),
                "auto_enable_on_start":false,"account_store_path":workspace.root.join("accounts"),
                "providers":[{"id":"personal","name":"Personal API","base_url":fixture.personal.base_url(),
                    "protocol":"responses","auth_mode":"api_key","api_key":"m03-personal-key"}],
                "models":[{"id":"personal/model","provider":"personal","upstream_id":PERSONAL_MODEL,"enabled":true}]}));
            let model = if index == 0 {
                SHARED_MODEL
            } else {
                PERSONAL_MODEL
            };
            write_file(
                &workspace.root.join("native.json"),
                &catalog(model).to_string(),
            );
            if index == 0 {
                write_file(
                    &workspace.codex_home.join("auth.json"),
                    &json!({"tokens":{
                        "access_token":fixture.token,"id_token":fixture.token,
                        "account_id":"m03-subscription"}})
                    .to_string(),
                );
            }
            fixture.workspaces.push(workspace);
            let emp = fixture.release.start_emp(&fixture.workspaces[index]);
            emp.change("team", "install");
            emp.active(&fixture.current);
            fixture.devices.push(emp);
        }
        fixture.login(0, "deployment", "vps_admin");
        let invitation = team(&fixture.devices[0], "issue_invite", json!({}), 200);
        for name in ["manager", "researcher"] {
            let application = json!({"base":fixture.relay.base(),"code":invitation["code"],
                "name":name,"nickname":name,"password":PASSWORD});
            assert_eq!(
                team(&fixture.devices[1], "join", application.clone(), 200)["pending"],
                true
            );
            let denied = team(&fixture.devices[1], "login", application, 403);
            assert_eq!(denied["error"]["code"], "workspace_join_pending");
        }
        let members = team(&fixture.devices[0], "list_members", json!({}), 200);
        let id = |name: &str| {
            members["members"]
                .as_array()
                .expect("members")
                .iter()
                .find(|row| row["name"] == name)
                .expect("pending member")["id"]
                .as_str()
                .expect("member ID")
                .to_owned()
        };
        fixture.manager = id("manager");
        fixture.member = id("researcher");
        team(
            &fixture.devices[0],
            "member_access",
            json!({"id":fixture.manager,"role":"team_admin",
                "publish":true,"sync":true,"enabled":true}),
            200,
        );
        fixture.login(0, "manager", "team_admin");
        team(
            &fixture.devices[0],
            "member_access",
            json!({"id":fixture.member,"role":"member",
                "publish":false,"sync":true,"enabled":true}),
            200,
        );
        fixture.login(1, "researcher", "member");
        let ids: Vec<_> = fixture
            .workspaces
            .iter()
            .map(|workspace| {
                std::fs::read_to_string(workspace.root.join("state/team-device-id"))
                    .expect("device identity")
            })
            .collect();
        assert_ne!(ids[0], ids[1], "EMP devices must have distinct identities");
        fixture
    }

    pub fn login(&self, device: usize, name: &str, role: &str) {
        let status = team(
            &self.devices[device],
            "login",
            json!({"base":self.relay.base(),"name":name,"password":PASSWORD}),
            200,
        );
        assert_eq!(status["joined"], true);
        assert_eq!(status["identity"]["member"]["role"], role);
    }

    pub fn share(&self) -> String {
        let published = team(&self.devices[0], "publish", json!({}), 200);
        assert_eq!(published["review_required"], true);
        assert_eq!(team(&self.devices[1], "sync", json!({}), 200)["synced"], 0);
        let review = json!({"id":published["id"],"version":published["version"],"shared":true});
        team(&self.devices[1], "review_service", review.clone(), 403);
        assert_eq!(
            team(&self.devices[0], "review_service", review, 200)["shared"],
            true
        );
        assert_eq!(team(&self.devices[1], "sync", json!({}), 200)["synced"], 1);
        assert_eq!(team(&self.devices[1], "sync", json!({}), 200)["synced"], 0);
        let id = format!("team-{}", published["id"].as_str().expect("share ID"));
        self.subscription.assert_idle();
        let config = report(&self.devices[1], "/api/config");
        let account = config["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .find(|row| row["id"] == id)
            .expect("shared account");
        let prefix = account["prefix"].as_str().expect("shared route prefix");
        let route = format!("{prefix}/{SHARED_MODEL}");
        let auth = json!({"tokens":{
            "access_token":self.token,"account_id":"m03-subscription"
        }});
        let headers = emp_codex::account_auth_headers(&auth).expect("catalog owner headers");
        let mut cached = catalog(SHARED_MODEL);
        cached["account_owner"] = json!(emp_codex::native_catalog_owner(&headers));
        cached["base_url"] = json!(format!("{}/v1", self.relay.base()));
        write_file(
            &self.workspaces[1]
                .root
                .join("accounts")
                .join(id)
                .join("models_cache.json"),
            &cached.to_string(),
        );
        self.assert_shared_model_listed(true);
        route
    }

    pub fn assert_shared_model_listed(&self, expected: bool) {
        let model = format!("manager/{SHARED_MODEL}");
        let listed = self.devices[1].codex("GET", "/v1/models", None);
        assert_eq!(listed.status, 200, "{}", listed.text());
        assert_eq!(
            listed.json()["data"]
                .as_array()
                .expect("models")
                .iter()
                .any(|row| row["id"] == model),
            expected,
            "shared subscription model {model} visibility: {}",
            listed.text()
        );
    }

    pub fn call(&self, device: usize, route: &str, id: &str, upstream: &Upstream) -> String {
        let model = if route == "personal/model" {
            PERSONAL_MODEL
        } else {
            SHARED_MODEL
        };
        upstream.reply_json(&json!({"id":id,"object":"response",
            "status":"completed","model":model,"output":[],
            "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}));
        let token = if device == 0 {
            &self.token
        } else {
            super::NATIVE_TOKEN
        };
        let bearer = format!("Bearer {token}");
        let response = http(
            self.devices[device].port,
            "POST",
            "/v1/responses",
            &[
                ("Authorization", &bearer),
                ("Content-Type", "application/json"),
            ],
            json!({"model":route,"input":"M03 request","metadata":{"session_id":id}})
                .to_string()
                .as_bytes(),
        );
        assert_eq!(response.status, 200, "{}", response.text());
        assert_eq!(response.json()["id"], id);
        let forwarded = upstream.next_forwarded();
        assert_eq!(forwarded.path, "/v1/responses");
        assert_eq!(forwarded.body.expect("forwarded JSON")["model"], model);
        assert_eq!(
            forwarded.headers["authorization"],
            if route == "personal/model" {
                "Bearer m03-personal-key".to_owned()
            } else {
                format!("Bearer {}", self.token)
            }
        );
        if route != "personal/model" {
            assert_eq!(forwarded.headers["chatgpt-account-id"], "m03-subscription");
        }
        upstream.assert_idle();
        forwarded.headers["x-emp-request-id"].clone()
    }

    pub fn stream(&self, route: &str, id: &str) -> (SseClient, super::upstream::StreamGate) {
        self.subscription.assert_idle();
        let first = [
            json!({"type":"response.created","response":{"id":id,"object":"response",
                "status":"in_progress","model":SHARED_MODEL,"output":[]}}),
            json!({"type":"response.output_text.delta","item_id":"msg_team",
                "output_index":0,"content_index":0,"delta":"M03 visible"}),
        ];
        let tail = json!({"type":"response.completed","response":{"id":id,"object":"response",
            "status":"completed","model":SHARED_MODEL,"output":[{
                "id":"msg_team","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"M03 visible","annotations":[]}]}],
            "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}});
        let gate = self.subscription.gate_stream(
            first
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect(),
            Some(format!("data: {tail}\n\n")),
        );
        let mut client = SseClient::responses(
            self.devices[1].port,
            &json!({"model":route,"input":"M03 gated request","stream":true,
                "metadata":{"session_id":id}}),
        );
        let forwarded = self.subscription.next_forwarded();
        assert_eq!(
            forwarded.headers["authorization"],
            format!("Bearer {}", self.token)
        );
        assert_eq!(
            next_event(&mut client, "response.output_text.delta")["delta"],
            "M03 visible"
        );
        (client, gate)
    }

    pub fn usage(&self, member: &str, requests: u64) -> Value {
        poll("Team ledger count", |deadline| {
            let usage = team_before(
                &self.devices[0],
                "usage",
                json!({"period":"lifetime"}),
                200,
                deadline,
            );
            let row = usage["rows"]
                .as_array()
                .expect("Team usage")
                .iter()
                .find(|row| row["member"] == member)
                .expect("member usage");
            let count = row["requests"].as_u64().expect("request count");
            assert!(count <= requests, "duplicate Team billing: {row}");
            (count == requests).then(|| row.clone())
        })
    }

    pub fn local_call(&self, device: usize, request: &str) -> Value {
        poll("local call ledger", |deadline| {
            let calls = report_before(
                &self.devices[device],
                "/api/calls?start=0&end=4102444800",
                deadline,
            );
            calls["records"]
                .as_array()
                .expect("calls")
                .iter()
                .find(|row| row["request_id"] == request && row["input_tokens"] == 7)
                .cloned()
        })
    }

    pub fn stop(mut self) {
        for device in self.devices.drain(..) {
            device.stop();
        }
        let relay = self.relay.finish();
        let release = self.release.finish();
        self.server.stop();
        relay.expect("Team relay worker panicked");
        release.expect("module release server worker panicked");
    }
}

pub fn next_event(client: &mut SseClient, kind: &str) -> Value {
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    loop {
        let event = client
            .next_event(&deadline)
            .expect("Team SSE event")
            .expect("Team SSE ended");
        if event["type"] == kind {
            return event;
        }
        assert!(
            !matches!(
                event["type"].as_str(),
                Some("error" | "response.failed" | "response.incomplete")
            ),
            "{event}"
        );
    }
}

pub fn denied(response: Response, status: u16, code: &str) {
    assert_eq!(response.status, status, "{}", response.text());
    assert_eq!(response.json()["error"]["code"], code);
}
