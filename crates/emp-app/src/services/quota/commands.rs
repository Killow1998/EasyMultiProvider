//! Explicit quota commands own serialization and reset/read ordering.
use super::{consume_quota_reset_for_account, refresh_locked_account, with_refresh_target};
use crate::app::ServerState;
use crate::services::observation::operation::{OperationObservation, observe};
use emp_codex::quota::{QuotaError, validated_reset_credit_id, validated_reset_idempotency_key};
use serde_json::Value;

pub(crate) enum CommandError {
    Internal,
    UnknownAccount(String),
    InvalidCredit,
    ResetForbidden,
    Reset(QuotaError),
    Refresh(QuotaError),
}

pub(crate) enum QuotaCommand {
    Refresh,
    Reset(Result<(String, Option<String>), CommandError>),
}

impl QuotaCommand {
    pub(crate) fn reset(body: &Value) -> Self {
        let key = body
            .get("idempotency_key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let credit = match body.get("credit_id") {
            None => None,
            Some(Value::String(value)) => Some(value.as_str()),
            _ => return Self::Reset(Err(CommandError::InvalidCredit)),
        };
        Self::Reset(
            validated_reset_idempotency_key(key)
                .and_then(|key| {
                    validated_reset_credit_id(credit).map(|credit| (key, credit.map(str::to_owned)))
                })
                .map_err(CommandError::Reset),
        )
    }
}

pub(crate) fn execute(
    request_id: Option<&str>,
    state: &ServerState,
    account: &str,
    command: QuotaCommand,
) -> Result<Value, CommandError> {
    observe(
        &state.backend.diagnostics,
        request_id,
        match command {
            QuotaCommand::Reset(_) => "quota_reset",
            QuotaCommand::Refresh => "quota_read",
        },
        |receipt| {
            receipt.subject(account);
            execute_inner(state, account, command, receipt)
        },
    )
}

fn execute_inner(
    state: &ServerState,
    account: &str,
    command: QuotaCommand,
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
    let reset = match command {
        QuotaCommand::Refresh => None,
        QuotaCommand::Reset(input) => Some(input?),
    };
    if reset.is_some() && !state.backend.team.shared_subscription_reset(state, account) {
        return Err(CommandError::ResetForbidden);
    }
    with_refresh_target(state, account, |replacement| {
        execute_locked(state, account, replacement, reset, receipt)
    })
    .ok_or(CommandError::Internal)?
}

fn execute_locked(
    state: &ServerState,
    account: &str,
    replacement: Option<&str>,
    reset: Option<(String, Option<String>)>,
    receipt: &mut OperationObservation,
) -> Result<Value, CommandError> {
    if let Some((key, credit_id)) = reset {
        let consumed = receipt.step("consume_reset", || {
            consume_quota_reset_for_account(state, account, &key, credit_id.as_deref())
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
            refresh_locked_account(state, account, replacement)
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
        .step("refresh_quota", || {
            refresh_locked_account(state, account, replacement)
        })
        .map_err(CommandError::Refresh)?;
    receipt.check("quota_refreshed", Some(true));
    Ok(serde_json::json!({"account":snapshot}))
}
