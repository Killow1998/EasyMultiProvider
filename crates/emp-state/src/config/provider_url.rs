//! Provider URL and identifier normalization at the configuration boundary.

use super::{ConfigError, ConfigResult, Value, python_capability_trim};

#[derive(Debug)]
pub(super) struct ParsedProviderUrl<'a> {
    pub(super) scheme: String,
    pub(super) netloc: &'a str,
    pub(super) path: &'a str,
    pub(super) params: &'a str,
    pub(super) query: &'a str,
    pub(super) fragment: &'a str,
}

fn split_scheme(value: &str) -> Option<(&str, &str)> {
    let (scheme, remainder) = value.split_once(':')?;
    let mut characters = scheme.chars();
    if !characters.next()?.is_ascii_alphabetic()
        || !characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
    {
        return None;
    }
    Some((scheme, remainder))
}

fn split_path_params(path: &str) -> (&str, &str) {
    let search_from = path.rfind('/').map_or(0, |index| index + 1);
    match path[search_from..].find(';') {
        Some(relative) => {
            let index = search_from + relative;
            (&path[..index], &path[index + 1..])
        }
        None => (path, ""),
    }
}

pub(super) fn parse_provider_url(value: &str) -> Option<ParsedProviderUrl<'_>> {
    let (scheme, remainder) = split_scheme(value)?;
    let authority_and_rest = remainder.strip_prefix("//")?;
    let authority_end = authority_and_rest
        .find(['/', '?', '#'])
        .unwrap_or(authority_and_rest.len());
    let netloc = &authority_and_rest[..authority_end];
    let rest = &authority_and_rest[authority_end..];
    let (before_fragment, fragment) = rest
        .split_once('#')
        .map_or((rest, ""), |(head, tail)| (head, tail));
    let (path_with_params, query) = before_fragment
        .split_once('?')
        .map_or((before_fragment, ""), |(head, tail)| (head, tail));
    let (path, params) = split_path_params(path_with_params);
    Some(ParsedProviderUrl {
        scheme: scheme.to_ascii_lowercase(),
        netloc,
        path,
        params,
        query,
        fragment,
    })
}

pub(super) fn userinfo(netloc: &str) -> (&str, Option<&str>) {
    let Some((raw, _)) = netloc.rsplit_once('@') else {
        return ("", None);
    };
    raw.split_once(':')
        .map_or((raw, None), |(username, password)| {
            (username, Some(password))
        })
}

pub(super) fn hostname(netloc: &str) -> Option<&str> {
    let host_port = netloc
        .rsplit_once('@')
        .map_or(netloc, |(_, host_port)| host_port);
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split_once(']').map(|(host, _)| host);
    }
    Some(
        host_port
            .rsplit_once(':')
            .map_or(host_port, |(host, _)| host),
    )
}

pub(super) fn string_value(
    raw: Option<&Value>,
    field: &str,
    required: bool,
) -> ConfigResult<String> {
    let value = match raw {
        None | Some(Value::Null) => "",
        Some(Value::String(value)) => python_capability_trim(value),
        Some(_) => return Err(ConfigError::new(format!("{field} must be a string"))),
    };
    if required && value.is_empty() {
        return Err(ConfigError::new(format!("{field} is required")));
    }
    Ok(value.to_owned())
}

/// Validate and trim an external provider identifier exactly like Python.
pub fn normalize_provider_id(raw: Option<&Value>) -> ConfigResult<String> {
    let value = string_value(raw, "provider.id", true)?;
    let mut characters = value.chars();
    let valid = value.len() <= 64
        && characters
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        });
    if !valid {
        return Err(ConfigError::new(
            "provider.id must be a safe single path segment",
        ));
    }
    Ok(value)
}

fn strip_terminal_path<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    if path.len() < suffix.len() {
        return None;
    }
    let root_end = path.len() - suffix.len();
    if !path.get(root_end..)?.eq_ignore_ascii_case(suffix) {
        return None;
    }
    path.get(..root_end)
}

fn normalize_provider_path(path: &str) -> String {
    let mut path = path.trim_end_matches('/').to_owned();
    for suffix in [
        "/responses/compact",
        "/chat/completions",
        "/response",
        "/responses",
        "/messages",
        "/models",
    ] {
        if let Some(root) = strip_terminal_path(&path, suffix) {
            path.truncate(root.len());
            break;
        }
    }

    let mut prefix_end = path.len();
    let mut v1_count = 0;
    while prefix_end >= 3
        && path
            .get(prefix_end - 3..prefix_end)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("/v1"))
    {
        prefix_end -= 3;
        v1_count += 1;
    }
    if v1_count >= 2 {
        path.truncate(prefix_end);
        path.push_str("/v1");
    }
    if path.is_empty() {
        path.push_str("/v1");
    }
    path
}

/// Turn a pasted provider origin or request URL into the API root EMP owns.
pub fn normalize_provider_base_url(raw: Option<&Value>) -> ConfigResult<String> {
    let entered = string_value(raw, "provider.base_url", true)?;
    let entered = entered.trim_end_matches('/');
    let parsed = parse_provider_url(entered)
        .ok_or_else(|| ConfigError::new("provider.base_url must be an http(s) URL"))?;
    if !matches!(parsed.scheme.as_str(), "http" | "https") || parsed.netloc.is_empty() {
        return Err(ConfigError::new("provider.base_url must be an http(s) URL"));
    }
    let (username, password) = userinfo(parsed.netloc);
    if !username.is_empty() || password.is_some_and(|value| !value.is_empty()) {
        return Err(ConfigError::new(
            "provider.base_url must not contain URL credentials",
        ));
    }
    if !parsed.query.is_empty() || !parsed.fragment.is_empty() {
        return Err(ConfigError::new(
            "provider.base_url must not contain a query or fragment",
        ));
    }
    let host = hostname(parsed.netloc).unwrap_or("").to_ascii_lowercase();
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if parsed.scheme == "http" && !loopback {
        return Err(ConfigError::new(
            "provider.base_url must use HTTPS unless it targets loopback",
        ));
    }

    let path = normalize_provider_path(parsed.path);
    let mut normalized = format!("{}://{}{}", parsed.scheme, parsed.netloc, path);
    if !parsed.params.is_empty() {
        normalized.push(';');
        normalized.push_str(parsed.params);
    }
    Ok(normalized.trim_end_matches('/').to_owned())
}
