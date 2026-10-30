use super::ConfigError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdaptiveEffort {
    pub enabled: bool,
    pub classifier_model: String,
    pub max_effort: Effort,
}

impl Default for AdaptiveEffort {
    fn default() -> Self {
        Self {
            enabled: false,
            classifier_model: "gpt-6-luna".into(),
            max_effort: Effort::Xhigh,
        }
    }
}

pub(super) fn normalize(value: Option<&Value>) -> Result<Value, ConfigError> {
    let setting: AdaptiveEffort = match value {
        None => AdaptiveEffort::default(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|error| ConfigError::new(format!("adaptive_effort: {error}")))?,
    };
    if setting.max_effort == Effort::Low {
        return Err(ConfigError::new(
            "adaptive_effort.max_effort must allow at least medium",
        ));
    }
    if !setting.classifier_model.starts_with("gpt-")
        || setting.classifier_model.len() > 100
        || !setting
            .classifier_model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
    {
        return Err(ConfigError::new(
            "adaptive_effort.classifier_model must be an upstream GPT model ID",
        ));
    }
    Ok(serde_json::to_value(setting).expect("adaptive effort settings"))
}
