use super::{Emp, IO_TIMEOUT, Workspace, http::read_message};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const IDS: [&str; 3] = ["decision", "gateway", "aggregate"];

#[derive(Clone)]
pub struct BuiltModule {
    pub id: String,
    pub version: String,
    pub digest: String,
    host_api: u64,
    bytes: Arc<Vec<u8>>,
}

impl BuiltModule {
    pub fn current(id: &str) -> Self {
        Self::read(
            Path::new(env!("CARGO_BIN_EXE_EMP"))
                .parent()
                .expect("EMP build directory"),
            id,
        )
    }

    pub fn next(id: &str) -> Self {
        let directory = std::env::var_os("EMP_E2E_MODULE_NEXT_DIR").expect(
            "M01 requires higher-version real module binaries: build emp-mod-decision, \
             emp-mod-gateway and emp-mod-aggregate in a separate versioned workspace, \
             then set EMP_E2E_MODULE_NEXT_DIR to its target/debug (or release) directory",
        );
        Self::read(Path::new(&directory), id)
    }

    fn read(directory: &Path, id: &str) -> Self {
        let binary = directory.join(format!("empmod-{id}{}", std::env::consts::EXE_SUFFIX));
        Self::from_binary(&binary, id)
    }

    pub fn from_binary(binary: &Path, id: &str) -> Self {
        let bytes = std::fs::read(binary).unwrap_or_else(|error| {
            panic!(
                "missing real module binary {}: {error}; first run cargo build --locked \
                 -p emp-mod-decision -p emp-mod-gateway -p emp-mod-aggregate --bins \
                 using the same target directory and profile as EMP",
                binary.display()
            )
        });
        let output = Command::new(binary)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("{}: {error}", binary.display()));
        assert!(
            output.status.success(),
            "{}: {:?}",
            binary.display(),
            output
        );
        let hello: Value = serde_json::from_slice(&output.stdout).expect("real module handshake");
        assert_eq!(hello["module"], id, "{}: {hello}", binary.display());
        assert_eq!(hello["protocol"], 1, "{hello}");
        Self {
            id: id.into(),
            version: hello["module_version"]
                .as_str()
                .filter(|version| !version.is_empty())
                .expect("module handshake version")
                .into(),
            host_api: hello["host_api"].as_u64().expect("module host API"),
            digest: format!("{:x}", Sha256::digest(&bytes)),
            bytes: Arc::new(bytes),
        }
    }

    fn asset_name(&self) -> String {
        format!(
            "EMP-mod-{}-{}-{}{}",
            self.id,
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::EXE_SUFFIX
        )
    }

    fn tag(&self) -> String {
        format!("emp-mod-{}-api{}-v{}", self.id, self.host_api, self.version)
    }
}

#[derive(Clone, Copy)]
pub enum Fault {
    None,
    Digest,
    Executable,
}

struct Asset {
    module: BuiltModule,
    fault: Fault,
}

impl Asset {
    fn bytes(&self) -> &[u8] {
        match self.fault {
            Fault::Executable => b"damaged module executable\n",
            _ => &self.module.bytes,
        }
    }

    fn metadata(&self, base: &str, tag: &str) -> Value {
        let name = self.module.asset_name();
        let digest = match self.fault {
            Fault::Digest => "0".repeat(64),
            _ => format!("{:x}", Sha256::digest(self.bytes())),
        };
        json!({"name":name, "size":self.bytes().len(), "digest":format!("sha256:{digest}"),
            "browser_download_url":format!("{base}/releases/download/{tag}/{name}")})
    }
}

struct Releases {
    entries: BTreeMap<String, Vec<Asset>>,
    downloads: Vec<String>,
}

pub struct ModuleRelease {
    port: u16,
    state: Arc<Mutex<Releases>>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ModuleRelease {
    pub fn start(modules: &[BuiltModule]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind module release");
        let port = listener.local_addr().expect("release address").port();
        let state = Arc::new(Mutex::new(Releases {
            entries: BTreeMap::from([(
                format!("v{}", env!("CARGO_PKG_VERSION")),
                modules
                    .iter()
                    .cloned()
                    .map(|module| Asset {
                        module,
                        fault: Fault::None,
                    })
                    .collect(),
            )]),
            downloads: Vec::new(),
        }));
        let stopping = Arc::new(AtomicBool::new(false));
        let shared = Arc::clone(&state);
        let stopped = Arc::clone(&stopping);
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let mut stream = stream.expect("accept release request");
                stream
                    .set_read_timeout(Some(IO_TIMEOUT))
                    .expect("release read timeout");
                stream
                    .set_write_timeout(Some(IO_TIMEOUT))
                    .expect("release write timeout");
                let (line, _, _) = read_message(&mut stream, false);
                let path = line.split_whitespace().nth(1).expect("release path");
                let base = format!("http://127.0.0.1:{port}");
                let mut state = shared.lock().expect("release state");
                let metadata = |tag: &str, assets: &[Asset]| {
                    json!({"tag_name":tag, "draft":false, "prerelease":tag.contains("-beta"),
                        "body":"Local real module build", "assets":assets.iter()
                            .map(|asset| asset.metadata(&base, tag)).collect::<Vec<_>>()})
                };
                let core_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
                let mut next_page = false;
                let body = if path.split('?').next() == Some("/releases") {
                    let second = path
                        .split('?')
                        .nth(1)
                        .is_some_and(|query| query.split('&').any(|pair| pair == "page=2"));
                    next_page = !second && state.entries.len() > 1;
                    Some(
                        serde_json::to_vec(
                            &state
                                .entries
                                .iter()
                                .filter(|(tag, _)| (tag.as_str() == core_tag) != second)
                                .map(|(tag, assets)| metadata(tag, assets))
                                .collect::<Vec<_>>(),
                        )
                        .expect("release list"),
                    )
                } else if let Some(tag) = path.strip_prefix("/releases/tags/") {
                    state.entries.get(tag).map(|assets| {
                        serde_json::to_vec(&metadata(tag, assets)).expect("release metadata")
                    })
                } else if let Some(download) = path.strip_prefix("/releases/download/") {
                    state.downloads.push(download.into());
                    download.split_once('/').and_then(|(tag, name)| {
                        state
                            .entries
                            .get(tag)?
                            .iter()
                            .find(|asset| asset.module.asset_name() == name)
                            .map(|asset| asset.bytes().to_vec())
                    })
                } else {
                    None
                };
                drop(state);
                let (status, body) = body.map_or((404, Vec::new()), |body| (200, body));
                let link = if next_page {
                    format!("Link: <{base}/releases?per_page=100&page=2>; rel=\"next\"\r\n")
                } else {
                    String::new()
                };
                let head = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\n{link}Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream
                    .write_all(head.as_bytes())
                    .and_then(|()| stream.write_all(&body));
            }
        });
        Self {
            port,
            state,
            stopping,
            worker: Some(worker),
        }
    }

    pub fn publish(&self, module: &BuiltModule, fault: Fault) {
        self.state.lock().expect("release state").entries.insert(
            module.tag(),
            vec![Asset {
                module: module.clone(),
                fault,
            }],
        );
    }

    pub fn downloads(&self) -> usize {
        self.state.lock().expect("release state").downloads.len()
    }

    pub fn start_emp(&self, workspace: &Workspace) -> ModuleEmp {
        if !cfg!(debug_assertions) {
            panic!(
                "module E2E requires debug EMP: EMP_UPDATE_TEST_* is unavailable in release builds; run cargo test without --release"
            );
        }
        let mut command = workspace.command();
        let base = format!("http://127.0.0.1:{}", self.port);
        command
            .env("EMP_UPDATE_TEST_REPOSITORY_URL", &base)
            .env(
                "EMP_UPDATE_TEST_API_URL",
                format!("{base}/releases?per_page=100"),
            )
            .args(workspace.serve_arguments());
        let emp = Emp::spawn(command);
        eprintln!(
            "Core PID={} sha256={:x}",
            emp.pid(),
            Sha256::digest(std::fs::read(env!("CARGO_BIN_EXE_EMP")).expect("EMP binary"))
        );
        ModuleEmp {
            emp: Some(emp),
            root: workspace.root.join("state/modules"),
            observed: Mutex::new(BTreeSet::new()),
        }
    }
}

impl Drop for ModuleRelease {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(worker) = self.worker.take() {
            worker.join().expect("release server stopped");
        }
    }
}

pub struct ModuleEmp {
    emp: Option<Emp>,
    root: PathBuf,
    observed: Mutex<BTreeSet<i32>>,
}

impl Deref for ModuleEmp {
    type Target = Emp;

    fn deref(&self) -> &Emp {
        self.emp.as_ref().expect("running EMP")
    }
}

fn processes() -> Vec<(i32, String)> {
    let output = Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=", "-o", "args="])
        .output()
        .expect("ps is required to check real module processes");
    assert!(output.status.success(), "ps failed: {output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (pid, args) = line.trim().split_once(char::is_whitespace).expect("ps row");
            (pid.parse().expect("process ID"), args.trim().to_owned())
        })
        .collect()
}

impl ModuleEmp {
    pub fn status(&self, id: &str) -> Value {
        let response = self.get("/api/modules");
        assert_eq!(response.status, 200, "{}", response.text());
        response.json()["modules"]
            .as_array()
            .expect("module status")
            .iter()
            .find(|row| row["id"] == id)
            .expect("module status row")
            .clone()
    }

    pub fn change(&self, id: &str, action: &str) {
        let response = self.post("/api/modules", &json!({"id":id,"action":action}));
        assert_eq!(response.status, 200, "{id}/{action}: {}", response.text());
    }

    pub fn active(&self, module: &BuiltModule) -> i32 {
        let workers = self.active_processes(module);
        assert_eq!(workers.len(), 1, "expected one real {} worker", module.id);
        workers[0]
    }

    pub fn active_processes(&self, module: &BuiltModule) -> Vec<i32> {
        let row = self.status(&module.id);
        assert_eq!(row["enabled"], true, "{row}");
        assert_eq!(row["active"], true, "{row}");
        assert_eq!(row["installed_version"], module.version, "{row}");
        assert_eq!(row["active_version"], module.version, "{row}");
        assert_eq!(row["active_revision"], module.digest, "{row}");
        assert_eq!(row["restart_required"], false, "{row}");
        assert!(row["error"].is_null(), "{row}");
        let binary = self.root.join(format!(
            "{}-{}{}",
            module.id,
            module.digest,
            std::env::consts::EXE_SUFFIX
        ));
        let snapshot = processes();
        assert!(
            snapshot.iter().any(|(pid, args)| {
                *pid == self.pid() && args.starts_with(env!("CARGO_BIN_EXE_EMP"))
            }),
            "original Core process exited"
        );
        let executable = binary.to_string_lossy();
        let workers: Vec<_> = snapshot
            .into_iter()
            .filter(|(_, args)| args.as_str() == executable.as_ref())
            .collect();
        assert!(!workers.is_empty(), "missing real {} worker", module.id);
        workers
            .into_iter()
            .map(|(pid, _)| {
                self.observed.lock().expect("observed workers").insert(pid);
                eprintln!(
                    "Core PID={} module={} PID={pid} version={} sha256={}",
                    self.pid(),
                    module.id,
                    module.version,
                    module.digest
                );
                pid
            })
            .collect()
    }

    pub fn assert_gone(&self, pid: i32) {
        eventually(Duration::from_secs(5), "module PID was not reaped", || {
            (!processes().iter().any(|(found, _)| *found == pid)).then_some(())
        });
    }

    pub fn stop(mut self) {
        self.emp.take().expect("running EMP").stop();
        let directory = self.root.to_string_lossy();
        eventually(Duration::from_secs(5), "orphan module processes", || {
            let observed = self.observed.lock().expect("observed workers");
            let remaining = processes()
                .into_iter()
                .any(|(pid, args)| observed.contains(&pid) || args.starts_with(directory.as_ref()));
            (!remaining).then_some(())
        });
    }
}

/// Polls `probe` every 20 ms until it yields a value, panicking with `what` after `timeout`.
pub fn eventually<T>(timeout: Duration, what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what} within {timeout:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

impl Drop for ModuleEmp {
    fn drop(&mut self) {
        if let Some(emp) = self.emp.take() {
            emp.stop();
        }
        let directory = self.root.to_string_lossy();
        for (pid, args) in processes() {
            if args.starts_with(directory.as_ref()) {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    }
}
