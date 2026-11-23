use crate::app::ServerState;
use crate::http::request::Request;
use crate::http::request::{percent_decode, query_values};
use crate::http::response::json_error_body;
use crate::server::http::error::{EntryError, JsonReply};
use crate::services::account_catalog::{self, CatalogRefreshError};
use crate::services::accounts::{
    AccountImport, accounts_snapshot, delete_account_state, import_parsed_account,
};
use crate::services::configuration::{ChangeError, settings};
use crate::services::quota::commands::{self, CommandError, QuotaCommand};
use crate::services::quota::{QuotaHistoryResponseError, quota_history_response};
use serde_json::{Map, Value, json};

pub(crate) enum AccountRead<'a> {
    List,
    Models(&'a str),
    QuotaHistory(&'a str),
}

impl<'a> AccountRead<'a> {
    pub(crate) fn for_path(path: &'a str) -> Option<Self> {
        if path == "/api/accounts" {
            return Some(Self::List);
        }
        let suffix = path.strip_prefix("/api/accounts/")?;
        if suffix == "models" {
            return Some(Self::Models(""));
        }
        suffix
            .strip_suffix("/models")
            .map(Self::Models)
            .or_else(|| {
                suffix
                    .strip_suffix("/quota-history")
                    .map(Self::QuotaHistory)
            })
    }

    pub(crate) fn read(
        self,
        request: Request<'_>,
        state: &ServerState,
        now: f64,
    ) -> Result<JsonReply, EntryError> {
        let payload = match self {
            Self::List => accounts_snapshot(state).ok_or_else(EntryError::internal)?,
            Self::Models(id) => {
                let config = state
                    .backend
                    .configuration
                    .read()
                    .map_err(|_| EntryError::internal())?
                    .clone();
                let models = crate::services::catalog::subscription_options(
                    state,
                    &config,
                    &percent_decode(id, false),
                )
                .map_err(|message| EntryError::Json(400, message, None))?;
                serde_json::json!({"models":models})
            }
            Self::QuotaHistory("") => return Err(EntryError::NotFound),
            Self::QuotaHistory(id) => {
                let range = query_values(request.target, "range")
                    .into_iter()
                    .find(|value| !value.is_empty())
                    .unwrap_or_else(|| "1d".to_owned());
                let bound = |name| {
                    query_values(request.target, name)
                        .into_iter()
                        .find_map(|value| value.parse::<f64>().ok())
                        .filter(|value| value.is_finite())
                        .map(|value| value.trunc() as i64)
                };
                match quota_history_response(
                    state,
                    &percent_decode(id, false),
                    &range,
                    bound("start").zip(bound("end")),
                    now.trunc() as i64,
                ) {
                    Ok(payload) => payload,
                    Err(QuotaHistoryResponseError::History(error)) => {
                        return Ok(JsonReply::new(
                            400,
                            json_error_body(&error.to_string(), None),
                        ));
                    }
                    Err(QuotaHistoryResponseError::Account(error)) => {
                        return Ok(JsonReply::new(
                            404,
                            json_error_body(&error.to_string(), None),
                        ));
                    }
                }
            }
        };
        Ok(JsonReply::new(
            200,
            serde_json::to_vec(&payload).expect("account read reply is JSON serializable"),
        ))
    }
}

pub(crate) enum AccountWrite {
    Import(Result<AccountImport, String>),
    Delete(String),
    EditConfiguration(Map<String, Value>),
    RefreshModels(String),
    Quota(String, QuotaCommand),
}

impl AccountWrite {
    pub(crate) fn execute(self, state: &ServerState, id: Option<&str>) -> JsonReply {
        let payload = match self {
            Self::Import(input) => import_parsed_account(id, state, input)
                .map(|account| json!({"account":account}))
                .map_err(|error| JsonReply::error(400, &error, None)),
            Self::Delete(account) => delete_account_state(id, state, &account)
                .map(|()| json!({"status":"ok"}))
                .map_err(|error| JsonReply::error(400, &error, None)),
            Self::EditConfiguration(body) => {
                return match settings::update(id, state, &Value::Object(body)) {
                    Ok(()) => super::catalog::read_configuration(state),
                    Err(ChangeError::Invalid(message)) => JsonReply::error(400, &message, None),
                    Err(ChangeError::Unavailable) => EntryError::internal().reply(),
                };
            }
            Self::RefreshModels(account) => {
                account_catalog::refresh(id, state, &account).map_err(|error| match error {
                    CatalogRefreshError::Upstream(error) => {
                        super::failure_response::router_error_reply(error)
                    }
                    CatalogRefreshError::Invalid(message) => JsonReply::error(400, message, None),
                    CatalogRefreshError::Internal => EntryError::internal().reply(),
                })
            }
            Self::Quota(account, command) => {
                commands::execute(id, state, &account, command).map_err(quota_error)
            }
        };
        match payload {
            Ok(payload) => JsonReply::new(200, payload.to_string()),
            Err(reply) => reply,
        }
    }
}

fn quota_error(error: CommandError) -> JsonReply {
    match error {
        CommandError::Internal => EntryError::internal().reply(),
        CommandError::UnknownAccount(account) => JsonReply::error(
            503,
            &format!("unknown account: {account}"),
            Some("quota_error"),
        ),
        CommandError::InvalidCredit => JsonReply::error(
            400,
            "reset credit id is invalid",
            Some("quota_reset_invalid_request"),
        ),
        CommandError::ResetForbidden => JsonReply::error(
            403,
            "A workspace administrator must reset this shared subscription",
            Some("workspace_reset_forbidden"),
        ),
        CommandError::Reset(error) => {
            let status = if error.code() == "quota_reset_invalid_request" {
                400
            } else {
                503
            };
            JsonReply::error(status, &error.to_string(), Some(error.code()))
        }
        CommandError::Refresh(error) => {
            JsonReply::error(503, &error.to_string(), Some(error.code()))
        }
    }
}
