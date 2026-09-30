use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoginStatus {
    SubscriptionOAuth,
    Missing,
    ApiKey,
    Unknown,
}

/// Read only the supported status fields; all other CLI output stays private.
pub(super) fn parse_status(stdout: &[u8]) -> LoginStatus {
    let Ok(status) = serde_json::from_slice::<Value>(stdout) else {
        return LoginStatus::Unknown;
    };
    let Some(logged_in) = status.get("loggedIn").and_then(Value::as_bool) else {
        return LoginStatus::Unknown;
    };
    let method = status.get("authMethod").and_then(Value::as_str);
    let provider = status.get("apiProvider").and_then(Value::as_str);
    match (logged_in, method, provider) {
        (true, Some("claude.ai"), Some("firstParty")) => LoginStatus::SubscriptionOAuth,
        (_, Some("api_key"), _) => LoginStatus::ApiKey,
        (false, Some("none"), _) => LoginStatus::Missing,
        _ => LoginStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_status_accepts_only_observed_cli_methods_and_never_identity_fields() {
        for (status, expected) in [
            (
                br#"{"loggedIn":true,"authMethod":"oauth_token","email":"private","configDirectory":"/private/path"}"#.as_slice(),
                LoginStatus::Unknown,
            ),
            (
                br#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"private","configDirectory":"/private/path"}"#.as_slice(),
                LoginStatus::SubscriptionOAuth,
            ),
            (
                br#"{"loggedIn":true,"authMethod":"api_key","account":"private"}"#.as_slice(),
                LoginStatus::ApiKey,
            ),
            (
                br#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty","configDirectory":"/private/path"}"#.as_slice(),
                LoginStatus::Missing,
            ),
            (
                br#"{"loggedIn":true,"authMethod":"unrecognized"}"#.as_slice(),
                LoginStatus::Unknown,
            ),
            (
                br#"{"loggedIn":false,"authMethod":"claude.ai","apiProvider":"firstParty"}"#.as_slice(),
                LoginStatus::Unknown,
            ),
            (br#"{"loggedIn":true}"#.as_slice(), LoginStatus::Unknown),
            (b"not JSON".as_slice(), LoginStatus::Unknown),
        ] {
            assert_eq!(parse_status(status), expected);
        }
    }
}
