//! User paths, managed private-path validation and platform config location.

use super::{CONFIG_PATH_ENV, ConfigError, ConfigResult, Value, default_native_catalog_path};
use std::env;
use std::path::{Path, PathBuf, absolute};

/// Stable generated catalog location, matching Python's expanded, resolved Codex home.
pub fn generated_catalog_path(codex_home: Option<&Path>) -> PathBuf {
    let home = codex_home.map(Path::to_path_buf).unwrap_or_else(|| {
        env::var("CODEX_HOME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| PathBuf::from(value.trim()))
            .unwrap_or_else(|| expand_user(Path::new("~/.codex")))
    });
    let home = if home.as_os_str().is_empty() {
        Path::new(".")
    } else {
        &home
    };
    path_python_resolve(&expand_user(home))
        .join("easy-multi-provider")
        .join("catalog.json")
}

/// Resolve a user-selected path as Python's expanduser().resolve() does,
/// including nonexistent final components.
pub fn resolve_user_path(path: &Path) -> PathBuf {
    path_python_resolve(&expand_user(path))
}

pub(crate) fn path_python_resolve(path: &Path) -> PathBuf {
    fn recurse(path: &Path, followed_links: usize) -> PathBuf {
        let absolute = absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let mut resolved = PathBuf::new();
        let components = absolute.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            use std::path::Component;
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
                Component::Normal(name) => {
                    let candidate = resolved.join(name);
                    let is_link = std::fs::symlink_metadata(&candidate)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink());
                    if is_link
                        && followed_links < 64
                        && let Ok(target) = std::fs::read_link(&candidate)
                    {
                        let mut redirected = if target.is_absolute() {
                            target
                        } else {
                            resolved.join(target)
                        };
                        for remaining in &components[index + 1..] {
                            redirected.push(remaining.as_os_str());
                        }
                        return recurse(&redirected, followed_links + 1);
                    }
                    resolved.push(name);
                }
            }
        }
        resolved
    }
    recurse(path, 0)
}

pub(crate) fn account_root(config: &Value) -> PathBuf {
    expand_user(Path::new(
        config
            .get("account_store_path")
            .and_then(Value::as_str)
            .unwrap_or("state/accounts"),
    ))
}

/// Expand `~` exactly for paths escaped from the configuration crate.
pub(crate) fn expand_home(path: &Path) -> PathBuf {
    expand_user(path)
}

/// Return the Python-native catalog fallback, expanded in the caller's home.
pub(crate) fn expand_home_default_native_catalog_path() -> PathBuf {
    expand_user(Path::new(&default_native_catalog_path()))
}

fn expand_user(path: &Path) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    else {
        return path.to_path_buf();
    };
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| path.to_path_buf(), |home| PathBuf::from(home).join(rest))
}

pub(super) fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let character = byte as char;
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '~') {
            encoded.push(character);
        } else {
            encoded.push('%');
            encoded.push_str(format!("{byte:02X}").as_str());
        }
    }
    encoded
}

fn canonical_account_paths(config: &mut Value, config_path: &Path) -> Result<(), ConfigError> {
    let raw_accounts = config
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or_else(|| ConfigError::new("accounts must be a list"))?;
    let entries = raw_accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            (
                index,
                account
                    .get("auth_file")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                account.get("id").and_then(Value::as_str).map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    for (index, raw_path, id) in entries {
        if raw_path.is_empty() {
            continue;
        }
        let Some(id) = id else {
            return Err(ConfigError::new(
                "account.id must be a safe single path segment",
            ));
        };
        let expected = crate::accounts::account_auth_path(config, &id, config_path)
            .map_err(|error| ConfigError::new(error.to_string()))?;
        let mut actual = expand_user(Path::new(&raw_path));
        if !actual.is_absolute() {
            actual = config_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(actual);
        }
        let actual_is_symlink = std::fs::symlink_metadata(&actual)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        let actual = path_python_resolve(&actual);
        if actual != expected || actual_is_symlink {
            return Err(ConfigError::new(
                "account.auth_file must be managed inside the account store",
            ));
        }
        let account = config
            .get_mut("accounts")
            .and_then(Value::as_array_mut)
            .and_then(|accounts| accounts.get_mut(index))
            .ok_or_else(|| ConfigError::new("accounts must be a list"))?;
        if let Some(object) = account.as_object_mut() {
            object.insert(
                "auth_file".to_owned(),
                Value::from(expected.to_string_lossy().into_owned()),
            );
        }
    }
    Ok(())
}

/// Canonicalize configured account credentials against the derived account root.
///
/// This is the public Rust compatibility slice for Python
/// `canonicalize_account_paths`. Account path errors are represented as
/// `ConfigError` because Python's `_canonicalize_private_paths` catches the
/// original `AccountError` and re-raises it as a `ConfigError`.
pub fn canonicalize_account_paths(config: &mut Value, config_path: &Path) -> ConfigResult<()> {
    canonical_account_paths(config, config_path)
}

pub(super) fn canonical_secret_root(config: &Value, config_path: &Path) -> PathBuf {
    let mut root = expand_user(Path::new(
        config
            .get("secret_store_path")
            .and_then(Value::as_str)
            .unwrap_or("state/secrets"),
    ));
    if !root.is_absolute() {
        root = config_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(root);
    }
    path_python_resolve(&root)
}

fn canonical_secret_paths(config: &mut Value, config_path: &Path) -> Result<(), ConfigError> {
    let base = config_path
        .parent()
        .ok_or_else(|| ConfigError::new("configuration path must have a parent"))?;
    let mut root = expand_user(Path::new(
        config
            .get("secret_store_path")
            .and_then(Value::as_str)
            .unwrap_or("state/secrets"),
    ));
    if !root.is_absolute() {
        root = base.join(root);
    }
    let root = path_python_resolve(&root);
    let providers = config
        .get_mut("providers")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ConfigError::new("providers must be a list"))?;
    for provider in providers {
        let raw_path = provider
            .get("api_key_file")
            .and_then(Value::as_str)
            .unwrap_or("");
        if raw_path.is_empty() {
            continue;
        }
        let Some(id) = provider.get("id").and_then(Value::as_str) else {
            return Err(ConfigError::new(
                "provider.id must be a safe single path segment",
            ));
        };
        let expected = root.join(percent_encode(id) + ".key.enc");
        let actual = expand_user(Path::new(raw_path));
        let actual = if actual.is_absolute() {
            actual
        } else {
            config_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(actual)
        };
        let actual_is_symlink = std::fs::symlink_metadata(&actual)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        let actual = path_python_resolve(&actual);
        if actual != expected || actual_is_symlink {
            return Err(ConfigError::new(
                "provider.api_key_file must be managed inside the secret store",
            ));
        }
        if let Some(object) = provider.as_object_mut() {
            object.insert(
                "api_key_file".to_owned(),
                Value::String(expected.to_string_lossy().into_owned()),
            );
        }
    }
    Ok(())
}

/// Canonicalize managed private paths for already normalized configuration.
///
/// This is the public Rust compatibility slice for Python
/// `_canonicalize_private_paths`. Relative store paths and relative managed
/// files are based on `config_path.parent`, `~` is expanded, missing path
/// components are retained, existing links are followed, and the final managed
/// input path must itself not be a symlink.
pub fn canonicalize_private_paths(config: &mut Value, config_path: &Path) -> ConfigResult<()> {
    canonical_account_paths(config, config_path)?;
    canonical_secret_paths(config, config_path)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigPlatform {
    Windows,
    MacOs,
    Unix,
}

struct ConfigPathEnvironment<'a> {
    configured: Option<&'a str>,
    home: Option<&'a str>,
    user_profile: Option<&'a str>,
    xdg_config_home: Option<&'a str>,
    local_app_data: Option<&'a str>,
    app_data: Option<&'a str>,
}

fn nonblank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn config_path_with_home(value: &str, home: Option<&str>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(value);
    };
    if value == "~" {
        return PathBuf::from(home);
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

fn config_path_from_environment(
    environment: ConfigPathEnvironment<'_>,
    platform: ConfigPlatform,
) -> PathBuf {
    let home = match platform {
        ConfigPlatform::Windows => {
            nonblank(environment.user_profile).or_else(|| nonblank(environment.home))
        }
        ConfigPlatform::MacOs | ConfigPlatform::Unix => {
            nonblank(environment.home).or_else(|| nonblank(environment.user_profile))
        }
    };
    if let Some(configured) = nonblank(environment.configured) {
        return config_path_with_home(configured, home);
    }

    match platform {
        ConfigPlatform::Windows => nonblank(environment.local_app_data)
            .or_else(|| nonblank(environment.app_data))
            .map(|root| config_path_with_home(root, home))
            .unwrap_or_else(|| {
                home.map_or_else(PathBuf::new, PathBuf::from)
                    .join("AppData/Local")
            })
            .join("EasyMultiProvider/config.json"),
        ConfigPlatform::MacOs => home
            .map_or_else(PathBuf::new, PathBuf::from)
            .join("Library/Application Support/EasyMultiProvider/config.json"),
        ConfigPlatform::Unix => nonblank(environment.xdg_config_home)
            .map(|root| config_path_with_home(root, home))
            .unwrap_or_else(|| {
                home.map_or_else(PathBuf::new, PathBuf::from)
                    .join(".config")
            })
            .join("easy-multi-provider/config.json"),
    }
}

/// Return the configuration path shared by desktop and CLI launches.
///
/// An explicit CLI path takes precedence at the call site, followed here by
/// `EASY_MULTI_PROVIDER_CONFIG` and the platform's per-user default.
pub fn config_path() -> PathBuf {
    let configured = env::var(CONFIG_PATH_ENV).ok();
    let home = env::var("HOME").ok();
    let user_profile = env::var("USERPROFILE").ok();
    let xdg_config_home = env::var("XDG_CONFIG_HOME").ok();
    let local_app_data = env::var("LOCALAPPDATA").ok();
    let app_data = env::var("APPDATA").ok();
    let environment = ConfigPathEnvironment {
        configured: configured.as_deref(),
        home: home.as_deref(),
        user_profile: user_profile.as_deref(),
        xdg_config_home: xdg_config_home.as_deref(),
        local_app_data: local_app_data.as_deref(),
        app_data: app_data.as_deref(),
    };
    let platform = if cfg!(windows) {
        ConfigPlatform::Windows
    } else if cfg!(target_os = "macos") {
        ConfigPlatform::MacOs
    } else {
        ConfigPlatform::Unix
    };
    config_path_from_environment(environment, platform)
}

#[cfg(test)]
mod tests;
