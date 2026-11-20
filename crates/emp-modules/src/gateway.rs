use crate::GatewayIdentity;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Request {
    Identify {
        digest: String,
        candidates: Vec<GatewayIdentity>,
    },
    Create {
        name: String,
    },
    Replace {
        id: String,
        device_ids: Vec<String>,
    },
    Remove {
        id: String,
        device_ids: Vec<String>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Caller {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub struct CreatePlan {
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub struct ReplacePlan {
    pub index: usize,
}

#[derive(Serialize, Deserialize)]
pub struct RemovePlan {
    pub keep: Vec<usize>,
}
