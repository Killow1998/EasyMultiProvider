use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    Priority,
    Balanced,
    RoundRobin,
}

impl Policy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "priority" => Some(Self::Priority),
            "balanced" => Some(Self::Balanced),
            "round_robin" => Some(Self::RoundRobin),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Subscription,
    Provider,
    NativeApiKey,
    LocalNativeApiKey,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Candidate {
    pub id: usize,
    pub owner_id: usize,
    pub source: Source,
    pub cooling_down: bool,
    pub headroom: Option<f64>,
    pub subscription_plan: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Request {
    pub policy: Policy,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct BindingPlan {
    pub candidate_ids: Vec<usize>,
    pub rotate: bool,
}
