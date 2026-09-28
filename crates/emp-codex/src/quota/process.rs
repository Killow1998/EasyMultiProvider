//! Isolated Codex app-server process and executable verification.

use super::*;

#[derive(Debug)]
pub(super) struct TrustedBinary {
    path: PathBuf,
    identity: BinaryIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BinaryIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(not(unix))]
    Portable {
        length: u64,
        modified: Option<SystemTime>,
    },
}

impl TrustedBinary {
    pub(super) fn resolve(name: &str) -> Result<Self, QuotaError> {
        let candidate = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            find_in_path(name).ok_or_else(binary_unavailable)?
        };
        let path = candidate.canonicalize().map_err(|_| binary_unavailable())?;
        validate_binary_path(&path)?;
        Ok(Self {
            identity: binary_identity(&path)?,
            path,
        })
    }

    fn verify(&self) -> Result<(), QuotaError> {
        let verified = Self::resolve(
            self.path
                .to_str()
                .ok_or_else(|| QuotaError::new("Codex executable was replaced", "quota_error"))?,
        )
        .map_err(|_| QuotaError::new("Codex executable was replaced", "quota_error"))?;
        if verified.path != self.path || verified.identity != self.identity {
            return Err(QuotaError::new(
                "Codex executable was replaced",
                "quota_error",
            ));
        }
        Ok(())
    }
}

fn binary_unavailable() -> QuotaError {
    QuotaError::new("Codex executable is unavailable", "quota_error")
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains(std::path::MAIN_SEPARATOR) {
        return None;
    }
    #[cfg(windows)]
    let names = if Path::new(name).extension().is_some() {
        vec![name.to_owned()]
    } else {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!("{name}{extension}"))
            .collect::<Vec<_>>()
    };
    #[cfg(not(windows))]
    let names = [name.to_owned()];
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        names.iter().find_map(|name| {
            let candidate = directory.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

fn validate_binary_path(path: &Path) -> Result<(), QuotaError> {
    use crate::executable_trust::{TrustFailure, validate_executable};
    validate_executable(path).map_err(|failure| match failure {
        TrustFailure::Unavailable => binary_unavailable(),
        TrustFailure::NotTrusted => {
            QuotaError::new("Codex executable is not trusted", "quota_error")
        }
        TrustFailure::Writable => {
            QuotaError::new("Codex executable path is writable", "quota_error")
        }
    })
}

fn binary_identity(path: &Path) -> Result<BinaryIdentity, QuotaError> {
    let metadata = fs::metadata(path).map_err(|_| binary_unavailable())?;
    #[cfg(unix)]
    {
        Ok(BinaryIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(BinaryIdentity::Portable {
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

pub(super) fn read_native_auth(path: &Path) -> Result<Value, QuotaError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| QuotaError::new("native auth.json is unavailable", "quota_error"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_AUTH_BYTES {
        return Err(QuotaError::new(
            "native auth.json is unavailable",
            "quota_error",
        ));
    }
    let raw = fs::read(path)
        .map_err(|_| QuotaError::new("native auth.json is unavailable", "quota_error"))?;
    let auth: Value = serde_json::from_slice(&raw)
        .map_err(|_| QuotaError::new("native auth.json is unavailable", "quota_error"))?;
    if account_auth_headers(&auth).is_none() {
        return Err(QuotaError::new(
            "native auth.json is unavailable",
            "quota_error",
        ));
    }
    Ok(auth)
}

pub(super) fn run_isolated_quota_process(
    auth: &Value,
    binary: &TrustedBinary,
    timeout: Duration,
    allow_refresh: bool,
    reset_idempotency_key: Option<&str>,
    reset_credit_id: Option<&str>,
    mut persist_rotation: Option<PersistRotation<'_>>,
) -> Result<QuotaProcessResult, QuotaError> {
    sweep_stale_credential_directories(&env::temp_dir(), SystemTime::now());
    let directory = tempfile::Builder::new()
        .prefix(CREDENTIAL_DIRECTORY_PREFIX)
        .tempdir()
        .map_err(|_| quota_check_failed())?;
    set_private_directory(directory.path())?;
    let auth_path = directory.path().join("auth.json");
    write_private(
        &auth_path,
        &serde_json::to_vec(auth).map_err(|_| quota_check_failed())?,
    )?;
    write_private(
        &directory.path().join("config.toml"),
        b"cli_auth_credentials_store = \"file\"\n",
    )?;

    binary.verify()?;
    let mut command = Command::new(&binary.path);
    command
        .arg("app-server")
        .arg("--stdio")
        .current_dir(directory.path())
        .env_clear()
        .env("CODEX_HOME", directory.path())
        .env("HOME", home_dir())
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in INHERITED_ENVIRONMENT {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().map_err(|_| quota_check_failed())?;
    let result = query_child(
        &mut child,
        timeout,
        allow_refresh,
        reset_idempotency_key,
        reset_credit_id,
    );
    let cleanup = finish_child(&mut child);
    let refreshed_auth = fs::read(&auth_path)
        .ok()
        .filter(|raw| raw.len() <= MAX_AUTH_BYTES as usize)
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .filter(|value| value != auth && account_auth_headers(value).is_some());
    if let (Some(persist), Some(refreshed)) = (&mut persist_rotation, &refreshed_auth) {
        persist(refreshed)?;
    }
    let output = match result {
        Ok(output) => {
            cleanup?;
            output
        }
        Err(error) => return Err(error),
    };
    Ok(QuotaProcessResult {
        quota: if reset_idempotency_key.is_some() {
            json!({"outcome": reset_outcome(&output, 3)?})
        } else {
            parse_app_server_output(&output)?
        },
        refreshed_auth,
    })
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Temporary `CODEX_HOME` directories hold a decrypted `auth.json` while one
/// quota check runs.
const CREDENTIAL_DIRECTORY_PREFIX: &str = "easy-mp-codex-account-";
/// A quota check is bounded well below this age; older directories are
/// leftovers of a crashed or killed EMP process and still hold live tokens.
const STALE_CREDENTIAL_DIRECTORY_AGE: Duration = Duration::from_secs(60 * 60);

/// Remove this user's stale credential directories left behind by an abnormal
/// exit. Links, other users' entries and recent directories are left alone.
fn sweep_stale_credential_directories(temp_root: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(temp_root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(CREDENTIAL_DIRECTORY_PREFIX))
        {
            continue;
        }
        // DirEntry metadata does not follow links.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        #[cfg(unix)]
        if metadata.uid() != unsafe { libc::getuid() } {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_CREDENTIAL_DIRECTORY_AGE);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn set_private_directory(path: &Path) -> Result<(), QuotaError> {
    emp_state::create_private_directory(path).map_err(|_| quota_check_failed())
}

/// Create a new private file without following links: mode 0600 on Unix and
/// a current-user-only DACL on Windows.
fn write_private(path: &Path, value: &[u8]) -> Result<(), QuotaError> {
    emp_state::write_new_private_file(path, value).map_err(|_| quota_check_failed())
}

enum ProcessLine {
    Line(String),
    Encoding,
    Read,
    End,
}

fn stdout_reader(stdout: impl Read + Send + 'static) -> (Receiver<ProcessLine>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sender.send(ProcessLine::End);
                    return;
                }
                Ok(_) => {
                    if sender.send(ProcessLine::Line(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let kind = if error.kind() == std::io::ErrorKind::InvalidData {
                        ProcessLine::Encoding
                    } else {
                        ProcessLine::Read
                    };
                    let _ = sender.send(kind);
                    return;
                }
            }
        }
    });
    (receiver, worker)
}

fn query_child(
    child: &mut Child,
    timeout: Duration,
    allow_refresh: bool,
    reset_idempotency_key: Option<&str>,
    reset_credit_id: Option<&str>,
) -> Result<String, QuotaError> {
    let stdout = child.stdout.take().ok_or_else(quota_check_failed)?;
    let stderr = child.stderr.take().ok_or_else(quota_check_failed)?;
    let (receiver, stdout_worker) = stdout_reader(stdout);
    let stderr_worker = thread::spawn(move || {
        let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
    });
    let mut stdin = child.stdin.take().ok_or_else(quota_check_failed)?;
    let deadline = Instant::now() + timeout;
    let requests = [
        json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "easy-multi-provider",
                    "title": "EMP",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {},
            }
        }),
        json!({"method": "initialized"}),
        json!({"id": 2, "method": "account/read", "params": {"refreshToken": allow_refresh}}),
        reset_idempotency_key.map_or_else(
            || json!({"id": 3, "method": "account/rateLimits/read", "params": Value::Null}),
            |key| {
                let mut params = json!({"idempotencyKey": key});
                if let Some(credit_id) = reset_credit_id {
                    params["creditId"] = Value::String(credit_id.to_owned());
                }
                json!({
                    "id": 3,
                    "method": "account/rateLimitResetCredit/consume",
                    "params": params,
                })
            },
        ),
    ];
    let mut output = String::new();
    let result = (|| {
        for request in requests {
            serde_json::to_writer(&mut stdin, &request).map_err(|_| quota_check_failed())?;
            stdin.write_all(b"\n").map_err(|_| quota_check_failed())?;
            stdin.flush().map_err(|_| quota_check_failed())?;
            let Some(id) = request.get("id").and_then(Value::as_i64) else {
                continue;
            };
            wait_for_response(&receiver, deadline, id, &request["method"], &mut output)?;
        }
        Ok(output)
    })();
    drop(stdin);
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = stdout_worker.join();
    let _ = stderr_worker.join();
    result
}

fn wait_for_response(
    receiver: &Receiver<ProcessLine>,
    deadline: Instant,
    request_id: i64,
    method: &Value,
    output: &mut String,
) -> Result<(), QuotaError> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(QuotaError::new(
                "Codex account quota check timed out",
                "quota_timeout",
            ));
        };
        let line = match receiver.recv_timeout(remaining) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                return Err(QuotaError::new(
                    "Codex account quota check timed out",
                    "quota_timeout",
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(QuotaError::new(
                    "Codex did not return account rate limits",
                    "quota_error",
                ));
            }
        };
        let line = match line {
            ProcessLine::Line(line) => line,
            ProcessLine::Encoding => {
                return Err(QuotaError::new(
                    "Codex app-server output is not valid UTF-8",
                    "quota_output_encoding_error",
                ));
            }
            ProcessLine::Read => {
                return Err(QuotaError::new(
                    "Could not read Codex app-server output",
                    "quota_output_read_error",
                ));
            }
            ProcessLine::End => {
                return Err(QuotaError::new(
                    "Codex did not return account rate limits",
                    "quota_error",
                ));
            }
        };
        output.push_str(&line);
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let Some(message) = message.as_object() else {
            return Err(QuotaError::new(
                "Codex quota JSON-RPC output must be an object",
                "quota_output_protocol_error",
            ));
        };
        if message.get("id").and_then(Value::as_i64) != Some(request_id) {
            continue;
        }
        let method = method.as_str().unwrap_or_default();
        if let Some(error) = message.get("error") {
            return Err(quota_rpc_error(method, error));
        }
        if method == "account/read"
            && message
                .get("result")
                .and_then(Value::as_object)
                .is_some_and(|result| result.get("account") == Some(&Value::Null))
        {
            return Err(QuotaError::new(
                "Codex account needs sign-in; sign in again and re-import the account",
                "quota_auth_required",
            ));
        }
        return Ok(());
    }
}

fn finish_child(child: &mut Child) -> Result<(), QuotaError> {
    let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(quota_check_failed())
                };
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(quota_check_failed());
            }
        }
    }
}

fn quota_check_failed() -> QuotaError {
    QuotaError::new("Codex account quota check failed", "quota_error")
}

pub(super) fn rpc_http_status(message: &str) -> Option<u16> {
    let public = message
        .split_once("; body=")
        .map_or(message, |(head, _)| head);
    for (index, _) in public.match_indices(" failed: ") {
        let tail = &public[index + " failed: ".len()..];
        let bytes = tail.as_bytes();
        if bytes.len() >= 3
            && bytes[..3].iter().all(u8::is_ascii_digit)
            && bytes.get(3).is_none_or(|byte| !byte.is_ascii_digit())
            && public[index..].contains("; content-type=")
        {
            return tail[..3].parse().ok();
        }
    }
    None
}

#[cfg(all(test, unix))]
mod credential_directory_tests {
    use super::*;

    #[test]
    fn stale_credential_directories_are_swept_but_recent_and_foreign_entries_stay() {
        let root = tempfile::tempdir().expect("temporary root");
        let stale = root
            .path()
            .join(format!("{CREDENTIAL_DIRECTORY_PREFIX}stale"));
        let recent = root
            .path()
            .join(format!("{CREDENTIAL_DIRECTORY_PREFIX}recent"));
        let unrelated = root.path().join("unrelated-stale");
        let target = root.path().join("link-target");
        for directory in [&stale, &recent, &unrelated, &target] {
            fs::create_dir(directory).expect("create directory");
        }
        fs::write(stale.join("auth.json"), b"{}").expect("stale credential");
        let link = root
            .path()
            .join(format!("{CREDENTIAL_DIRECTORY_PREFIX}link"));
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        // Everything created just now is two hours old from `now`'s view,
        // except `recent`, which is judged against its own modification time.
        let later = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        sweep_stale_credential_directories(root.path(), SystemTime::now());
        assert!(stale.exists() && recent.exists(), "fresh directories stay");
        sweep_stale_credential_directories(root.path(), later);
        assert!(!stale.exists(), "stale credential directory removed");
        assert!(
            !recent.exists(),
            "an old enough prefixed directory is stale too"
        );
        assert!(unrelated.exists(), "unrelated directories stay");
        assert!(
            link.symlink_metadata().is_ok() && target.exists(),
            "links are not followed"
        );
    }

    #[test]
    fn credential_files_are_private_and_never_replace_existing_paths() {
        let root = tempfile::tempdir().expect("temporary root");
        let directory = root.path().join("home");
        fs::create_dir(&directory).expect("create directory");
        set_private_directory(&directory).expect("private directory");
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let auth = directory.join("auth.json");
        write_private(&auth, b"{}").expect("private file");
        assert_eq!(
            fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            write_private(&auth, b"{}").is_err(),
            "existing file is not replaced"
        );
        let link = directory.join("link.json");
        std::os::unix::fs::symlink(root.path().join("elsewhere"), &link).expect("symlink");
        assert!(
            write_private(&link, b"{}").is_err(),
            "links are not followed"
        );
        assert!(!root.path().join("elsewhere").exists());
    }
}
