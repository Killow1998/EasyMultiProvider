//! Api quota.

use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::percent_decode;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::cross_origin_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::http::response::unauthorized_response;
use crate::services::quota::consume_quota_reset_for_account;
use crate::services::quota::refresh_account_by_id;
use crate::util::system_now;
use serde_json::Value;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) const QUOTA_EVENT_SLOT_LIMIT: usize = 4;

const QUOTA_EVENT_KEEP_ALIVE: Duration = Duration::from_secs(15);

struct QuotaEventSlot<'a> {
    active: &'a AtomicUsize,
}

impl QuotaEventSlot<'_> {
    fn acquire(active: &AtomicUsize) -> Option<QuotaEventSlot<'_>> {
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < QUOTA_EVENT_SLOT_LIMIT).then_some(count + 1)
            })
            .ok()
            .map(|_| QuotaEventSlot { active })
    }
}

impl Drop for QuotaEventSlot<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn serve_quota_events(
    stream: &mut TcpStream,
    request: Request<'_>,
    state: &ServerState,
    now: f64,
) {
    if !same_origin(request, state.port) {
        let _ = stream.write_all(&cross_origin_response("management session is required"));
        let _ = stream.flush();
        return;
    }
    let cookie = request.session_cookie();
    if !state.sessions.contains(cookie.as_deref(), now) {
        let _ = stream.write_all(&unauthorized_response());
        let _ = stream.flush();
        return;
    }
    let Some(_slot) = QuotaEventSlot::acquire(&state.backend.accounts.quota_event_slots) else {
        let response = json_error_response(
            503,
            status_text(503),
            "Too many quota subscribers",
            None,
            &[("Retry-After", "15")],
        );
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        return;
    };
    if stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .is_err()
        || stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Accel-Buffering: no\r\nConnection: close\r\n\r\n",
            )
            .is_err()
        || stream.flush().is_err()
    {
        return;
    }
    let mut observed_revision = u64::MAX;
    loop {
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }
        let revision = match state.backend.accounts.quota_revision.lock() {
            Ok(revision) => revision,
            Err(_) => break,
        };
        let (revision, _) = match state.backend.accounts.quota_condition.wait_timeout_while(
            revision,
            QUOTA_EVENT_KEEP_ALIVE,
            |revision| *revision == observed_revision && !state.shutdown.load(Ordering::Acquire),
        ) {
            Ok(result) => result,
            Err(_) => break,
        };
        let current = *revision;
        drop(revision);
        if state.shutdown.load(Ordering::Acquire)
            || !state.sessions.contains(cookie.as_deref(), system_now())
        {
            break;
        }
        let frame: &[u8] = if current != observed_revision {
            b"event: quota-updated\ndata: {}\n\n"
        } else {
            b": keep-alive\n\n"
        };
        observed_revision = current;
        if stream.write_all(frame).is_err() || stream.flush().is_err() {
            break;
        }
    }
}

pub(crate) fn management_quota_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    let supplied_cookie = request.session_cookie();
    if !state.sessions.contains(supplied_cookie.as_deref(), now) {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let path = request.raw_path();
    let reset = path.ends_with("/quota-reset");
    let suffix = if reset { "/quota-reset" } else { "/quota" };
    let raw_account = &path["/api/accounts/".len()..path.len() - suffix.len()];
    let account = percent_decode(raw_account.trim_end_matches('/'), false);
    let known_account = account == "@native"
        || state
            .backend
            .configuration
            .config
            .lock()
            .is_ok_and(|config| {
                config
                    .get("accounts")
                    .and_then(Value::as_array)
                    .is_some_and(|accounts| {
                        accounts.iter().any(|candidate| {
                            candidate.get("id").and_then(Value::as_str) == Some(account.as_str())
                        })
                    })
            });
    if !known_account {
        return json_error_response(
            503,
            status_text(503),
            &format!("unknown account: {account}"),
            Some("quota_error"),
            &[],
        );
    }
    let refresh_lock = match state.backend.accounts.quota_refresh_locks.lock() {
        Ok(mut locks) => Arc::clone(
            locks
                .entry(account.clone())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        ),
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    let _refresh_guard = match refresh_lock.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    if reset {
        let key = body
            .get("idempotency_key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let outcome = match consume_quota_reset_for_account(state, &account, key) {
            Ok(outcome) => outcome,
            Err(error) => {
                let status = if error.code() == "quota_reset_invalid_request" {
                    400
                } else {
                    503
                };
                return json_error_response(
                    status,
                    status_text(status),
                    &error.to_string(),
                    Some(error.code()),
                    &[],
                );
            }
        };
        let (account_snapshot, refresh_error) = match refresh_account_by_id(state, &account) {
            Ok(snapshot) => (snapshot, Value::Null),
            Err(error) => (
                Value::Null,
                serde_json::json!({
                    "code": error.code(),
                    "message": error.to_string(),
                }),
            ),
        };
        let response_body = serde_json::to_vec(&serde_json::json!({
            "outcome": outcome,
            "account": account_snapshot,
            "refresh_error": refresh_error,
        }))
        .expect("quota reset result is serializable");
        return response("HTTP/1.1 200 OK", "application/json", &response_body, &[]);
    }
    let refreshed = refresh_account_by_id(state, &account);
    match refreshed {
        Ok(account_snapshot) => {
            let body = serde_json::to_vec(&serde_json::json!({"account": account_snapshot}))
                .expect("account snapshot is serializable");
            response("HTTP/1.1 200 OK", "application/json", &body, &[])
        }
        Err(error) => json_error_response(
            503,
            status_text(503),
            &error.to_string(),
            Some(error.code()),
            &[],
        ),
    }
}
