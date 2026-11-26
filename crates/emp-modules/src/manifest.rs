use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

pub const HOST_API_V3: HostApi = HostApi { major: 3, minor: 0 };

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostApi {
    pub major: u16,
    pub minor: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiRequirement {
    pub major: u16,
    pub min_minor: u16,
}

impl ApiRequirement {
    pub const fn accepts(&self, host: HostApi) -> bool {
        self.major == host.major && self.min_minor <= host.minor
    }
}

/// Decode a manifest with [`Self::from_json`] to enforce its JSON object shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModuleManifest {
    pub id: String,
    pub module_version: String,
    pub host_api: ApiRequirement,
    pub contributions: Vec<Contribution>,
}

#[derive(Debug)]
pub enum ManifestDecodeError {
    Json(serde_json::Error),
    ManifestNotObject,
    HostApiNotObject,
    ContributionNotObject { index: usize },
    ContributionFieldNotString { index: usize, field: &'static str },
}

impl fmt::Display for ManifestDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => error.fmt(f),
            Self::ManifestNotObject => f.write_str("模组 manifest 必须是 JSON 对象"),
            Self::HostApiNotObject => f.write_str("host_api 必须是 JSON 对象"),
            Self::ContributionNotObject { index } => {
                write!(f, "贡献[{index}] 必须是 JSON 对象")
            }
            Self::ContributionFieldNotString { index, field } => {
                write!(f, "贡献[{index}] 的 {field} 必须是字符串")
            }
        }
    }
}

impl std::error::Error for ManifestDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Contribution<RouteId = String> {
    HttpRoute {
        id: RouteId,
        method: String,
        path: String,
        auth: RouteAuth,
    },
    Hook {
        phase: HookPhase,
    },
    #[serde(deserialize_with = "deserialize_catalog")]
    Catalog,
    Panel {
        id: String,
        entry: String,
        revision: String,
    },
}

fn deserialize_catalog<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Catalog {}

    Catalog::deserialize(deserializer).map(|_| ())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAuth {
    Management,
    Proxy,
    LocalProxy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    Authenticate,
    ProjectAccounts,
    ResolveRoute,
    CheckAccess,
    ProjectCatalog,
    CallFinished,
}

impl HookPhase {
    pub const fn method(self) -> &'static str {
        match self {
            Self::Authenticate => "module.auth.authenticate",
            Self::ProjectAccounts => "module.accounts.project",
            Self::ResolveRoute => "module.route.resolve",
            Self::CheckAccess => "module.access.check",
            Self::ProjectCatalog => "module.catalog.project",
            Self::CallFinished => "module.call.finished",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ManifestError {
    InvalidModuleId {
        id: String,
    },
    EmptyModuleVersion,
    IncompatibleHostApi {
        required: ApiRequirement,
        host: HostApi,
    },
    DuplicateContributionId {
        index: usize,
        id: String,
    },
    InvalidRouteId {
        index: usize,
        id: String,
    },
    InvalidRouteMethod {
        index: usize,
        id: String,
        method: String,
    },
    InvalidRoutePath {
        index: usize,
        id: String,
        path: String,
    },
    DuplicateRoute {
        index: usize,
        id: String,
        method: String,
        path: String,
    },
    DuplicateHook {
        index: usize,
        phase: HookPhase,
    },
    DuplicateCatalog {
        index: usize,
    },
    InvalidPanelEntry {
        index: usize,
        id: String,
        entry: String,
    },
    EmptyPanelRevision {
        index: usize,
        id: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidModuleId { id } => {
                write!(f, "模组 ID {id:?} 必须匹配 ^[a-z][a-z0-9-]{{0,31}}$")
            }
            Self::EmptyModuleVersion => f.write_str("模组版本不能为空"),
            Self::IncompatibleHostApi { required, host } => write!(
                f,
                "模组要求宿主 API 主版本为 {} 且次版本至少为 {}，当前为 {}.{}",
                required.major, required.min_minor, host.major, host.minor
            ),
            Self::DuplicateContributionId { index, id } => {
                write!(f, "贡献[{index}] 的 ID {id:?} 已声明")
            }
            Self::InvalidRouteId { index, id } => {
                write!(
                    f,
                    "贡献[{index}] 的路由 ID {id:?} 必须匹配 ^[A-Za-z0-9._-]{{1,64}}$"
                )
            }
            Self::InvalidRouteMethod { index, id, method } => {
                write!(
                    f,
                    "贡献[{index}] 路由 {id:?} 的 HTTP 方法 {method:?} 不受支持"
                )
            }
            Self::InvalidRoutePath { index, id, path } => {
                write!(f, "贡献[{index}] 路由 {id:?} 的路径 {path:?} 无效")
            }
            Self::DuplicateRoute {
                index,
                id,
                method,
                path,
            } => {
                write!(f, "贡献[{index}] 路由 {id:?} 的 {method} {path:?} 已声明")
            }
            Self::DuplicateHook { index, phase } => {
                write!(f, "贡献[{index}] 的钩子 {} 已声明", phase.method())
            }
            Self::DuplicateCatalog { index } => write!(f, "贡献[{index}] 重复声明目录"),
            Self::InvalidPanelEntry { index, id, entry } => write!(
                f,
                "贡献[{index}] 面板 {id:?} 的入口 {entry:?} 必须是以 .js 结尾的相对路径，每段只含 [A-Za-z0-9._-] 且不能只由点组成"
            ),
            Self::EmptyPanelRevision { index, id } => {
                write!(f, "贡献[{index}] 面板 {id:?} 的修订版本不能为空")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

impl ModuleManifest {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ManifestDecodeError> {
        Self::from_value(crate::strict_json::from_slice(bytes).map_err(ManifestDecodeError::Json)?)
    }

    /// Use a value from an input parsed with duplicate keys rejected, such as a control frame.
    pub fn from_value(value: Value) -> Result<Self, ManifestDecodeError> {
        #[derive(Deserialize)]
        #[serde(remote = "ModuleManifest", deny_unknown_fields)]
        struct ManifestFields {
            id: String,
            module_version: String,
            host_api: ApiRequirement,
            contributions: Vec<Contribution>,
        }

        let manifest = value
            .as_object()
            .ok_or(ManifestDecodeError::ManifestNotObject)?;
        if !manifest.get("host_api").is_some_and(Value::is_object) {
            return Err(ManifestDecodeError::HostApiNotObject);
        }
        if let Some(contributions) = manifest.get("contributions").and_then(Value::as_array) {
            for (index, contribution) in contributions.iter().enumerate() {
                let contribution = contribution
                    .as_object()
                    .ok_or(ManifestDecodeError::ContributionNotObject { index })?;
                for field in ["type", "auth", "phase"] {
                    let must_be_string = field == "type" || contribution.contains_key(field);
                    if must_be_string && !contribution.get(field).is_some_and(Value::is_string) {
                        return Err(ManifestDecodeError::ContributionFieldNotString {
                            index,
                            field,
                        });
                    }
                }
            }
        }

        ManifestFields::deserialize(value).map_err(ManifestDecodeError::Json)
    }

    pub fn validate(&self, host: HostApi) -> Result<(), Vec<ManifestError>> {
        let mut errors = Vec::new();
        let valid_id = self
            .id
            .as_bytes()
            .split_first()
            .is_some_and(|(first, rest)| {
                first.is_ascii_lowercase()
                    && rest.len() <= 31
                    && rest
                        .iter()
                        .all(|&byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
            });
        if !valid_id {
            errors.push(ManifestError::InvalidModuleId {
                id: self.id.clone(),
            });
        }
        if self.module_version.is_empty() {
            errors.push(ManifestError::EmptyModuleVersion);
        }
        if !self.host_api.accepts(host) {
            errors.push(ManifestError::IncompatibleHostApi {
                required: self.host_api,
                host,
            });
        }

        let mut ids = BTreeSet::new();
        let mut routes = BTreeSet::new();
        let mut hooks = BTreeSet::new();
        let mut has_catalog = false;
        for (index, contribution) in self.contributions.iter().enumerate() {
            if let Contribution::HttpRoute { id, .. } | Contribution::Panel { id, .. } =
                contribution
                && !ids.insert(id)
            {
                errors.push(ManifestError::DuplicateContributionId {
                    index,
                    id: id.clone(),
                });
            }
            match contribution {
                Contribution::HttpRoute {
                    id, method, path, ..
                } => {
                    if !(1..=64).contains(&id.len())
                        || !id.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                        })
                    {
                        errors.push(ManifestError::InvalidRouteId {
                            index,
                            id: id.clone(),
                        });
                    }
                    if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
                        errors.push(ManifestError::InvalidRouteMethod {
                            index,
                            id: id.clone(),
                            method: method.clone(),
                        });
                    }
                    if !path.starts_with('/')
                        || path.contains("..")
                        || path.contains("//")
                        || path.contains(['?', '#'])
                    {
                        errors.push(ManifestError::InvalidRoutePath {
                            index,
                            id: id.clone(),
                            path: path.clone(),
                        });
                    }
                    if !routes.insert((method, path)) {
                        errors.push(ManifestError::DuplicateRoute {
                            index,
                            id: id.clone(),
                            method: method.clone(),
                            path: path.clone(),
                        });
                    }
                }
                Contribution::Hook { phase } => {
                    if !hooks.insert(*phase) {
                        errors.push(ManifestError::DuplicateHook {
                            index,
                            phase: *phase,
                        });
                    }
                }
                Contribution::Catalog => {
                    if has_catalog {
                        errors.push(ManifestError::DuplicateCatalog { index });
                    }
                    has_catalog = true;
                }
                Contribution::Panel {
                    id,
                    entry,
                    revision,
                } => {
                    if !entry.ends_with(".js")
                        || !entry.split('/').all(|segment| {
                            !segment.trim_matches('.').is_empty()
                                && segment.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric()
                                        || matches!(byte, b'.' | b'_' | b'-')
                                })
                        })
                    {
                        errors.push(ManifestError::InvalidPanelEntry {
                            index,
                            id: id.clone(),
                            entry: entry.clone(),
                        });
                    }
                    if revision.is_empty() {
                        errors.push(ManifestError::EmptyPanelRevision {
                            index,
                            id: id.clone(),
                        });
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
