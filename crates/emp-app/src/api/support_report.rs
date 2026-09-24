//! Allowlisted support report projections. Raw configuration and credentials
//! never cross this boundary.

use crate::app::ServerState;
use crate::http::response::{json_error_response, response, status_text};
use emp_transport::{
    HttpClientPolicy, HttpMethod, ProxyEnvironment, ProxyOrigin, ProxyPolicy, TimeoutPolicy,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const COMPATIBILITY: &[&str] = &[
    "recommended",
    "supported",
    "unverified",
    "unsupported",
    "unavailable",
    "unknown",
];
const RUNTIME_SOURCES: &[&str] = &[
    "configured",
    "codex_app",
    "managed",
    "vscode",
    "vscode_insiders",
    "cursor",
    "path_cli",
];
const CREDENTIAL_STATES: &[&str] = &["valid", "invalid", "unknown"];

fn choice<'a>(value: Option<&'a str>, allowed: &[&str]) -> &'a str {
    value
        .filter(|value| allowed.contains(value))
        .unwrap_or("unknown")
}

fn valid_version(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    for maximum in [4, 4, 6] {
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == start || index - start > maximum {
            return false;
        }
        if maximum != 6 {
            if bytes.get(index) != Some(&b'.') {
                return false;
            }
            index += 1;
        }
    }
    for separator in [b'-', b'+'] {
        if bytes.get(index) == Some(&separator) {
            index += 1;
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'.' | b'-'))
            {
                index += 1;
            }
            if index == start || index - start > 32 {
                return false;
            }
        }
    }
    index == bytes.len()
}

fn quota_status(
    error: Option<&str>,
    credential_status: &str,
    credential_present: bool,
    quota_is_object: bool,
) -> &'static str {
    match error {
        Some("quota_auth_required") => return "auth_required",
        Some("quota_transport_error") => return "transport_error",
        Some("quota_rate_limited") => return "rate_limited",
        Some(value) if !value.is_empty() => return "unclassified_error",
        _ => {}
    }
    if credential_status == "invalid" {
        "auth_required"
    } else if !credential_present {
        "credential_missing"
    } else if quota_is_object {
        "success"
    } else {
        "not_checked"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigPlatform {
    Windows,
    MacOs,
    Other,
}

fn expanded_absolute(
    path: &std::path::Path,
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> std::path::PathBuf {
    use std::path::{Component, Path, PathBuf};

    let expanded = match path.to_str() {
        Some("~") => home.to_path_buf(),
        Some(value) if value.starts_with("~/") || value.starts_with("~\\") => {
            home.join(&value[2..])
        }
        _ => path.to_path_buf(),
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    normalized
}

fn desktop_config_path(
    platform: ConfigPlatform,
    home: &std::path::Path,
    local_app_data: Option<&str>,
    app_data: Option<&str>,
    xdg_config_home: Option<&str>,
) -> std::path::PathBuf {
    use std::path::{Path, PathBuf};

    match platform {
        ConfigPlatform::Windows => local_app_data
            .filter(|value| !value.trim().is_empty())
            .or_else(|| app_data.filter(|value| !value.trim().is_empty()))
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"))
            .join("EasyMultiProvider/config.json"),
        ConfigPlatform::MacOs => {
            home.join("Library/Application Support/EasyMultiProvider/config.json")
        }
        ConfigPlatform::Other => {
            let root = xdg_config_home
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            root.join(Path::new("easy-multi-provider/config.json"))
        }
    }
}

fn configuration_path_mask(
    path: &std::path::Path,
    home: &std::path::Path,
    cwd: &std::path::Path,
    platform: ConfigPlatform,
    local_app_data: Option<&str>,
    app_data: Option<&str>,
    xdg_config_home: Option<&str>,
) -> (String, &'static str) {
    use std::path::Path;

    let home = expanded_absolute(home, home, cwd);
    let absolute = expanded_absolute(path, &home, cwd);
    let desktop = expanded_absolute(
        &desktop_config_path(platform, &home, local_app_data, app_data, xdg_config_home),
        &home,
        cwd,
    );
    let working = expanded_absolute(Path::new("config.json"), &home, cwd);
    let same_path = |left: &Path, right: &Path| {
        if platform == ConfigPlatform::Windows {
            left.to_string_lossy()
                .eq_ignore_ascii_case(&right.to_string_lossy())
        } else {
            left == right
        }
    };
    if same_path(&absolute, &desktop) {
        let display = match platform {
            ConfigPlatform::Windows => "<user-config>/EasyMultiProvider/config.json".to_owned(),
            ConfigPlatform::MacOs => {
                "~/Library/Application Support/EasyMultiProvider/config.json".to_owned()
            }
            ConfigPlatform::Other => {
                let root = xdg_config_home
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| expanded_absolute(Path::new(value), &home, cwd))
                    .unwrap_or_else(|| home.join(".config"));
                if same_path(&root, &home.join(".config")) {
                    "~/.config/easy-multi-provider/config.json".to_owned()
                } else {
                    "<xdg-config>/easy-multi-provider/config.json".to_owned()
                }
            }
        };
        return (display, "desktop_default");
    }
    if same_path(&absolute, &working) {
        return ("./config.json".to_owned(), "working_directory");
    }
    let under_home = absolute.starts_with(&home);
    let filename = if absolute
        .file_name()
        .is_some_and(|name| name == "config.json")
    {
        "config.json"
    } else {
        "<custom-file>"
    };
    let display = if under_home {
        format!("~/<custom>/{filename}")
    } else {
        format!("<custom>/{filename}")
    };
    (display, "custom")
}

#[derive(Clone, Debug)]
pub(crate) struct NetworkSnapshot {
    source_at_startup: &'static str,
    chatgpt_route: &'static str,
    proxy_scheme: Option<String>,
}

impl NetworkSnapshot {
    pub(crate) fn capture(environment: &ProxyEnvironment) -> Self {
        let policy = HttpClientPolicy::new(
            ProxyPolicy::from_environment(environment.clone()),
            TimeoutPolicy::default(),
        );
        let route = policy
            .plan(
                HttpMethod::Get,
                "https://chatgpt.com",
                BTreeMap::new(),
                false,
            )
            .ok()
            .map(|plan| plan.route.proxy_origin);
        let (chatgpt_route, proxy_scheme) = match route {
            Some(ProxyOrigin::Direct) => ("direct", None),
            Some(ProxyOrigin::Proxy { origin, .. }) => {
                let scheme = origin
                    .split_once("://")
                    .map(|(scheme, _)| scheme.to_owned());
                ("proxy", scheme)
            }
            None => ("unknown", None),
        };
        let source_at_startup = if [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "WS_PROXY",
            "WSS_PROXY",
            "SOCKS_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "ws_proxy",
            "wss_proxy",
            "socks_proxy",
        ]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
        {
            "environment"
        } else {
            "direct"
        };
        Self {
            source_at_startup,
            chatgpt_route,
            proxy_scheme,
        }
    }

    fn report(&self) -> Value {
        let allowed_schemes = ["http", "https", "socks4", "socks4a", "socks5", "socks5h"];
        let scheme = self
            .proxy_scheme
            .as_deref()
            .filter(|scheme| allowed_schemes.contains(scheme));
        json!({
            "source_at_startup": choice(Some(self.source_at_startup), &["environment", "system", "direct"]),
            "chatgpt_route": self.chatgpt_route,
            "proxy_scheme": if self.chatgpt_route == "proxy" { scheme } else { None },
            "connectivity_probe": "not_run",
        })
    }
}

fn configuration_report(path: &Path) -> Value {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    let platform = if cfg!(windows) {
        ConfigPlatform::Windows
    } else if cfg!(target_os = "macos") {
        ConfigPlatform::MacOs
    } else {
        ConfigPlatform::Other
    };
    let local_app_data = std::env::var("LOCALAPPDATA").ok();
    let app_data = std::env::var("APPDATA").ok();
    let xdg_config_home = std::env::var("XDG_CONFIG_HOME").ok();
    let (display, location) = configuration_path_mask(
        path,
        &home,
        &cwd,
        platform,
        local_app_data.as_deref(),
        app_data.as_deref(),
        xdg_config_home.as_deref(),
    );
    let absolute = expanded_absolute(path, &home, &cwd);
    let metadata = std::fs::metadata(&absolute);
    let exists = metadata.as_ref().is_ok_and(|metadata| metadata.is_file());
    let access_path = if exists {
        absolute.as_path()
    } else {
        absolute.parent().unwrap_or_else(|| Path::new("."))
    };
    let write_access = write_access(access_path).unwrap_or("unknown");
    json!({
        "path": display,
        "location": location,
        "exists": exists,
        "write_access_hint": write_access,
    })
}

#[cfg(unix)]
fn write_access(path: &Path) -> Option<&'static str> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid NUL-terminated path and access does not mutate it.
    Some(if unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0 {
        "allowed"
    } else {
        "denied"
    })
}

#[cfg(windows)]
fn write_access(path: &Path) -> Option<&'static str> {
    std::fs::metadata(path).ok().map(|metadata| {
        if metadata.permissions().readonly() {
            "denied"
        } else {
            "allowed"
        }
    })
}

#[cfg(not(any(unix, windows)))]
fn write_access(_path: &Path) -> Option<&'static str> {
    None
}

fn version_value(value: Option<&str>) -> Value {
    value
        .filter(|value| valid_version(value))
        .map_or(Value::Null, |value| Value::String(value.to_owned()))
}

fn runtime_report(state: &ServerState) -> Value {
    let compatibility = crate::services::runtime::compatibility_snapshot(state, false);
    let sync = state.backend.integration.runtime.snapshot();
    let inventory = compatibility.get("runtimes").and_then(Value::as_array);
    let items = inventory
        .into_iter()
        .flatten()
        .take(16)
        .filter_map(|item| {
            let object = item.as_object()?;
            Some(json!({
                "version": version_value(object.get("installed").and_then(Value::as_str)),
                "compatibility": choice(object.get("status").and_then(Value::as_str), COMPATIBILITY),
                "source": choice(object.get("source").and_then(Value::as_str), RUNTIME_SOURCES),
                "selected": object.get("helper").and_then(Value::as_bool) == Some(true),
                "targeted": object.get("targeted").and_then(Value::as_bool) == Some(true),
            }))
        })
        .collect::<Vec<_>>();
    let runtime_states = [
        "not_checked",
        "catalog_unverified",
        "reload_required",
        "stopping",
        "emp_loaded",
        "native_loaded",
        "stopped_waiting_for_start",
        "stop_failed",
        "verification_failed",
        "unsupported",
    ];
    json!({
        "version": version_value(compatibility.get("installed").and_then(Value::as_str)),
        "compatibility": choice(compatibility.get("status").and_then(Value::as_str), COMPATIBILITY),
        "source": choice(compatibility.get("source").and_then(Value::as_str), RUNTIME_SOURCES),
        "state": choice(sync.get("state").and_then(Value::as_str), &runtime_states),
        "target": choice(sync.get("target").and_then(Value::as_str), &["emp", "native"]),
        "verified": sync.get("verified").and_then(Value::as_bool) == Some(true),
        "inventory": items,
        "inventory_truncated": inventory.is_some_and(|items| items.len() > 16),
    })
}

fn quota_status_value(
    error: Option<&str>,
    credential_status: &str,
    credential_present: bool,
    quota: Option<&Value>,
) -> &'static str {
    quota_status(
        error,
        credential_status,
        credential_present,
        quota.is_some_and(Value::is_object),
    )
}

fn accounts_report(state: &ServerState) -> Result<Value, ()> {
    let public = crate::services::accounts::accounts_snapshot(state).ok_or(())?;
    let errors = public.get("refresh_errors").and_then(Value::as_object);
    let native = public.get("native_account").and_then(Value::as_object);
    let native_present = native
        .and_then(|account| account.get("credential_set"))
        .and_then(Value::as_bool)
        == Some(true);
    let native_quota = native.and_then(|account| account.get("quota"));
    let native_status = quota_status_value(
        errors
            .and_then(|errors| errors.get("@native"))
            .and_then(Value::as_str),
        "unknown",
        native_present,
        native_quota,
    );
    let accounts = public.get("accounts").and_then(Value::as_array);
    let imported_count = accounts.map_or(0, Vec::len);
    let imported = accounts
        .into_iter()
        .flatten()
        .take(32)
        .enumerate()
        .filter_map(|(index, account)| {
            let account = account.as_object()?;
            let credential_status = choice(
                account.get("credential_status").and_then(Value::as_str),
                CREDENTIAL_STATES,
            );
            let present = account.get("credential_set").and_then(Value::as_bool) == Some(true);
            let error = account.get("id")
                .and_then(Value::as_str)
                .and_then(|id| errors.and_then(|errors| errors.get(id)))
                .and_then(Value::as_str);
            Some(json!({
                "index": index + 1,
                "credential_present": present,
                "credential_status": credential_status,
                "quota_status": quota_status_value(error, credential_status, present, account.get("quota")),
            }))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "native": {"credential_present": native_present, "quota_status": native_status},
        "imported_count": imported_count,
        "imported": imported,
        "imported_truncated": imported_count > 32,
        "scope": "saved_status_and_this_process_errors",
    }))
}

fn build_support_report(state: &ServerState) -> Result<Value, ()> {
    let generated_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| ())?;
    Ok(json!({
        "schema_version": 1,
        "generated_at": generated_at,
        "emp_version": crate::VERSION,
        "configuration": configuration_report(&state.backend.configuration.config_path),
        "codex": runtime_report(state),
        "network": state.backend.transport.support_network.report(),
        "accounts": accounts_report(state)?,
    }))
}

pub(crate) fn read(state: &ServerState) -> Vec<u8> {
    let report = match build_support_report(state) {
        Ok(report) => report,
        Err(()) => {
            return json_error_response(
                503,
                status_text(503),
                "Support report is unavailable",
                None,
                &[],
            );
        }
    };
    let body = serde_json::to_vec(&report).expect("support report is JSON serializable");
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &body,
        &[(
            "Content-Disposition",
            "attachment; filename=\"EMP-support-report.json\"",
        )],
    )
}

#[cfg(test)]
mod tests {
    use super::{
        COMPATIBILITY, CREDENTIAL_STATES, ConfigPlatform, RUNTIME_SOURCES, choice,
        configuration_path_mask, desktop_config_path, quota_status, valid_version,
    };

    #[test]
    fn support_report_allowlists_reject_unknown_values() {
        assert_eq!(choice(Some("recommended"), COMPATIBILITY), "recommended");
        assert_eq!(choice(Some("secret-path"), COMPATIBILITY), "unknown");
        assert_eq!(choice(Some("invalid"), CREDENTIAL_STATES), "invalid");
        assert_eq!(choice(None, CREDENTIAL_STATES), "unknown");
        assert_eq!(choice(Some("path_cli"), RUNTIME_SOURCES), "path_cli");
    }

    #[test]
    fn support_report_version_validation_matches_python_shape() {
        for valid in ["0.11.10", "1234.9.123456-beta.1+local"] {
            assert!(valid_version(valid), "{valid}");
        }
        for invalid in ["", "0.11", "12345.1.1", "1.2.1234567", "1.2.3+", "1.2.3 x"] {
            assert!(!valid_version(invalid), "{invalid}");
        }
    }

    #[test]
    fn support_report_quota_status_matches_python_precedence() {
        assert_eq!(
            quota_status(Some("quota_auth_required"), "valid", true, true),
            "auth_required"
        );
        assert_eq!(
            quota_status(Some("quota_transport_error"), "valid", true, true),
            "transport_error"
        );
        assert_eq!(
            quota_status(Some("quota_rate_limited"), "valid", true, true),
            "rate_limited"
        );
        assert_eq!(
            quota_status(Some("other"), "valid", true, true),
            "unclassified_error"
        );
        assert_eq!(quota_status(None, "invalid", true, true), "auth_required");
        assert_eq!(
            quota_status(None, "unknown", false, true),
            "credential_missing"
        );
        assert_eq!(quota_status(None, "valid", true, true), "success");
        assert_eq!(quota_status(None, "valid", true, false), "not_checked");
    }

    #[test]
    fn support_report_configuration_masks_desktop_working_and_custom_paths() {
        use std::path::Path;
        let home = Path::new("/home/alice");
        let cwd = Path::new("/work/project");
        let cases = [
            (
                Path::new("/home/alice/.config/easy-multi-provider/config.json"),
                ConfigPlatform::Other,
                None,
                "~/.config/easy-multi-provider/config.json",
                "desktop_default",
            ),
            (
                Path::new("/private/xdg/easy-multi-provider/config.json"),
                ConfigPlatform::Other,
                Some("/private/xdg"),
                "<xdg-config>/easy-multi-provider/config.json",
                "desktop_default",
            ),
            (
                Path::new("/home/alice/Library/Application Support/EasyMultiProvider/config.json"),
                ConfigPlatform::MacOs,
                None,
                "~/Library/Application Support/EasyMultiProvider/config.json",
                "desktop_default",
            ),
            (
                Path::new("/home/alice/AppData/Local/EasyMultiProvider/config.json"),
                ConfigPlatform::Windows,
                Some("/home/alice/AppData/Local"),
                "<user-config>/EasyMultiProvider/config.json",
                "desktop_default",
            ),
        ];
        for (path, platform, xdg, expected_path, expected_location) in cases {
            let (display, location) = configuration_path_mask(
                path,
                home,
                cwd,
                platform,
                if platform == ConfigPlatform::Windows {
                    xdg
                } else {
                    None
                },
                None,
                if platform == ConfigPlatform::Other {
                    xdg
                } else {
                    None
                },
            );
            assert_eq!(display, expected_path);
            assert_eq!(location, expected_location);
            assert!(!display.contains("alice") && !display.contains("private"));
        }
        assert_eq!(
            configuration_path_mask(
                Path::new("./config.json"),
                home,
                cwd,
                ConfigPlatform::Other,
                None,
                None,
                None,
            ),
            ("./config.json".to_owned(), "working_directory"),
        );
        assert_eq!(
            configuration_path_mask(
                Path::new("/tmp/private-config/config.json"),
                home,
                cwd,
                ConfigPlatform::Other,
                None,
                None,
                None,
            ),
            ("<custom>/config.json".to_owned(), "custom"),
        );
    }

    #[test]
    fn desktop_config_path_uses_python_environment_precedence() {
        use std::path::{Path, PathBuf};
        let home = Path::new("/home/alice");
        assert_eq!(
            desktop_config_path(
                ConfigPlatform::Windows,
                home,
                Some("/local"),
                Some("/roaming"),
                None
            ),
            PathBuf::from("/local/EasyMultiProvider/config.json"),
        );
        assert_eq!(
            desktop_config_path(ConfigPlatform::Windows, home, None, Some("/roaming"), None),
            PathBuf::from("/roaming/EasyMultiProvider/config.json"),
        );
        assert_eq!(
            desktop_config_path(ConfigPlatform::Other, home, None, None, None),
            PathBuf::from("/home/alice/.config/easy-multi-provider/config.json"),
        );
    }
}
