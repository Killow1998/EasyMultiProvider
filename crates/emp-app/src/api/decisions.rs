//! Forward native decision payloads using service credentials and instance authentication.

use crate::app::ServerState;
use crate::http::{
    auth::proxy_allowed,
    request::{Request, read_json_body},
    response::{body_error_response, json_error_response, response, status_text},
};
use emp_transport::HttpMethod;
use std::{collections::BTreeMap, net::TcpStream};
use url::Url;

pub(crate) fn request(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !proxy_allowed(request, state, now) {
        return json_error_response(
            401,
            status_text(401),
            "EMP gateway authentication is required",
            None,
            &[],
        );
    }
    if request.header_count("X-EMP-Provider") > 1 || request.header("X-EMP-Provider") == Some("") {
        return json_error_response(
            400,
            status_text(400),
            "X-EMP-Provider must name one configured API service",
            None,
            &[],
        );
    }
    let mut body = match read_json_body(stream, request, prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let (endpoint, key, route) = {
        let Ok(config) = state.backend.configuration.read() else {
            return json_error_response(
                500,
                status_text(500),
                "configuration is unavailable",
                None,
                &[],
            );
        };
        let Some(provider_id) = request
            .header("X-EMP-Provider")
            .or_else(|| config["decision_provider"].as_str())
            .filter(|id| !id.is_empty())
        else {
            return json_error_response(
                400,
                status_text(400),
                "Choose a decision service in Settings or send X-EMP-Provider",
                None,
                &[],
            );
        };
        let Some(provider) = config["providers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|provider| provider["id"] == provider_id && provider["enabled"] != false)
        else {
            return json_error_response(
                404,
                status_text(404),
                "Decision service is not enabled",
                None,
                &[],
            );
        };
        if provider["auth_mode"] != "api_key" || provider["execution_backend"] == "claude_cli" {
            return json_error_response(
                400,
                status_text(400),
                "Decisions requires an API key service",
                None,
                &[],
            );
        }
        if body.get("model").is_none() {
            body["model"] = provider["decision_api"]["model"].clone();
        }
        if body["model"]
            .as_str()
            .is_none_or(|model| model.trim().is_empty())
        {
            return json_error_response(
                400,
                status_text(400),
                "A decision model is required in the request or Settings",
                None,
                &[],
            );
        }
        let base = provider["base_url"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('/');
        let endpoint = match Url::parse(&format!("{base}/")).and_then(|url| {
            let api =
                provider["decision_api"]["protocol"]
                    .as_str()
                    .unwrap_or(match url.host_str() {
                        Some("api.openai.com") => "openai",
                        Some("api.typesafe.ai") => "systemone",
                        _ => "openrouter",
                    });
            url.join(match api {
                "openai" => "decisions",
                "systemone" => "systemone",
                "openrouter" => "../alpha/decisions",
                _ => unreachable!("normalized decision API"),
            })
        }) {
            Ok(url) => url.to_string(),
            Err(_) => {
                return json_error_response(
                    400,
                    status_text(400),
                    "Decision service URL is invalid",
                    None,
                    &[],
                );
            }
        };
        let key = emp_state::provider_api_key(provider, &state.backend.configuration.vault);
        if key.is_empty() {
            return json_error_response(
                503,
                status_text(503),
                "Decision service API key is unavailable",
                None,
                &[],
            );
        }
        let model = body["model"].as_str().expect("validated model");
        let route = match emp_core::resolved_route_from_parts(
            model,
            provider.as_object().expect("normalized provider").clone(),
            serde_json::from_value(serde_json::json!({"id":model,"upstream_id":model}))
                .expect("model object"),
            emp_core::RouteSource::ExplicitModel,
        ) {
            Ok(route) => route,
            Err(_) => {
                return json_error_response(
                    503,
                    status_text(503),
                    "Decision route is unavailable",
                    None,
                    &[],
                );
            }
        };
        (endpoint, key, route)
    };
    let mut incoming = request.incoming();
    if let Some(id) = request.observation_id {
        incoming.insert("X-EMP-Request-ID".into(), id.into());
    }
    let mut outcome = crate::services::request_outcome::RequestOutcome::new(
        state,
        &route,
        &body,
        &incoming,
        None,
        "decisions",
    );
    outcome.dispatch();
    let mut headers = BTreeMap::from([
        ("Authorization".to_owned(), format!("Bearer {key}")),
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
    ]);
    for name in [
        "TypeSafe-Danger-Zone-Long-Context",
        "TypeSafe-Danger-Zone-Images",
    ] {
        if let Some(value) = request.header(name) {
            headers.insert(name.to_owned(), value.to_owned());
        }
    }
    let transport = &state.backend.transport;
    use crate::services::disconnect::{DisconnectMonitor, DisconnectRace, raced};
    let mut monitor = DisconnectMonitor::start(stream).ok();
    let upstream = match raced(
        &transport.runtime,
        monitor.as_mut(),
        transport.client.open(
            HttpMethod::Post,
            &endpoint,
            headers,
            Some(serde_json::to_vec(&body).expect("JSON request")),
            false,
        ),
    ) {
        DisconnectRace::Ready(Ok(upstream)) => upstream,
        DisconnectRace::Disconnected => {
            outcome.disconnected();
            return Vec::new();
        }
        DisconnectRace::Ready(Err(_)) => {
            outcome.status(503, "network_error");
            return json_error_response(
                503,
                status_text(503),
                "Cannot connect to the decision service",
                Some("network_error"),
                &[],
            );
        }
    };
    let status = upstream.status();
    outcome.http_status(status);
    let content_type = upstream
        .header("content-type")
        .unwrap_or("application/json")
        .to_owned();
    let retry = upstream.header("retry-after").map(str::to_owned);
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    let raw = match raced(
        &transport.runtime,
        monitor.as_mut(),
        upstream.read_limited(emp_transport::MAX_PROXY_REQUEST_BYTES),
    ) {
        DisconnectRace::Ready(Ok(raw)) => raw,
        DisconnectRace::Disconnected => {
            outcome.disconnected();
            return Vec::new();
        }
        DisconnectRace::Ready(Err(_)) => {
            outcome.status(502, "protocol_error");
            return json_error_response(
                502,
                status_text(502),
                "Decisions response is incomplete or too large",
                Some("protocol_error"),
                &[],
            );
        }
    };
    if let Ok(mut reported) = serde_json::from_slice::<serde_json::Value>(&raw) {
        if let Some(usage) = reported
            .get_mut("usage")
            .and_then(serde_json::Value::as_object_mut)
        {
            for (canonical, alternate) in [
                ("input_tokens", "prompt_tokens"),
                ("output_tokens", "completion_tokens"),
            ] {
                if !usage.contains_key(canonical)
                    && let Some(value) = usage.get(alternate).cloned()
                {
                    usage.insert(canonical.into(), value);
                }
            }
        }
        outcome.observe_upstream(&reported);
        // HTTP status is authoritative, including bodies that contain a status field.
        outcome.http_status(status);
    }
    outcome.finish();
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        &content_type,
        &raw,
        &headers,
    )
}
