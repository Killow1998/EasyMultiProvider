use crate::manifest::{HostApi, ModuleManifest};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::{num::NonZeroU16, path::PathBuf};

pub const INITIALIZE: &str = "module.initialize";
pub const ROUTE_ID_HEADER: &str = "x-emp-route-id";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Initialize {
    pub host_api: HostApi,
    pub state_dir: PathBuf,
    pub callback_port: Option<NonZeroU16>,
    pub module_bearer: String,
    pub core_bearer: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Initialized {
    #[serde(deserialize_with = "manifest")]
    pub manifest: ModuleManifest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_port: Option<NonZeroU16>,
}

fn manifest<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ModuleManifest, D::Error> {
    ModuleManifest::from_value(serde_json::Value::deserialize(deserializer)?)
        .map_err(de::Error::custom)
}
