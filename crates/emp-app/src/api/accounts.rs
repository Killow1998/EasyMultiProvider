use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::request::{percent_decode, query_values};
use crate::http::response::body_error_response;
use crate::http::response::cross_origin_response;
use crate::http::response::json_error_body;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::http::response::unauthorized_response;
use crate::server::http::error::{EntryError, JsonReply};
use crate::services::account_catalog::CatalogRefreshError;
use crate::services::accounts::{accounts_snapshot, import_account_state};
use crate::services::quota::{QuotaHistoryResponseError, quota_history_response};
use std::net::TcpStream;

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

pub(crate) fn management_account_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    if !state
        .sessions
        .contains(request.session_token().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    if let Some(id) = request
        .raw_path()
        .strip_prefix("/api/accounts/")
        .and_then(|path| path.strip_suffix("/models/refresh"))
    {
        let id = percent_decode(id, false);
        return match crate::services::account_catalog::refresh(request.observation_id, state, &id) {
            Ok(payload) => response(
                "HTTP/1.1 200 OK",
                "application/json",
                &serde_json::to_vec(&payload).expect("subscription models"),
                &[],
            ),
            Err(CatalogRefreshError::Upstream(error)) => {
                crate::api::failure_response::router_error_response(error)
            }
            Err(CatalogRefreshError::Invalid(message)) => {
                json_error_response(400, status_text(400), message, None, &[])
            }
            Err(CatalogRefreshError::Internal) => {
                json_error_response(500, status_text(500), "internal server error", None, &[])
            }
        };
    }
    match import_account_state(request.observation_id, state, &body) {
        Ok(account) => {
            let body = serde_json::to_vec(&serde_json::json!({"account":account})).unwrap();
            response("HTTP/1.1 200 OK", "application/json", &body, &[])
        }
        Err(error) => json_error_response(400, status_text(400), &error, None, &[]),
    }
}
