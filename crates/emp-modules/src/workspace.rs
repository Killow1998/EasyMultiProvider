pub use emp_domain::MAX_USAGE_MODEL_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const NATIVE_SHARE: &str = "subscription:@native";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    VpsAdmin,
    TeamAdmin,
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VpsAdmin => "vps_admin",
            Self::TeamAdmin => "team_admin",
            Self::Member => "member",
        }
    }

    pub fn can_manage(self) -> bool {
        self != Self::Member
    }

    pub fn manages(self, target: Self) -> bool {
        match self {
            Self::VpsAdmin => target != Self::VpsAdmin,
            Self::TeamAdmin => target == Self::Member,
            Self::Member => false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SharePayload {
    pub provider: Value,
    pub models: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<SubscriptionSnapshot>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SubscriptionSnapshot {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    pub account_id: String,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl SubscriptionSnapshot {
    pub fn auth_document(&self) -> Value {
        serde_json::json!({"auth_mode":"chatgptAuthTokens","tokens":{
            "access_token":self.access_token,"account_id":self.account_id,
            "id_token":self.id_token.as_deref().unwrap_or(&self.access_token),"refresh_token":""
        }})
    }
}

impl SharePayload {
    pub fn valid(&self) -> bool {
        self.subscription.as_ref().is_some_and(|auth| {
            self.provider["auth_mode"] == "codex_subscription"
                && self.provider["id"] == NATIVE_SHARE
                && self.provider["name"] == "Native"
                && self
                    .provider
                    .as_object()
                    .is_some_and(|fields| fields.len() == 3)
                && self.models.is_empty()
                && !auth.access_token.is_empty()
                && auth.access_token.len() <= 32_768
                && auth
                    .access_token
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
                && auth
                    .id_token
                    .as_ref()
                    .is_none_or(|token| token.len() <= 32_768)
                && !auth.account_id.is_empty()
                && auth.account_id.len() <= 128
                && auth.expires_at > 0
                && auth.plan.as_deref().is_none_or(|plan| {
                    matches!(
                        plan,
                        "free" | "go" | "plus" | "pro" | "prolite" | "promax" | "business"
                    )
                })
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct SharedService {
    pub id: String,
    pub version: i64,
    pub payload: SharePayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Publish {
    pub source: String,
    pub expected_version: i64,
    pub payload: SharePayload,
}

#[derive(Serialize, Deserialize)]
pub struct UsageEvent {
    pub id: String,
    pub share: String,
    pub model: String,
    pub finished_at: i64,
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub estimated_usd_nanos: Option<i64>,
    pub actual_usd_nanos: Option<i64>,
}
