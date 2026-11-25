use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub const PROTOCOL: u8 = 1;
pub const HOST_API: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Module {
    Decision,
    Gateway,
    Aggregate,
    Team,
    Aero,
}

impl Module {
    pub const STANDARD: [Self; 4] = [Self::Decision, Self::Gateway, Self::Aggregate, Self::Team];
    pub const ALL: [Self; 5] = [
        Self::Decision,
        Self::Gateway,
        Self::Aggregate,
        Self::Team,
        Self::Aero,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Gateway => "gateway",
            Self::Aggregate => "aggregate",
            Self::Team => "team",
            Self::Aero => "aero",
        }
    }

    pub const fn version(self) -> &'static str {
        match self {
            Self::Decision => "0.10-beta",
            Self::Gateway => "0.10-beta",
            Self::Aggregate => "0.10-beta",
            Self::Team => "0.10.2-beta",
            Self::Aero => "0.10.0-beta",
        }
    }

    pub fn asset(self) -> String {
        self.asset_for(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn asset_for(self, os: &str, arch: &str) -> String {
        let suffix = if os == "windows" { ".exe" } else { "" };
        format!("EMP-mod-{}-{os}-{arch}{suffix}", self.id())
    }
}

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub module: Module,
    pub version: String,
    #[serde(default)]
    pub module_version: Option<String>,
    pub protocol: u8,
    #[serde(default)]
    pub host_api: Option<u8>,
}

#[derive(Serialize, Deserialize)]
pub struct Candidate {
    pub capacity: Option<f64>,
    pub plan: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct GatewayIdentity {
    pub id: String,
    pub name: String,
    pub digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    ReadPanel,
    AeroStart {
        state_dir: std::path::PathBuf,
        core_base_url: String,
        inbound_key: String,
        upstream_key: String,
        profiles: Value,
    },
    AeroConfigure {
        profiles: Value,
    },
    Decision {
        request: decision::Request,
    },
    Gateway {
        request: gateway::Request,
    },
    Aggregate {
        request: aggregate::Request,
    },
    Team {
        request: team::Request,
    },
    DecisionEndpoint {
        base: String,
        protocol: Option<String>,
    },
    GatewayIdentify {
        digest: String,
        candidates: Vec<GatewayIdentity>,
    },
    AggregateOrder {
        policy: String,
        candidates: Vec<Candidate>,
    },
    TeamEndpoint {
        base: String,
        operation: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub value: Value,
    pub error: Option<String>,
}

pub fn worker(
    module: Module,
    version: &str,
    legacy_core: Option<&str>,
    panel: &str,
    handle: fn(Command) -> Result<Value, &'static str>,
) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(
        &mut out,
        &Hello {
            module,
            version: legacy_core.unwrap_or(version).into(),
            module_version: Some(version.replace(".0-beta", "-beta")),
            protocol: PROTOCOL,
            host_api: Some(HOST_API),
        },
    )?;
    out.write_all(b"\n")?;
    out.flush()?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let result = match serde_json::from_str(&line) {
            Ok(Command::ReadPanel) => Ok(json!(panel)),
            Ok(command) => handle(command),
            Err(_) => Err("invalid_module_command"),
        };
        let response = match result {
            Ok(value) => Reply { value, error: None },
            Err(error) => Reply {
                value: json!(null),
                error: Some(error.into()),
            },
        };
        serde_json::to_writer(&mut out, &response)?;
        out.write_all(b"\n")?;
        out.flush()?;
    }
    Ok(())
}

pub mod aggregate;
pub mod decision;
pub mod gateway;
pub mod team;
pub mod workspace;
