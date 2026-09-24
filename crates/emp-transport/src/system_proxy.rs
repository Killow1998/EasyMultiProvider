use crate::http_policy::ProxyEnvironment;
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const SYSTEM_PROXY_READ_TIMEOUT: Duration = Duration::from_millis(750);
const SYSTEM_PROXY_CACHE_TTL: Duration = Duration::from_secs(1);
#[cfg(any(windows, test))]
const WINDOWS_INTERNET_SETTINGS_KEY: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

pub(crate) fn capture() -> Option<ProxyEnvironment> {
    static CACHE: OnceLock<SystemProxyCache> = OnceLock::new();
    CACHE.get_or_init(SystemProxyCache::default).resolve_with(
        Instant::now,
        false,
        read_system_proxy,
    )
}

#[derive(Default)]
struct SystemProxyCache {
    state: Mutex<SystemProxyCacheState>,
}

#[derive(Default)]
struct SystemProxyCacheState {
    value: Option<ProxyEnvironment>,
    last_attempt: Option<Instant>,
}

impl SystemProxyCache {
    fn resolve_with<C, R>(&self, clock: C, force: bool, reader: R) -> Option<ProxyEnvironment>
    where
        C: Fn() -> Instant,
        R: FnOnce() -> Result<Option<ProxyEnvironment>, ()>,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = clock();
        let is_fresh = state
            .last_attempt
            .and_then(|last| now.checked_duration_since(last))
            .is_some_and(|elapsed| elapsed < SYSTEM_PROXY_CACHE_TTL);
        if !force && is_fresh {
            return state.value.clone();
        }
        if let Ok(value) = reader() {
            state.value = value;
        }
        state.last_attempt = Some(clock());
        state.value.clone()
    }
}

fn read_system_proxy() -> Result<Option<ProxyEnvironment>, ()> {
    #[cfg(target_os = "linux")]
    {
        return capture_linux_gnome();
    }
    #[cfg(target_os = "macos")]
    {
        return capture_macos();
    }
    #[cfg(windows)]
    {
        return capture_windows();
    }
    #[allow(unreachable_code)]
    Ok(None)
}

fn run_command(program: &str, arguments: &[&str], timeout: Duration) -> Option<String> {
    if timeout.is_zero() {
        return None;
    }
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now().checked_add(timeout)?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut output = String::new();
                child.stdout.take()?.read_to_string(&mut output).ok()?;
                return Some(output);
            }
            Ok(Some(_)) => return None,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn capture_linux_gnome() -> Result<Option<ProxyEnvironment>, ()> {
    const ROOT: &str = "org.gnome.system.proxy";
    let deadline = Instant::now()
        .checked_add(SYSTEM_PROXY_READ_TIMEOUT)
        .ok_or(())?;
    let mode = gsettings_value(ROOT, "mode", deadline).ok_or(())?;
    if gsettings_atom(&mode).as_deref() != Some("manual") {
        return Ok(None);
    }
    let mut values = BTreeMap::new();
    for (name, schema, key) in [
        ("http_host", "org.gnome.system.proxy.http", "host"),
        ("http_port", "org.gnome.system.proxy.http", "port"),
        ("https_host", "org.gnome.system.proxy.https", "host"),
        ("https_port", "org.gnome.system.proxy.https", "port"),
        ("socks_host", "org.gnome.system.proxy.socks", "host"),
        ("socks_port", "org.gnome.system.proxy.socks", "port"),
        ("same_proxy", ROOT, "use-same-proxy"),
        ("ignore_hosts", ROOT, "ignore-hosts"),
    ] {
        if let Some(value) = gsettings_value(schema, key, deadline) {
            values.insert(name.to_owned(), value);
        }
    }
    Ok(parse_gnome_proxy_settings(&mode, &values))
}

#[cfg(target_os = "linux")]
fn gsettings_value(schema: &str, key: &str, deadline: Instant) -> Option<String> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    run_command("gsettings", &["get", schema, key], remaining)
}

#[cfg(target_os = "macos")]
fn capture_macos() -> Result<Option<ProxyEnvironment>, ()> {
    let output = run_command("scutil", &["--proxy"], SYSTEM_PROXY_READ_TIMEOUT).ok_or(())?;
    Ok(parse_macos_proxy_settings(&output))
}

#[cfg(windows)]
fn capture_windows() -> Result<Option<ProxyEnvironment>, ()> {
    let output = run_command(
        "reg",
        &["query", WINDOWS_INTERNET_SETTINGS_KEY],
        SYSTEM_PROXY_READ_TIMEOUT,
    )
    .ok_or(())?;
    Ok(parse_windows_proxy_settings(&output))
}

fn parse_gnome_proxy_settings(
    mode: &str,
    values: &BTreeMap<String, String>,
) -> Option<ProxyEnvironment> {
    if gsettings_atom(mode)?.as_str() != "manual" {
        return None;
    }
    let http = proxy_url(
        values
            .get("http_host")
            .and_then(|value| gsettings_atom(value))
            .as_deref(),
        values
            .get("http_port")
            .and_then(|value| gsettings_integer(value)),
        "http",
    );
    let mut https = proxy_url(
        values
            .get("https_host")
            .and_then(|value| gsettings_atom(value))
            .as_deref(),
        values
            .get("https_port")
            .and_then(|value| gsettings_integer(value)),
        "http",
    );
    if https.is_none()
        && values
            .get("same_proxy")
            .is_some_and(|value| value.trim() == "true")
    {
        https.clone_from(&http);
    }
    let socks = proxy_url(
        values
            .get("socks_host")
            .and_then(|value| gsettings_atom(value))
            .as_deref(),
        values
            .get("socks_port")
            .and_then(|value| gsettings_integer(value)),
        "socks5h",
    );
    let mut environment = ProxyEnvironment {
        http,
        https,
        socks,
        ..ProxyEnvironment::default()
    };
    if let Some(ignored) = values.get("ignore_hosts") {
        for host in gsettings_list(ignored) {
            if !environment.no_proxy.contains(&host) {
                environment.no_proxy.push(host);
            }
        }
    }
    environment.has_proxy().then_some(environment)
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_proxy_settings(output: &str) -> Option<ProxyEnvironment> {
    let mut values = BTreeMap::new();
    let mut exceptions = Vec::new();
    let mut reading_exceptions = false;
    for line in output.lines() {
        let line = line.trim();
        if reading_exceptions {
            if line.starts_with('}') {
                reading_exceptions = false;
            } else if let Some((_, value)) = line.split_once(':') {
                let value = gsettings_atom(value).unwrap_or_else(|| value.trim().to_owned());
                if !value.is_empty() {
                    exceptions.push(value);
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key == "ExceptionsList" {
            reading_exceptions = value.contains("<array>");
            if let Some(value) = gsettings_atom(value) {
                exceptions.push(value);
            }
        } else {
            values.insert(key.to_owned(), value.trim().to_owned());
        }
    }
    let mut environment = ProxyEnvironment {
        http: mac_enabled_proxy(&values, "HTTPEnable", "HTTPProxy", "HTTPPort", "http"),
        https: mac_enabled_proxy(&values, "HTTPSEnable", "HTTPSProxy", "HTTPSPort", "http"),
        socks: mac_enabled_proxy(&values, "SOCKSEnable", "SOCKSProxy", "SOCKSPort", "socks5h"),
        ..ProxyEnvironment::default()
    };
    for host in exceptions {
        if !environment.no_proxy.contains(&host) {
            environment.no_proxy.push(host);
        }
    }
    environment.has_proxy().then_some(environment)
}

#[cfg(any(target_os = "macos", test))]
fn mac_enabled_proxy(
    values: &BTreeMap<String, String>,
    enabled_key: &str,
    host_key: &str,
    port_key: &str,
    scheme: &str,
) -> Option<String> {
    if values
        .get(enabled_key)
        .and_then(|value| value.parse::<u8>().ok())
        != Some(1)
    {
        return None;
    }
    proxy_url(
        values
            .get(host_key)
            .map(|value| gsettings_atom(value).unwrap_or_else(|| value.trim().to_owned()))
            .as_deref(),
        values
            .get(port_key)
            .and_then(|value| gsettings_integer(value)),
        scheme,
    )
}

#[cfg(any(windows, test))]
fn parse_windows_proxy_settings(output: &str) -> Option<ProxyEnvironment> {
    let values = output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let key = fields.next()?;
            let value_type = fields.next()?;
            let value = fields.collect::<Vec<_>>().join(" ");
            Some((key.to_ascii_lowercase(), (value_type.to_owned(), value)))
        })
        .collect::<BTreeMap<_, _>>();
    let enabled = values
        .get("proxyenable")
        .and_then(|(_, value)| parse_integer(value))
        == Some(1);
    if !enabled {
        return None;
    }
    let proxy_server = values.get("proxyserver")?.1.as_str();
    let mut lanes = BTreeMap::<String, String>::new();
    if !proxy_server.contains('=') && !proxy_server.contains(';') {
        if let Some(proxy) = normalize_windows_proxy(proxy_server, "http") {
            lanes.insert("http".to_owned(), proxy.clone());
            lanes.insert("https".to_owned(), proxy);
        }
    } else {
        for assignment in proxy_server.split(';') {
            let Some((protocol, address)) = assignment.split_once('=') else {
                continue;
            };
            let protocol = protocol.trim().to_ascii_lowercase();
            let default_scheme = if protocol == "socks" {
                "socks5h"
            } else {
                "http"
            };
            if let Some(proxy) = normalize_windows_proxy(address, default_scheme) {
                let lane = if protocol == "socks" {
                    "socks"
                } else {
                    protocol.as_str()
                };
                lanes.insert(lane.to_owned(), proxy);
            }
        }
    }
    if let Some(socks) = lanes.get("socks").cloned() {
        lanes
            .entry("http".to_owned())
            .or_insert_with(|| socks.clone());
        lanes.entry("https".to_owned()).or_insert(socks);
    }
    let mut environment = ProxyEnvironment {
        http: lanes.remove("http"),
        https: lanes.remove("https"),
        socks: lanes.remove("socks"),
        ..ProxyEnvironment::default()
    };
    if let Some((_, bypass)) = values.get("proxyoverride") {
        for host in bypass
            .split(';')
            .map(str::trim)
            .filter(|host| !host.is_empty())
        {
            if !environment.no_proxy.contains(&host.to_owned()) {
                environment.no_proxy.push(host.to_owned());
            }
        }
    }
    environment.has_proxy().then_some(environment)
}

#[cfg(any(windows, test))]
fn normalize_windows_proxy(address: &str, default_scheme: &str) -> Option<String> {
    let address = address.trim();
    if address.is_empty() {
        return None;
    }
    let proxy = if address.contains("://") {
        address.to_owned()
    } else {
        format!("{default_scheme}://{address}")
    };
    valid_proxy_url(&proxy).then_some(proxy)
}

fn proxy_url(host: Option<&str>, port: Option<u16>, scheme: &str) -> Option<String> {
    let host = host?.trim();
    let port = port?;
    if host.is_empty()
        || host
            .chars()
            .any(|character| character.is_whitespace() || "/@?#".contains(character))
    {
        return None;
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let proxy = format!("{scheme}://{host}:{port}");
    valid_proxy_url(&proxy).then_some(proxy)
}

fn valid_proxy_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
        && url.host_str().is_some()
        && url.port_or_known_default().is_some()
}

fn gsettings_atom(value: &str) -> Option<String> {
    let value = value.trim();
    if value == "true" || value == "false" {
        return Some(value.to_owned());
    }
    let quote = value.chars().next()?;
    if !matches!(quote, '\'' | '"') || value.chars().last()? != quote {
        return None;
    }
    let inner = &value[quote.len_utf8()..value.len() - quote.len_utf8()];
    let mut result = String::with_capacity(inner.len());
    let mut escaped = false;
    for character in inner.chars() {
        if escaped {
            result.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            result.push(character);
        }
    }
    if escaped {
        result.push('\\');
    }
    Some(result)
}

fn gsettings_integer(value: &str) -> Option<u16> {
    let integer = parse_integer(value)?;
    u16::try_from(integer).ok().filter(|value| *value > 0)
}

fn parse_integer(value: &str) -> Option<u64> {
    let value = value.trim();
    value.strip_prefix("0x").map_or_else(
        || value.parse().ok(),
        |hex| u64::from_str_radix(hex, 16).ok(),
    )
}

fn gsettings_list(value: &str) -> Vec<String> {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut item = String::new();
    for character in inner.chars() {
        if escaped {
            item.push(character);
            escaped = false;
        } else if character == '\\' && quote.is_some() {
            escaped = true;
        } else if quote == Some(character) {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == ',' && quote.is_none() {
            push_nonempty(&mut result, &mut item);
        } else {
            item.push(character);
        }
    }
    push_nonempty(&mut result, &mut item);
    result
}

fn push_nonempty(result: &mut Vec<String>, item: &mut String) {
    let value = std::mem::take(item);
    let value = value.trim();
    if !value.is_empty() {
        result.push(value.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_policy::ProxySnapshot;

    #[test]
    fn gnome_manual_proxy_matches_same_proxy_socks_and_bypass_settings() {
        let values = BTreeMap::from([
            ("http_host".to_owned(), "'proxy.local'".to_owned()),
            ("http_port".to_owned(), "8080".to_owned()),
            ("same_proxy".to_owned(), "true".to_owned()),
            ("socks_host".to_owned(), "'socks.local'".to_owned()),
            ("socks_port".to_owned(), "1080".to_owned()),
            (
                "ignore_hosts".to_owned(),
                "['localhost', '*.internal.test']".to_owned(),
            ),
        ]);
        let environment = parse_gnome_proxy_settings("'manual'", &values).unwrap();
        assert_eq!(environment.http.as_deref(), Some("http://proxy.local:8080"));
        assert_eq!(environment.https, environment.http);
        assert_eq!(
            environment.socks.as_deref(),
            Some("socks5h://socks.local:1080")
        );
        assert!(environment.no_proxy.contains(&"localhost".to_owned()));
        assert!(environment.no_proxy.contains(&"*.internal.test".to_owned()));
        assert!(parse_gnome_proxy_settings("'auto'", &values).is_none());
    }

    #[test]
    fn macos_proxy_fields_respect_enable_flags_and_exception_list() {
        let output = "<dictionary> {\nHTTPEnable : 1\nHTTPProxy : proxy.local\nHTTPPort : 8080\nHTTPSEnable : 0\nHTTPSProxy : ignored.local\nHTTPSPort : 8443\nSOCKSEnable : 1\nSOCKSProxy : socks.local\nSOCKSPort : 1080\nExceptionsList : <array> {\n0 : *.internal.test\n1 : localhost\n}\n}";
        let environment = parse_macos_proxy_settings(output).unwrap();
        assert_eq!(environment.http.as_deref(), Some("http://proxy.local:8080"));
        assert!(environment.https.is_none());
        assert_eq!(
            environment.socks.as_deref(),
            Some("socks5h://socks.local:1080")
        );
        assert!(environment.no_proxy.contains(&"*.internal.test".to_owned()));
        assert!(environment.no_proxy.contains(&"localhost".to_owned()));
        assert!(parse_macos_proxy_settings("HTTPEnable : 1\nHTTPProxy : invalid\n").is_none());
    }

    #[test]
    fn windows_single_and_per_scheme_registry_proxy_values_are_supported() {
        assert_eq!(
            WINDOWS_INTERNET_SETTINGS_KEY,
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings"
        );
        let single = "ProxyEnable REG_DWORD 0x1\nProxyServer REG_SZ proxy.local:8080\nProxyOverride REG_SZ *.internal.test;<local>";
        let environment = parse_windows_proxy_settings(single).unwrap();
        assert_eq!(environment.http.as_deref(), Some("http://proxy.local:8080"));
        assert_eq!(environment.https, environment.http);
        assert!(environment.no_proxy.contains(&"<local>".to_owned()));
        assert!(environment.no_proxy.contains(&"*.internal.test".to_owned()));

        let per_scheme = "ProxyEnable REG_DWORD 0x1\nProxyServer REG_SZ http=http.local:80;https=https://secure.local:443;socks=socks.local:1080";
        let environment = parse_windows_proxy_settings(per_scheme).unwrap();
        assert_eq!(environment.http.as_deref(), Some("http://http.local:80"));
        assert_eq!(
            environment.https.as_deref(),
            Some("https://secure.local:443")
        );
        assert_eq!(
            environment.socks.as_deref(),
            Some("socks5h://socks.local:1080")
        );
        assert!(
            parse_windows_proxy_settings(
                "ProxyEnable REG_DWORD 0x0\nProxyServer REG_SZ proxy.local:8080"
            )
            .is_none()
        );
        assert!(
            parse_windows_proxy_settings(
                "ProxyEnable REG_DWORD 0x1\nProxyServer REG_SZ not a proxy"
            )
            .is_none()
        );
    }

    #[test]
    fn environment_source_wins_system_and_custom_bypass_is_preserved() {
        let environment = ProxyEnvironment {
            http: Some("http://env.local:8080".to_owned()),
            no_proxy: vec!["custom.internal".to_owned(), "localhost".to_owned()],
            ..ProxyEnvironment::default()
        };
        let system = ProxyEnvironment {
            https: Some("http://system.local:8080".to_owned()),
            no_proxy: vec!["system.internal".to_owned()],
            ..ProxyEnvironment::default()
        };
        let selected = ProxySnapshot::select(environment, Some(system));
        assert_eq!(
            selected.source,
            crate::http_policy::ProxySource::Environment
        );
        assert_eq!(
            selected.environment.http.as_deref(),
            Some("http://env.local:8080")
        );
        assert!(selected.environment.https.is_none());

        let environment = ProxyEnvironment {
            no_proxy: vec!["custom.internal".to_owned(), "localhost".to_owned()],
            ..ProxyEnvironment::default()
        };
        let system = ProxyEnvironment {
            https: Some("http://system.local:8080".to_owned()),
            no_proxy: vec!["system.internal".to_owned()],
            ..ProxyEnvironment::default()
        };
        let selected = ProxySnapshot::select(environment, Some(system));
        assert_eq!(selected.source, crate::http_policy::ProxySource::System);
        assert!(
            selected
                .environment
                .no_proxy
                .contains(&"custom.internal".to_owned())
        );
        assert!(
            selected
                .environment
                .no_proxy
                .contains(&"system.internal".to_owned())
        );

        let invalid_system = ProxyEnvironment {
            http: Some("not a proxy".to_owned()),
            ..ProxyEnvironment::default()
        };
        let selected = ProxySnapshot::select(ProxyEnvironment::default(), Some(invalid_system));
        assert_eq!(selected.source, crate::http_policy::ProxySource::Direct);
        assert!(!selected.environment.has_proxy());
    }

    #[test]
    fn system_proxy_cache_uses_ttl_force_refresh_and_keeps_last_valid_on_error() {
        let cache = SystemProxyCache::default();
        let start = Instant::now();
        let calls = std::cell::Cell::new(0);
        let first = ProxyEnvironment {
            https: Some("http://first.system:8080".to_owned()),
            ..ProxyEnvironment::default()
        };
        assert_eq!(
            cache.resolve_with(
                || start,
                false,
                || {
                    calls.set(calls.get() + 1);
                    Ok(Some(first.clone()))
                }
            ),
            Some(first.clone())
        );
        assert_eq!(
            cache.resolve_with(
                || start + Duration::from_millis(999),
                false,
                || -> Result<Option<ProxyEnvironment>, ()> { panic!("fresh cache must not read") }
            ),
            Some(first)
        );
        let refreshed = ProxyEnvironment {
            https: Some("http://second.system:8080".to_owned()),
            ..ProxyEnvironment::default()
        };
        assert_eq!(
            cache.resolve_with(
                || start + Duration::from_secs(1),
                false,
                || {
                    calls.set(calls.get() + 1);
                    Ok(Some(refreshed.clone()))
                }
            ),
            Some(refreshed.clone())
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(
            cache.resolve_with(|| start + Duration::from_secs(2), true, || Err(())),
            Some(refreshed.clone()),
            "a failed forced read keeps the last valid proxy snapshot"
        );
        assert_eq!(
            cache.resolve_with(|| start + Duration::from_secs(2), true, || Ok(None)),
            None,
            "a successful direct read clears the previous proxy"
        );
    }
}
