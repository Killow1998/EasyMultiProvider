use emp_modules::Command;
use emp_modules::decision::{
    ContextHeader, DiscoveryPlan, ModelChoice, ModelInput, PlanError, PlanReply, Protocol,
    ProtocolPreference, Request, RequestPlan, Service, ServiceAccess,
};
use serde_json::{Value, json};
use url::Url;

pub fn handle(command: Command) -> Result<Value, &'static str> {
    Ok(match command {
        Command::DecisionEndpoint { base, protocol } => {
            let (_, endpoint) = decision_endpoint(&base, protocol.as_deref().into()).map_err(
                |error| match error {
                    PlanError::InvalidProtocol => "invalid_decision_protocol",
                    _ => "invalid_decision_url",
                },
            )?;
            json!(endpoint.as_str())
        }
        Command::Decision { request } => match request {
            Request::Plan {
                default_service,
                override_service,
                model,
                context,
            } => json!(plan_request(
                override_service.or(default_service),
                model,
                &context
            )),
            Request::Discover { base, protocol } => json!(plan_discovery(&base, protocol)),
        },
        _ => return Err("invalid_module_command"),
    })
}

fn plan_request(
    service: Option<Service>,
    model: ModelInput,
    context: &[ContextHeader],
) -> PlanReply {
    let Service { id, access } = service.ok_or(PlanError::ProviderRequired)?;
    let (base, protocol, default_model) = match access {
        ServiceAccess::Workspace => return Err(PlanError::WorkspaceProvider),
        ServiceAccess::Disabled => return Err(PlanError::ProviderDisabled),
        ServiceAccess::Unsupported => return Err(PlanError::ApiServiceRequired),
        ServiceAccess::ApiKey {
            base,
            protocol,
            default_model,
        } => (base, protocol, default_model),
    };
    let model = match model {
        ModelInput::Explicit => ModelChoice::Explicit,
        ModelInput::Invalid => return Err(PlanError::ModelRequired),
        ModelInput::Omitted => ModelChoice::Default(
            default_model
                .filter(|model| !model.trim().is_empty())
                .ok_or(PlanError::ModelRequired)?,
        ),
    };
    let (protocol, _) = decision_endpoint(&base, protocol)?;
    let headers = ContextHeader::ALL
        .into_iter()
        .filter(|header| context.contains(header))
        .collect();
    Ok(RequestPlan {
        provider: id,
        protocol,
        model,
        headers,
    })
}

fn decision_endpoint(
    base: &str,
    protocol: ProtocolPreference,
) -> Result<(Protocol, Url), PlanError> {
    let base = Url::parse(&format!("{}/", base.trim_end_matches('/')))
        .map_err(|_| PlanError::InvalidUrl)?;
    let protocol = match protocol {
        ProtocolPreference::Explicit(protocol) => protocol,
        ProtocolPreference::Invalid => return Err(PlanError::InvalidProtocol),
        ProtocolPreference::Infer => match base.host_str() {
            Some("api.openai.com") => Protocol::OpenAi,
            Some("api.typesafe.ai") => Protocol::SystemOne,
            _ => Protocol::OpenRouter,
        },
    };
    let endpoint = base
        .join(protocol.endpoint_path())
        .map_err(|_| PlanError::InvalidUrl)?;
    Ok((protocol, endpoint))
}

fn plan_discovery(base: &str, protocol: Protocol) -> DiscoveryPlan {
    let check_key = Url::parse(base.trim_end_matches('/'))
        .ok()
        .is_some_and(|url| url.host_str() == Some("openrouter.ai"));
    DiscoveryPlan {
        check_key,
        filter_decisions: protocol == Protocol::OpenRouter || check_key,
    }
}
