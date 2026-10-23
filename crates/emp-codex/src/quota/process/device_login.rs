//! Official device-code RPC in an isolated credential home. No model requests.
use super::*;
use std::sync::{Arc, Mutex};

pub struct DeviceLogin {
    cancelled: Arc<AtomicBool>,
    public: Arc<Mutex<Value>>,
    result: Arc<Mutex<Option<Result<Value, &'static str>>>>,
    worker: Option<JoinHandle<()>>,
}

impl DeviceLogin {
    pub fn start(binary: &str) -> Result<Self, QuotaError> {
        let binary = TrustedBinary::resolve(binary)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let public = Arc::new(Mutex::new(json!({"status":"starting"})));
        let result = Arc::new(Mutex::new(None));
        let cancel = cancelled.clone();
        let snapshot = public.clone();
        let completed = result.clone();
        let worker = thread::spawn(move || {
            let outcome = run(&binary, &cancel, &snapshot);
            *completed.lock().unwrap() = Some(outcome);
        });
        Ok(Self {
            cancelled,
            public,
            result,
            worker: Some(worker),
        })
    }

    pub fn snapshot(&self) -> Value {
        self.public.lock().unwrap().clone()
    }
    pub fn take_result(&self) -> Option<Result<Value, &'static str>> {
        self.result.lock().unwrap().take()
    }
}

impl Drop for DeviceLogin {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run(
    binary: &TrustedBinary,
    cancel: &AtomicBool,
    public: &Mutex<Value>,
) -> Result<Value, &'static str> {
    let dir = tempfile::Builder::new()
        .prefix("easy-mp-codex-login-")
        .tempdir()
        .map_err(|_| "login_storage_failed")?;
    set_private_directory(dir.path()).map_err(|_| "login_storage_failed")?;
    write_private(
        &dir.path().join("quota-models.json"),
        include_bytes!("quota-models.json"),
    )
    .map_err(|_| "login_storage_failed")?;
    write_private(&dir.path().join("config.toml"), b"cli_auth_credentials_store = \"file\"\nmodel_catalog_json = \"quota-models.json\"\n[features]\nplugins = false\n").map_err(|_| "login_storage_failed")?;
    let mut command =
        app_server_command(binary, dir.path()).map_err(|_| "login_engine_unavailable")?;
    // Login errors may contain sensitive upstream details; expose only mapped codes.
    command.stderr(Stdio::null());
    let mut child = QuotaChild::spawn(&mut command).map_err(|_| "login_engine_unavailable")?;
    let stop = Arc::new(AtomicBool::new(false));
    let stdout = CancellablePipe::new(
        child.process.stdout.take().ok_or("login_protocol_error")?,
        stop.clone(),
    )
    .map_err(|_| "login_protocol_error")?;
    let mut stdin = child.process.stdin.take().ok_or("login_protocol_error")?;
    let (receiver, reader) = stdout_reader(stdout);
    let result = conversation(&mut stdin, &receiver, cancel, public);
    stop.store(true, Ordering::Release);
    drop(stdin);
    drop(child);
    let _ = reader.join();
    result?;
    let path = dir.path().join("auth.json");
    let metadata = fs::symlink_metadata(&path).map_err(|_| "login_credentials_missing")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_AUTH_BYTES {
        return Err("login_credentials_invalid");
    }
    let auth: Value =
        serde_json::from_slice(&fs::read(path).map_err(|_| "login_credentials_missing")?)
            .map_err(|_| "login_credentials_invalid")?;
    emp_state::validate_auth_json(&auth).map_err(|_| "login_credentials_invalid")
}

fn send(stdin: &mut impl Write, message: Value) -> Result<(), &'static str> {
    serde_json::to_writer(&mut *stdin, &message).map_err(|_| "login_protocol_error")?;
    stdin
        .write_all(b"\n")
        .and_then(|()| stdin.flush())
        .map_err(|_| "login_protocol_error")
}

fn conversation(
    stdin: &mut impl Write,
    receiver: &Receiver<ProcessLine>,
    cancel: &AtomicBool,
    public: &Mutex<Value>,
) -> Result<(), &'static str> {
    send(
        stdin,
        json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"easy-multi-provider","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}}),
    )?;
    let mut deadline = Instant::now() + Duration::from_secs(45);
    let mut login_id = None;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("login_cancelled");
        }
        if Instant::now() >= deadline {
            return Err("login_expired");
        }
        let line = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(ProcessLine::Line(line)) => line,
            Err(RecvTimeoutError::Timeout) => continue,
            _ => return Err("login_protocol_error"),
        };
        let value: Value = serde_json::from_str(&line).map_err(|_| "login_protocol_error")?;
        if value["id"] == 1 {
            if value.get("error").is_some() {
                return Err("login_engine_unavailable");
            }
            send(stdin, json!({"method":"initialized"}))?;
            send(
                stdin,
                json!({"id":2,"method":"account/login/start","params":{"type":"chatgptDeviceCode"}}),
            )?;
        } else if value["id"] == 2 {
            if value.get("error").is_some() {
                return Err("login_device_unavailable");
            }
            let response = &value["result"];
            let url = response["verificationUrl"]
                .as_str()
                .ok_or("login_protocol_error")?;
            // Only the official origin may be offered as a sign-in destination.
            if url != "https://auth.openai.com/codex/device" {
                return Err("login_protocol_error");
            }
            let code = response["userCode"]
                .as_str()
                .filter(|code| !code.is_empty() && code.len() <= 64)
                .ok_or("login_protocol_error")?;
            login_id = Some(
                response["loginId"]
                    .as_str()
                    .ok_or("login_protocol_error")?
                    .to_owned(),
            );
            deadline = Instant::now() + Duration::from_secs(15 * 60);
            *public.lock().unwrap() = json!({"status":"pending","verification_url":url,"user_code":code,"expires_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()+900});
        } else if value["method"] == "account/login/completed"
            && login_id
                .as_deref()
                .is_some_and(|id| value["params"]["loginId"] == id)
        {
            return if value["params"]["success"] == true {
                Ok(())
            } else {
                Err("login_failed")
            };
        }
    }
}
