use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    OpenAi,
    OpenRouter,
    SystemOne,
}

impl Protocol {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "openai" => Some(Self::OpenAi),
            "openrouter" => Some(Self::OpenRouter),
            "systemone" => Some(Self::SystemOne),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::OpenRouter => "openrouter",
            Self::SystemOne => "systemone",
        }
    }

    pub const fn endpoint_path(self) -> &'static str {
        match self {
            Self::OpenAi => "decisions",
            Self::OpenRouter => "../alpha/decisions",
            Self::SystemOne => "systemone",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolPreference {
    Infer,
    Explicit(Protocol),
    Invalid,
}

impl From<Option<&str>> for ProtocolPreference {
    fn from(value: Option<&str>) -> Self {
        match value {
            None => Self::Infer,
            Some(value) => Protocol::parse(value).map_or(Self::Invalid, Self::Explicit),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Service {
    pub id: String,
    pub access: ServiceAccess,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAccess {
    Workspace,
    Disabled,
    Unsupported,
    ApiKey {
        base: String,
        protocol: ProtocolPreference,
        default_model: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInput {
    Omitted,
    Explicit,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHeader {
    LongContext,
    Images,
}

impl ContextHeader {
    pub const ALL: [Self; 2] = [Self::LongContext, Self::Images];

    pub const fn name(self) -> &'static str {
        match self {
            Self::LongContext => "TypeSafe-Danger-Zone-Long-Context",
            Self::Images => "TypeSafe-Danger-Zone-Images",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Request {
    Plan {
        default_service: Option<Service>,
        override_service: Option<Service>,
        model: ModelInput,
        context: Vec<ContextHeader>,
    },
    Discover {
        base: String,
        protocol: Protocol,
    },
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelChoice {
    Explicit,
    Default(String),
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct RequestPlan {
    pub provider: String,
    pub protocol: Protocol,
    pub model: ModelChoice,
    pub headers: Vec<ContextHeader>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanError {
    ProviderRequired,
    WorkspaceProvider,
    ProviderDisabled,
    ApiServiceRequired,
    ModelRequired,
    InvalidUrl,
    InvalidProtocol,
}

pub type PlanReply = Result<RequestPlan, PlanError>;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DiscoveryPlan {
    pub check_key: bool,
    pub filter_decisions: bool,
}
