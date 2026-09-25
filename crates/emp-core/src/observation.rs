use super::*;

fn safe_token(value: &str) -> String {
    let normalized: String = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    normalized.trim_end_matches('_').to_string()
}

/// A content-free public failure safe for clients and diagnostics.
///
/// `status` is optional because a client disconnect may not have an HTTP
/// response boundary.  Unknown context observations remain opaque.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicFailure {
    pub error_class: String,
    pub status: Option<u16>,
    pub phase: Option<String>,
    pub failure_reason: Option<String>,
    pub public_message: Option<String>,
    pub retry_after_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_observation: Option<OpaqueJson>,
}

impl PublicFailure {
    /// Construct a normalized failure.  Failure reasons use the existing
    /// 64-character safe token policy; messages remain bounded but unchanged.
    pub fn new(
        error_class: impl Into<String>,
        status: Option<u16>,
        phase: Option<String>,
        failure_reason: Option<String>,
        public_message: Option<String>,
        retry_after_seconds: Option<u32>,
        context_observation: Option<Map<String, Value>>,
    ) -> Result<Self, ValidationError> {
        let error_class = safe_token(&error_class.into());
        if error_class.is_empty() {
            return Err(ValidationError::EmptyField("error_class"));
        }
        if status.is_some_and(|value| !(100..=599).contains(&value)) {
            return Err(ValidationError::InvalidStatus);
        }
        let failure_reason = failure_reason
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(safe_token)
            .filter(|value| !value.is_empty());
        let public_message = public_message
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.chars().take(512).collect());
        let phase = phase
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(safe_token)
            .filter(|value| !value.is_empty());
        let context_observation = context_observation.map(OpaqueJson::new).transpose()?;
        Ok(Self {
            error_class,
            status,
            phase,
            failure_reason,
            public_message,
            retry_after_seconds,
            context_observation,
        })
    }
}

/// Terminal stream facts shared by SSE and WebSocket adapters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamObservation {
    pub phase: String,
    pub duration_ms: u64,
    pub first_event_ms: Option<u64>,
    pub retry_count: u32,
    pub output_emitted: bool,
    pub tool_activity: bool,
    pub upstream_event_observed: bool,
    pub terminal_event_observed: bool,
    pub terminal: Option<PublicFailure>,
    #[serde(default)]
    pub unknown_fields: OpaqueJson,
}

impl StreamObservation {
    /// Construct an observation.  A terminal event must have a classified
    /// terminal value so stream truth cannot be silently discarded.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        phase: impl Into<String>,
        duration_ms: u64,
        first_event_ms: Option<u64>,
        retry_count: u32,
        output_emitted: bool,
        tool_activity: bool,
        upstream_event_observed: bool,
        terminal_event_observed: bool,
        terminal: Option<PublicFailure>,
        unknown_fields: Map<String, Value>,
    ) -> Result<Self, ValidationError> {
        if terminal_event_observed && terminal.is_none() {
            return Err(ValidationError::EmptyField("terminal"));
        }
        let phase = phase.into();
        if phase.is_empty() {
            return Err(ValidationError::EmptyField("phase"));
        }
        Ok(Self {
            phase,
            duration_ms,
            first_event_ms,
            retry_count,
            output_emitted,
            tool_activity,
            upstream_event_observed,
            terminal_event_observed,
            terminal,
            unknown_fields: OpaqueJson::new(unknown_fields)?,
        })
    }
}
