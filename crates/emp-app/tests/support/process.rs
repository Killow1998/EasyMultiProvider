use emp_test_support::test_timeout;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::{NATIVE_TOKEN, ORIGINAL_CODEX_CONFIG, PROVIDER_KEY, Response, Upstream, http};

pub fn emp_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_EMP"));
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "EASY_MULTI_PROVIDER_CONFIG",
        "EASY_MULTI_PROVIDER_MASTER_KEY_FILE",
        "EASY_MULTI_PROVIDER_MASTER_KEY",
        "EMP_OFFICIAL_MODELS_PATH",
        "EMP_UPDATE_READY",
        "EMP_UPDATE_RESULT",
        "NVM_DIR",
        "BROWSER",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        command.env_remove(key);
    }
    // Any accidental real-network access fails fast against a closed port.
    command
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("TZ", "UTC");
    command
}

/// A temporary user: CODEX_HOME with a synthetic login and user TOML, plus
/// an EMP configuration directory.
pub struct Workspace {
    _directory: TempDir,
    pub root: PathBuf,
    pub codex_home: PathBuf,
    pub config_path: PathBuf,
}

impl Workspace {
    pub fn new() -> Self {
        // Short /tmp paths keep nested AF_UNIX control sockets within limits.
        let directory = tempfile::Builder::new()
            .prefix("emp-j-")
            .tempdir_in(std::env::temp_dir())
            .expect("temporary root");
        let root = directory.path().canonicalize().expect("canonical root");
        let codex_home = root.join("codex");
        std::fs::create_dir_all(codex_home.join("sessions")).expect("CODEX_HOME");
        std::fs::create_dir_all(root.join("home")).expect("HOME");
        std::fs::write(
            codex_home.join("auth.json"),
            json!({"tokens": {"access_token": NATIVE_TOKEN, "account_id": "fixture-account"}})
                .to_string(),
        )
        .expect("auth.json");
        std::fs::write(codex_home.join("config.toml"), ORIGINAL_CODEX_CONFIG).expect("TOML");
        std::fs::write(root.join("native.json"), r#"{"models":[]}"#).expect("native catalog");
        let config_path = root.join("config.json");
        Self {
            _directory: directory,
            root,
            codex_home,
            config_path,
        }
    }

    /// Four routes, one per upstream protocol, all pointing at `upstream`.
    pub fn with_routes(upstream: &Upstream) -> Self {
        let workspace = Self::new();
        let providers: Vec<Value> = [
            ("test", "chat_completions"),
            ("anthropic", "anthropic_messages"),
            ("responses", "responses"),
            ("native", "responses"),
        ]
        .iter()
        .map(|(id, protocol)| {
            if *id == "native" {
                json!({"id": id, "base_url": upstream.base_url(), "protocol": protocol,
                       "auth_mode": "forward"})
            } else {
                json!({"id": id, "base_url": upstream.base_url(), "protocol": protocol,
                       "auth_mode": "api_key", "api_key": PROVIDER_KEY})
            }
        })
        .collect();
        let models: Vec<Value> = ["test", "anthropic", "responses", "native"]
            .iter()
            .map(|id| {
                json!({"id": format!("{id}/model"), "provider": id,
                       "upstream_id": "upstream-model", "context_window": 256000,
                       "enabled": true})
            })
            .collect();
        workspace.write_config(json!({"providers": providers, "models": models}));
        workspace
    }

    /// Write an EMP configuration; `native_catalog_path` is always filled in.
    pub fn write_config(&self, mut config: Value) {
        config["native_catalog_path"] = json!(self.root.join("native.json"));
        std::fs::write(&self.config_path, config.to_string()).expect("EMP config");
    }

    pub fn codex_config(&self) -> PathBuf {
        self.codex_home.join("config.toml")
    }

    pub fn read_codex_config(&self) -> String {
        std::fs::read_to_string(self.codex_config()).expect("read Codex TOML")
    }

    pub fn integration_dir(&self) -> PathBuf {
        self.codex_home.join("easy-multi-provider/integration")
    }

    /// Environment for any EMP process acting on this workspace.
    pub fn command(&self) -> Command {
        let mut command = emp_command();
        command
            .env("CODEX_HOME", &self.codex_home)
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("APPDATA", self.root.join("appdata"))
            .env("LOCALAPPDATA", self.root.join("local-appdata"))
            // Outside the config directory, so the config counts as "custom".
            .current_dir(self.root.join("home"));
        command
    }

    pub fn serve_arguments(&self) -> Vec<String> {
        vec![
            "serve".into(),
            "--config".into(),
            self.config_path.display().to_string(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            "0".into(),
        ]
    }

    pub fn start(&self) -> Emp {
        let mut command = self.command();
        command.args(self.serve_arguments());
        Emp::spawn(command)
    }

    pub fn start_without_path_cli(&self) -> Emp {
        let mut command = self.command();
        command
            .env("PATH", self.root.join("empty-bin"))
            .env_remove("NVM_DIR")
            .args(self.serve_arguments());
        Emp::spawn(command)
    }
}

/// A running EMP service with an authenticated management session.
pub struct Emp {
    child: Child,
    pub port: u16,
    pub session: String,
    pub bootstrap: String,
    pub startup_output: Vec<String>,
}

impl Emp {
    pub fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start EMP");
        let stdout = child.stdout.take().expect("EMP stdout");
        let stderr = child.stderr.take().expect("EMP stderr");
        let (lines, received) = mpsc::channel::<String>();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = lines.send(line);
            }
        });
        let errors = Arc::new(Mutex::new(String::new()));
        let collected = Arc::clone(&errors);
        thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            collected.lock().expect("stderr").push_str(&text);
        });
        let mut startup_output = Vec::new();
        let opened = loop {
            match received.recv_timeout(test_timeout(Duration::from_secs(30))) {
                Ok(line) => {
                    let opened = line.strip_prefix("Open in browser: ").map(str::to_owned);
                    startup_output.push(line);
                    if let Some(url) = opened {
                        break url;
                    }
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "启动失败后保留既有间隔，让 stderr 收集线程完成诊断输出"
                    )]
                    thread::sleep(Duration::from_millis(50));
                    panic!(
                        "EMP not ready ({error}); stdout: {startup_output:?}; stderr: {}",
                        errors.lock().expect("stderr")
                    );
                }
            }
        };
        // Keep draining stdout so the service never blocks on a full pipe.
        thread::spawn(move || while received.recv().is_ok() {});
        let (origin, query) = opened
            .split_once("/?bootstrap=")
            .unwrap_or_else(|| panic!("bootstrap URL: {opened}"));
        let port = origin
            .strip_prefix("http://127.0.0.1:")
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("loopback URL: {opened}"));
        let bootstrap = query.to_owned();
        let exchanged = http(
            port,
            "POST",
            "/api/session",
            &[("X-EMP-Bootstrap", &bootstrap)],
            b"",
        );
        assert_eq!(exchanged.status, 200, "{}", exchanged.text());
        let session = exchanged.json()["session"]
            .as_str()
            .expect("session token")
            .to_owned();
        let emp = Self {
            child,
            port,
            session,
            bootstrap,
            startup_output,
        };
        // `--port 0` is a test-only listener choice; saving settings through
        // the page requires the real port, as a user's config always has.
        let mut config = emp.get("/api/config").json();
        if config["port"] == 0 {
            config["port"] = json!(port);
            let saved = emp.post("/api/config", &config);
            assert_eq!(saved.status, 200, "{}", saved.text());
        }
        emp
    }

    pub fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).expect("pid")
    }

    /// Management request carrying the browser session.
    pub fn api(&self, method: &str, path: &str, body: Option<&Value>) -> Response {
        let payload = body.map(Value::to_string).unwrap_or_default();
        let mut headers = vec![("X-EMP-Session", self.session.as_str())];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        http(self.port, method, path, &headers, payload.as_bytes())
    }

    pub fn get(&self, path: &str) -> Response {
        self.api("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Response {
        self.api("POST", path, Some(body))
    }

    /// Unauthenticated request (no session, no bearer token).
    pub fn anonymous(&self, method: &str, path: &str) -> Response {
        http(self.port, method, path, &[], b"")
    }

    /// Codex-style model request: the native login token as bearer.
    pub fn codex(&self, method: &str, path: &str, body: Option<&Value>) -> Response {
        let payload = body.map(Value::to_string).unwrap_or_default();
        let bearer = format!("Bearer {NATIVE_TOKEN}");
        let mut headers = vec![("Authorization", bearer.as_str())];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        http(self.port, method, path, &headers, payload.as_bytes())
    }

    pub fn responses(&self, body: &Value) -> Response {
        self.codex("POST", "/v1/responses", Some(body))
    }

    #[cfg(unix)]
    pub fn signal(&self, signal: i32) {
        // SAFETY: kill(2) on our own live child process id.
        let result = unsafe { libc::kill(self.pid(), signal) };
        assert_eq!(result, 0, "signal EMP");
    }

    /// Wait for the process to exit and return its exit code.
    pub fn wait_exit(&mut self) -> Option<i32> {
        self.wait_exit_with_timeout(test_timeout(Duration::from_secs(8)))
    }

    pub fn wait_exit_with_timeout(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll EMP") {
                return status.code();
            }
            #[expect(
                clippy::disallowed_methods,
                reason = "子进程退出需保留取消或超时检查，按既有固定间隔轮询"
            )]
            thread::sleep(Duration::from_millis(10));
        }
        panic!("EMP did not exit within {timeout:?}");
    }

    /// Graceful stop (SIGTERM), as a user closing the service would do.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        #[cfg(unix)]
        self.signal(libc::SIGTERM);
        #[cfg(windows)]
        let _ = self.post("/api/quit", &json!({}));
        for _ in 0..500 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            #[expect(
                clippy::disallowed_methods,
                reason = "子进程退出需保留取消或超时检查，按既有固定间隔轮询"
            )]
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Emp {
    fn drop(&mut self) {
        self.shutdown();
    }
}
