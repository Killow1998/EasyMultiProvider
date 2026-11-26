use crate::manifest::{HostApi, ModuleManifest};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
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

#[derive(Serialize)]
pub struct Initialized {
    pub manifest: ModuleManifest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_port: Option<NonZeroU16>,
}

impl<'de> Deserialize<'de> for Initialized {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            manifest: Value,
            data_port: Option<NonZeroU16>,
        }

        let fields = crate::reply_object(crate::strict_json::deserialize(deserializer)?)
            .map_err(de::Error::custom)?;
        let Fields {
            manifest,
            data_port,
        } = Fields::deserialize(Value::Object(fields)).map_err(de::Error::custom)?;
        let manifest = ModuleManifest::from_value(manifest).map_err(de::Error::custom)?;
        Ok(Self {
            manifest,
            data_port,
        })
    }
}
