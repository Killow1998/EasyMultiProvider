use crate::VERSION;
use crate::api::failure_response::request_router_error_response;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::error::HttpReply;
use crate::services::accounts::native_subscription_headers;
use crate::util::random_hex;
use emp_codex::account_auth_headers;
use emp_transport::HttpMethod;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn execute(request: Request<'_>, body: Value, state: &ServerState) -> HttpReply {
    let config = match state.backend.configuration.read() {
        Ok(config) => config.clone(),
        Err(_) => {
            return HttpReply::error(500, "internal server error", None);
        }
    };
    if config["subscription_search"]["enabled"] != true {
        return HttpReply::error(
            403,
            "Subscription web search is disabled",
            Some("router_error"),
        );
    }
    let mut headers = BTreeMap::from([
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), format!("EMP/{VERSION}")),
    ]);
    let mut denied = false;
    let team = &state.backend.team;
    let team_active = state.backend.modules.active(emp_modules::Module::Team);
    let credentials = native_subscription_headers(&state.backend.codex.auth_path)
        .map(|headers| (headers, None))
        .or_else(|| {
            config["accounts"]
                .as_array()
                .expect("normalized accounts")
                .iter()
                .filter(|account| account["enabled"] != false)
                .find_map(|account| {
                    let id = account["id"].as_str().expect("normalized account ID");
                    let source = team.subscription_source(state, id, account);
                    if team.check_route(&source, team_active, None).is_err() {
                        denied = true;
                        return None;
                    }
                    let path = account.get("auth_file")?.as_str()?;
                    let auth = state
                        .backend
                        .configuration
                        .vault
                        .read_encrypted_json(Path::new(path))
                        .ok()?;
                    account_auth_headers(&auth).map(|headers| (headers, Some((source, auth))))
                })
        });
    if let Some((credentials, source)) = credentials {
        let budget = match source {
            Some((source, auth)) => team.check_subscription_budget(state, &source, || {
                Some(emp_state::accounts::account_auth_identity(&auth))
            }),
            None => team.check_native_budget(state),
        };
        if let Err(denial) = budget {
            return super::native_response::error_reply(denial.public_error(state));
        }
        headers.extend(credentials);
    } else if denied
        || emp_codex::configuration::local_auth::mode(&state.backend.codex.auth_path) == "local_key"
    {
        return HttpReply::error(
            503,
            "Subscription web search requires a signed-in or imported Codex subscription",
            Some("subscription_required"),
        );
    } else {
        for name in ["authorization", "chatgpt-account-id"] {
            if let Some(value) = request.header(name) {
                headers.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    if let Ok(id) = random_hex(8) {
        headers.insert("X-EMP-Request-ID".to_owned(), id);
    }
    let base = config
        .get("codex_base_url")
        .and_then(Value::as_str)
        .unwrap_or("https://chatgpt.com/backend-api/codex")
        .trim_end_matches('/');
    let endpoint = if base.ends_with("/alpha/search") {
        base.to_owned()
    } else {
        format!("{base}/alpha/search")
    };
    let encoded = match serde_json::to_vec(&body) {
        Ok(body) => body,
        Err(_) => return request_router_error_response(400, "request body is invalid"),
    };
    let upstream =
        match state
            .backend
            .transport
            .runtime
            .block_on(state.backend.transport.client.open(
                HttpMethod::Post,
                &endpoint,
                headers,
                Some(encoded),
                false,
            )) {
            Ok(response) => response,
            Err(_) => {
                return HttpReply::error(
                    503,
                    "Cannot connect to subscription web search",
                    Some("network_error"),
                );
            }
        };
    let status = upstream.status();
    let content_type = upstream
        .header("content-type")
        .unwrap_or("application/json")
        .to_owned();
    let raw = match state
        .backend
        .transport
        .runtime
        .block_on(upstream.read_limited(64 * 1024 * 1024))
    {
        Ok(raw) => raw,
        Err(_) => {
            return HttpReply::error(
                502,
                "native search response is too large or incomplete",
                Some("protocol_error"),
            );
        }
    };
    HttpReply::new(status, raw).with_content_type(content_type)
}
