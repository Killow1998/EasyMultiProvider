use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::error::HttpReply;
use crate::services::catalog::discovery::DiscoveryError;
use emp_modules::decision::{
    self, ContextHeader, DiscoveryPlan, ModelChoice, ModelInput, PlanError, PlanReply, Protocol,
    Service, ServiceAccess,
};
use emp_modules::{Command, Module};
use emp_router::discovery::discover_decision_models_with_plan;
use emp_transport::HttpMethod;
use serde_json::{Value, json};
use std::{collections::BTreeMap, net::TcpStream};
use url::Url;

pub(crate) fn validate_headers(request: Request<'_>) -> Result<(), HttpReply> {
    if request.header_count("X-EMP-Provider") > 1 || request.header("X-EMP-Provider") == Some("") {
        return Err(HttpReply::error(
            400,
            "X-EMP-Provider must name one configured API service",
            None,
        ));
    }
    Ok(())
}

pub(crate) fn execute(
    socket: &TcpStream,
    request: Request<'_>,
    mut body: Value,
    state: &ServerState,
) -> Option<HttpReply> {
    let provider_override = request.header("X-EMP-Provider");
    let model_input = match body.get("model") {
        None => ModelInput::Omitted,
        Some(Value::String(model)) if !model.trim().is_empty() => ModelInput::Explicit,
        Some(_) => ModelInput::Invalid,
    };
    let context = ContextHeader::ALL
        .into_iter()
        .filter(|header| request.header(header.name()).is_some())
        .collect();
    let (endpoint, key, route, planned_headers) = {
        let Ok(config) = state.backend.configuration.read() else {
            return Some(HttpReply::error(500, "configuration is unavailable", None));
        };
        let default = config["decision_provider"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(|id| service_snapshot(state, &config, id));
        let override_service = provider_override.map(|id| service_snapshot(state, &config, id));
        let provider = override_service
            .as_ref()
            .or(default.as_ref())
            .and_then(|(_, provider)| *provider);
        let plan = match state.backend.modules.call_typed::<PlanReply>(
            Module::Decision,
            Command::Decision {
                request: decision::Request::Plan {
                    default_service: default.map(|(service, _)| service),
                    override_service: override_service.map(|(service, _)| service),
                    model: model_input,
                    context,
                },
            },
        ) {
            Ok(Ok(plan)) => plan,
            Ok(Err(error)) => return Some(plan_error_response(error)),
            Err(error) => return Some(endpoint_error_response(503, &error)),
        };
        let Some(provider) = provider.filter(|provider| provider["id"] == plan.provider) else {
            return Some(endpoint_error_response(503, "invalid_module_reply"));
        };
        match (model_input, plan.model) {
            (ModelInput::Explicit, ModelChoice::Explicit) => {}
            (ModelInput::Omitted, ModelChoice::Default(model))
                if provider["decision_api"]["model"] == model =>
            {
                body["model"] = Value::String(model);
            }
            _ => return Some(endpoint_error_response(503, "invalid_module_reply")),
        }
        let base = provider["base_url"]
            .as_str()
            .expect("normalized provider URL");
        let endpoint = match Url::parse(&format!("{}/", base.trim_end_matches('/')))
            .and_then(|url| url.join(plan.protocol.endpoint_path()))
        {
            Ok(endpoint) => endpoint,
            Err(_) => return Some(endpoint_error_response(400, "invalid_decision_url")),
        };
        let key = emp_state::provider_api_key(provider, &state.backend.configuration.vault);
        if key.is_empty() {
            return Some(HttpReply::error(
                503,
                "Decision service API key is unavailable",
                None,
            ));
        }
        let model = body["model"].as_str().expect("validated model");
        let route = match emp_domain::resolved_route_from_parts(
            model,
            provider.as_object().expect("normalized provider").clone(),
            serde_json::from_value(serde_json::json!({"id":model,"upstream_id":model}))
                .expect("model object"),
            emp_domain::RouteSource::ExplicitModel,
        ) {
            Ok(route) => route,
            Err(_) => {
                return Some(HttpReply::error(503, "Decision route is unavailable", None));
            }
        };
        (endpoint, key, route, plan.headers)
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
    for header in planned_headers {
        let name = header.name();
        if let Some(value) = request.header(name) {
            headers.insert(name.to_owned(), value.to_owned());
        }
    }
    let transport = &state.backend.transport;
    use crate::services::disconnect::{DisconnectMonitor, DisconnectRace, raced};
    let mut monitor = DisconnectMonitor::start(socket).ok();
    let upstream = match raced(
        &transport.runtime,
        monitor.as_mut(),
        transport.client.open(
            HttpMethod::Post,
            endpoint.as_str(),
            headers,
            Some(serde_json::to_vec(&body).expect("JSON request")),
            false,
        ),
    ) {
        DisconnectRace::Ready(Ok(upstream)) => upstream,
        DisconnectRace::Disconnected => {
            outcome.disconnected();
            return None;
        }
        DisconnectRace::Ready(Err(_)) => {
            outcome.status(503, "network_error");
            return Some(HttpReply::error(
                503,
                "Cannot connect to the decision service",
                Some("network_error"),
            ));
        }
    };
    let status = upstream.status();
    outcome.http_status(status);
    let content_type = upstream
        .header("content-type")
        .unwrap_or("application/json")
        .to_owned();
    let retry = upstream.header("retry-after").map(str::to_owned);
    let raw = match raced(
        &transport.runtime,
        monitor.as_mut(),
        upstream.read_limited(emp_transport::MAX_PROXY_REQUEST_BYTES),
    ) {
        DisconnectRace::Ready(Ok(raw)) => raw,
        DisconnectRace::Disconnected => {
            outcome.disconnected();
            return None;
        }
        DisconnectRace::Ready(Err(_)) => {
            outcome.status(502, "protocol_error");
            return Some(HttpReply::error(
                502,
                "Decisions response is incomplete or too large",
                Some("protocol_error"),
            ));
        }
    };
    if let Ok(mut reported) = serde_json::from_slice::<Value>(&raw) {
        if let Some(usage) = reported.get_mut("usage").and_then(Value::as_object_mut) {
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
        outcome.http_status(status);
    }
    outcome.finish();
    let mut reply = HttpReply::new(status, raw).with_content_type(content_type);
    if let Some(retry) = retry {
        reply = reply.with_header("Retry-After", retry);
    }
    Some(reply)
}

fn service_snapshot<'a>(
    state: &ServerState,
    config: &'a Value,
    id: &str,
) -> (Service, Option<&'a Value>) {
    if state.backend.team.shared_subscription(id) {
        return (
            Service {
                id: id.into(),
                access: ServiceAccess::Workspace,
            },
            None,
        );
    }
    let provider = config["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|provider| provider["id"] == id && provider["enabled"] != false);
    let (access, provider) = match provider {
        None => (ServiceAccess::Disabled, None),
        Some(provider)
            if provider["auth_mode"] != "api_key"
                || provider["execution_backend"] == "claude_cli" =>
        {
            (ServiceAccess::Unsupported, None)
        }
        Some(provider) => (
            ServiceAccess::ApiKey {
                base: provider["base_url"]
                    .as_str()
                    .expect("normalized provider URL")
                    .to_owned(),
                protocol: provider["decision_api"]["protocol"].as_str().into(),
                default_model: provider["decision_api"]["model"]
                    .as_str()
                    .map(str::to_owned),
            },
            Some(provider),
        ),
    };
    (
        Service {
            id: id.into(),
            access,
        },
        provider,
    )
}

fn plan_error_response(error: PlanError) -> HttpReply {
    let (status, message, code) = match error {
        PlanError::ProviderRequired => (
            400,
            "Choose a decision service in Settings or send X-EMP-Provider",
            None,
        ),
        PlanError::WorkspaceProvider => (
            403,
            "Decision requires a personal API service",
            Some("workspace_decision_provider_denied"),
        ),
        PlanError::ProviderDisabled => (404, "Decision service is not enabled", None),
        PlanError::ApiServiceRequired => (400, "Decisions requires an API key service", None),
        PlanError::ModelRequired => (
            400,
            "A decision model is required in the request or Settings",
            None,
        ),
        PlanError::InvalidUrl => return endpoint_error_response(400, "invalid_decision_url"),
        PlanError::InvalidProtocol => {
            return endpoint_error_response(503, "invalid_decision_protocol");
        }
    };
    HttpReply::error(status, message, code)
}

fn endpoint_error_response(status: u16, code: &str) -> HttpReply {
    HttpReply::error(status, "Decision endpoint is unavailable", Some(code))
}

pub(crate) struct DecisionDiscovery {
    provider_id: String,
    protocol: Option<Protocol>,
}

impl DecisionDiscovery {
    pub(crate) fn parse(body: &Value) -> Result<Self, DiscoveryError> {
        let provider_id = body["provider"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| DiscoveryError::Invalid("provider is required".into()))?;
        Ok(Self {
            provider_id: provider_id.to_owned(),
            protocol: body["protocol"].as_str().and_then(Protocol::parse),
        })
    }
}

pub(crate) fn discover(
    state: &ServerState,
    operation: DecisionDiscovery,
) -> Result<Value, DiscoveryError> {
    let id = operation.provider_id.as_str();
    if state.backend.team.shared_subscription(id) {
        return Err(DiscoveryError::Invalid(
            "choose a personal HTTP API service".into(),
        ));
    }
    let protocol = operation
        .protocol
        .ok_or_else(|| DiscoveryError::Invalid("decision API type is invalid".into()))?;
    let (base, key) = {
        let config = state
            .backend
            .configuration
            .read()
            .map_err(|_| DiscoveryError::Unavailable)?;
        let (service, provider) = service_snapshot(state, &config, id);
        let base = match service.access {
            ServiceAccess::ApiKey { base, .. } => base,
            ServiceAccess::Disabled => {
                return Err(DiscoveryError::Invalid(format!(
                    "provider is missing or disabled: {id}"
                )));
            }
            ServiceAccess::Unsupported => {
                return Err(DiscoveryError::Invalid("choose an HTTP API service".into()));
            }
            ServiceAccess::Workspace => {
                return Err(DiscoveryError::Invalid(
                    "choose a personal HTTP API service".into(),
                ));
            }
        };
        let provider = provider.expect("configured API service");
        let key = emp_state::provider_api_key(provider, &state.backend.configuration.vault);
        if key.is_empty() {
            return Err(DiscoveryError::Invalid(
                "provider API key is not configured".into(),
            ));
        }
        (base, key)
    };
    let plan = state
        .backend
        .modules
        .call_typed::<DiscoveryPlan>(
            Module::Decision,
            Command::Decision {
                request: decision::Request::Discover {
                    base: base.clone(),
                    protocol,
                },
            },
        )
        .map_err(|_| DiscoveryError::Unavailable)?;
    let transport = &state.backend.transport;
    let models = transport
        .runtime
        .block_on(discover_decision_models_with_plan(
            &transport.client,
            &base,
            &key,
            protocol.as_str(),
            plan.check_key,
            plan.filter_decisions,
        ))
        .map_err(DiscoveryError::Upstream)?;
    Ok(json!({"models":models}))
}
