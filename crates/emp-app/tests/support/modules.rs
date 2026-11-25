use super::{Emp, Workspace, http::read_message, io_timeout};
use emp_test_support::{Deadline, OwnedChild, TestBudget, test_timeout};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
        let output = OwnedChild::output(Command::new(binary), Deadline::after(io_timeout()))
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
                let stream = stream.expect("accept release request");
                let deadline = Deadline::after(io_timeout());
                let mut stream = deadline.io(stream);
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
        let cleanup_budget = io_timeout();
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
            cleanup_budget,
        }
    }

    pub fn finish(mut self) -> thread::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> thread::Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        self.stopping.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        worker.join()
    }
}

impl Drop for ModuleRelease {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            let _ = writeln!(io::stderr(), "module release server worker panicked");
        }
    }
}

pub struct ModuleEmp {
    emp: Option<Emp>,
    root: PathBuf,
    observed: Mutex<BTreeSet<i32>>,
    cleanup_budget: TestBudget,
}

impl Deref for ModuleEmp {
    type Target = Emp;

    fn deref(&self) -> &Emp {
        self.emp.as_ref().expect("running EMP")
    }
}

fn processes(deadline: Deadline) -> io::Result<Vec<(i32, String)>> {
    let mut command = Command::new("ps");
    command.args(["-A", "-ww", "-o", "pid=", "-o", "args="]);
    let output = OwnedChild::output(command, deadline)
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
    if !output.status.success() {
        return Err(io::Error::other(format!("ps failed: {}", output.status)));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (pid, args) = line
                .trim()
                .split_once(char::is_whitespace)
                .ok_or(io::ErrorKind::InvalidData)?;
            let pid = pid.parse().map_err(|_| io::ErrorKind::InvalidData)?;
            Ok((pid, args.trim().to_owned()))
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
        let snapshot =
            processes(Deadline::after(io_timeout())).expect("enumerate module processes");
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
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        eventually(&deadline, "module PID was not reaped", || {
            let snapshot = processes(deadline).expect("enumerate module processes");
            (!snapshot.iter().any(|(found, _)| *found == pid)).then_some(())
        });
    }

    pub fn stop(mut self) {
        self.emp.take().expect("running EMP").stop();
        let directory = self.root.to_string_lossy();
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        eventually(&deadline, "orphan module processes", || {
            let observed = self.observed.lock().expect("observed workers");
            let remaining = processes(deadline)
                .expect("enumerate module processes")
                .into_iter()
                .any(|(pid, args)| observed.contains(&pid) || args.starts_with(directory.as_ref()));
            (!remaining).then_some(())
        });
    }
}

pub fn eventually<T>(deadline: &Deadline, what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    loop {
        deadline
            .accept((), what)
            .unwrap_or_else(|error| panic!("{error}"));
        if let Some(value) = probe() {
            return deadline
                .accept(value, what)
                .unwrap_or_else(|error| panic!("{error}"));
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "被观察的进程表和管理接口没有变更通知，只能固定间隔轮询"
        )]
        thread::sleep(deadline.remaining().min(Duration::from_millis(20)));
    }
}

impl Drop for ModuleEmp {
    fn drop(&mut self) {
        drop(self.emp.take());
        let directory = self.root.to_string_lossy();
        let snapshot = match processes(Deadline::after(self.cleanup_budget)) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = writeln!(io::stderr(), "module process cleanup: {error}");
                return;
            }
        };
        for (pid, args) in snapshot {
            if args.starts_with(directory.as_ref()) {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    }
}
