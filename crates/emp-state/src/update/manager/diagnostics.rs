//! One bounded failure receipt; never serialize exception text, URLs or arguments.
use super::UpdateError;
use super::UpdateManager;
use serde::{Deserialize, Serialize};
use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UpdateDiagnostic {
    pub stage: String,
    pub error: String,
    pub timestamp: u64,
    pub current_version: String,
    pub target_version: Option<String>,
    pub reason: Option<String>,
    pub os_error: Option<i32>,
    pub http_status: Option<u16>,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub retry_count: u8,
    #[serde(default)]
    pub retry_limit: u8,
}

pub(in crate::update) fn path_for(args: &[String]) -> Option<PathBuf> {
    let config = args.windows(2).find(|pair| pair[0] == "--config")?;
    Some(
        Path::new(&config[1])
            .parent()?
            .join("state/update-last-error.json"),
    )
}

pub(in crate::update) fn read(path: &Path) -> Option<UpdateDiagnostic> {
    let raw = crate::read_file_limited(path, 4096).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub(in crate::update) fn save(path: Option<&Path>, diagnostic: &UpdateDiagnostic) {
    let Ok(raw) = serde_json::to_vec(diagnostic) else {
        return;
    };
    // Even when the state directory cannot be written, the console retains the
    // same fixed-field receipt. No OS error's Display text enters either log.
    eprintln!("EMP update failure: {}", String::from_utf8_lossy(&raw));
    if let Some(path) = path
        && let Err(error) = crate::atomic_write_private_state(path, &raw)
    {
        eprintln!("EMP update diagnostic save failed: {error:?}");
    }
}

pub(in crate::update) fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl UpdateManager {
    pub(in crate::update) fn stage(&self, stage: &str) {
        if let Ok(mut diagnostic) = self.0.diagnostic.lock() {
            *diagnostic = UpdateDiagnostic {
                stage: stage.into(),
                ..UpdateDiagnostic::default()
            };
        }
    }

    pub(in crate::update) fn io_error(&self, stage: &str, error: std::io::Error) -> UpdateError {
        self.stage(stage);
        if let Ok(mut diagnostic) = self.0.diagnostic.lock() {
            diagnostic.reason = Some(if error.kind() == std::io::ErrorKind::TimedOut {
                "timeout".into()
            } else {
                format!("{:?}", error.kind())
            });
            diagnostic.os_error = error.raw_os_error();
            let mut source = error.source();
            while let Some(cause) = source {
                if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                    diagnostic.os_error = io.raw_os_error().or(diagnostic.os_error);
                }
                if let Some(request) = cause.downcast_ref::<reqwest::Error>() {
                    if request.is_timeout() {
                        diagnostic.reason = Some("timeout".into());
                    } else if request.is_connect() {
                        diagnostic.reason = Some("connection".into());
                    } else if request.is_body() || request.is_request() {
                        diagnostic.reason = Some("request".into());
                    }
                }
                source = cause.source();
            }
        }
        error.into()
    }

    pub(in crate::update) fn transport_error(&self, error: &reqwest::Error) {
        if let Ok(mut diagnostic) = self.0.diagnostic.lock() {
            diagnostic.reason = Some(
                if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connection"
                } else {
                    "request"
                }
                .into(),
            );
            diagnostic.http_status = error.status().map(|status| status.as_u16());
            let mut source = error.source();
            while let Some(cause) = source {
                if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                    diagnostic.os_error = io.raw_os_error().or(diagnostic.os_error);
                }
                source = cause.source();
            }
        }
    }

    pub(in crate::update) fn http_error(&self, status: u16) {
        if let Ok(mut diagnostic) = self.0.diagnostic.lock() {
            diagnostic.http_status = Some(status);
            diagnostic.reason = Some("http_status".into());
        }
    }

    pub(in crate::update) fn incomplete_download(&self) {
        self.stage("download_package");
        if let Ok(mut diagnostic) = self.0.diagnostic.lock() {
            diagnostic.reason = Some("incomplete_download".into());
        }
    }

    pub(in crate::update) fn failed(&self, error: UpdateError) {
        let snapshot = self.snapshot();
        let mut diagnostic = self
            .0
            .diagnostic
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        diagnostic.error = error.0.into();
        diagnostic.timestamp = timestamp();
        diagnostic.current_version = snapshot.current_version;
        diagnostic.target_version = snapshot.latest_version;
        diagnostic.retry_count = snapshot.retry_count;
        diagnostic.retry_limit = snapshot.retry_limit;
        save(self.0.diagnostic_path.as_deref(), &diagnostic);
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            snapshot.failure = Some(diagnostic);
        }
    }
}
