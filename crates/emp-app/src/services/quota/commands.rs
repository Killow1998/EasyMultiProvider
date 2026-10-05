//! Explicit quota commands own serialization and reset/read ordering.
use super::{consume_quota_reset_for_account, refresh_account_by_id};
use crate::app::ServerState;
use crate::services::accounts::quota_refresh_lock;
use crate::services::observation::operation::{OperationObservation, observe};
use emp_codex::quota::QuotaError;
use serde_json::Value;

pub(crate) enum CommandError {
    Internal,
    UnknownAccount(String),
    InvalidCredit,
    Reset(QuotaError),
    Refresh(QuotaError),
}

pub(crate) fn execute(
    request_id: Option<&str>,
    state: &ServerState,
    account: &str,
    reset: bool,
    body: &Value,
) -> Result<Value, CommandError> {
    observe(
        &state.backend.diagnostics,
        request_id,
        if reset { "quota_reset" } else { "quota_read" },
        |receipt| {
            receipt.subject(account);
            execute_inner(state, account, reset, body, receipt)
        },
    )
}

fn execute_inner(
    state: &ServerState,
    account: &str,
    reset: bool,
    body: &Value,
    receipt: &mut OperationObservation,
) -> Result<Value, CommandError> {
    let known_account = account == "@native"
        || state.backend.configuration.read().is_ok_and(|config| {
            config
                .get("accounts")
                .and_then(Value::as_array)
                .is_some_and(|accounts| {
                    accounts.iter().any(|candidate| {
                        candidate.get("id").and_then(Value::as_str) == Some(account)
                    })
                })
        });
    if !known_account {
        return Err(CommandError::UnknownAccount(account.to_owned()));
    }
    let refresh_lock = match quota_refresh_lock(state, account) {
        Some(lock) => lock,
        None => {
            return Err(CommandError::Internal);
        }
    };
    let _refresh_guard = match refresh_lock.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(CommandError::Internal);
        }
    };
    if reset {
        let key = body
            .get("idempotency_key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let credit_id = match body.get("credit_id") {
            None => None,
            Some(Value::String(value)) => Some(value.as_str()),
            _ => {
                return Err(CommandError::InvalidCredit);
            }
        };
        let consumed = receipt.step("consume_reset", || {
            consume_quota_reset_for_account(state, account, key, credit_id)
        });
        let journal = &state.backend.diagnostics.journal;
        journal.event(
            if consumed.is_ok() { "info" } else { "warning" },
            "quota_reset",
            &serde_json::json!({
                "account": journal.pseudonym(account),
                "success": consumed.is_ok(),
                "error_class": consumed.as_ref().err().map(|error| error.code()),
            }),
        );
        let outcome = match consumed {
            Ok(outcome) => outcome,
            Err(error) => return Err(CommandError::Reset(error)),
        };
        receipt.fact(
            "reset_outcome",
            match outcome.as_str() {
                "reset" => "reset",
                "nothingToReset" => "nothing_to_reset",
                "noCredit" => "no_credit",
                "alreadyRedeemed" => "already_redeemed",
                _ => "unknown",
            },
        );
        let (account_snapshot, refresh_error) = match receipt.step("refresh_after_reset", || {
            refresh_account_by_id(state, account)
        }) {
            Ok(snapshot) => (snapshot, Value::Null),
            Err(error) => (
                Value::Null,
                serde_json::json!({
                    "code": error.code(),
                    "message": error.to_string(),
                }),
            ),
        };
        receipt.check("quota_refreshed", Some(refresh_error.is_null()));
        return Ok(serde_json::json!({
            "outcome": outcome,
            "account": account_snapshot,
            "refresh_error": refresh_error,
        }));
    }
    let snapshot = receipt
        .step("refresh_quota", || refresh_account_by_id(state, account))
        .map_err(CommandError::Refresh)?;
    receipt.check("quota_refreshed", Some(true));
    Ok(serde_json::json!({"account":snapshot}))
}
