//! Retry only idempotent network steps, never installation or replacement.
use super::{Result, UpdateManager, json};
use std::time::Duration;

pub(super) const MAX_RETRIES: u8 = 3;

impl UpdateManager {
    pub(in crate::update) fn retry_network(
        &self,
        mut operation: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            snapshot.retry_count = 0;
        }
        loop {
            let Err(error) = operation() else {
                return Ok(());
            };
            let diagnostic = self.0.diagnostic.lock().ok().map(|value| value.clone());
            let Some(diagnostic) = diagnostic.filter(|value| {
                matches!(error.0, "update_failed" | "check_failed")
                    && matches!(
                        value.stage.as_str(),
                        "check_release" | "read_release" | "download_package"
                    )
                    && match value.http_status {
                        Some(status) => matches!(status, 408 | 500 | 502 | 503 | 504),
                        None => matches!(
                            value.reason.as_deref(),
                            Some(
                                "connection"
                                    | "timeout"
                                    | "request"
                                    | "incomplete_download"
                                    | "ConnectionReset"
                                    | "ConnectionAborted"
                                    | "BrokenPipe"
                                    | "UnexpectedEof"
                            )
                        ),
                    }
            }) else {
                return Err(error);
            };
            let count = {
                let Ok(mut snapshot) = self.0.snapshot.lock() else {
                    return Err(error);
                };
                if snapshot.retry_count == MAX_RETRIES {
                    return Err(error);
                }
                snapshot.retry_count += 1;
                snapshot.progress = 0;
                snapshot.retry_count
            };
            self.trace_retry(&diagnostic, count);
            eprintln!(
                "EMP update retry: {}",
                json!({
                    "retry_count": count, "retry_limit": MAX_RETRIES,
                    "stage": diagnostic.stage, "reason": diagnostic.reason,
                    "http_status": diagnostic.http_status, "os_error": diagnostic.os_error
                })
            );
            std::thread::sleep(Duration::from_secs(1 << (count - 1)));
        }
    }
}
