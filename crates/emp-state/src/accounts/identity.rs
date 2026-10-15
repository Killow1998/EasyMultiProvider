//! Local credential identity comparison; no token or identity is logged.
use super::{MAX_AUTH_BYTES, python_trim};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use std::collections::BTreeSet;

fn auth_identities(auth: &Value) -> BTreeSet<(String, String)> {
    let Some(auth) = auth.as_object() else {
        return BTreeSet::new();
    };
    let tokens = auth
        .get("tokens")
        .and_then(Value::as_object)
        .unwrap_or(auth);
    let mut identities = BTreeSet::new();
    for source in [tokens, auth] {
        for key in ["account_id", "chatgpt_account_id"] {
            if let Some(value) = source
                .get(key)
                .and_then(Value::as_str)
                .map(python_trim)
                .filter(|value| !value.is_empty())
            {
                identities.insert(("account_id".to_owned(), value.to_owned()));
            }
        }
        for key in ["chatgpt_user_id", "user_id"] {
            if let Some(value) = source
                .get(key)
                .and_then(Value::as_str)
                .map(python_trim)
                .filter(|value| !value.is_empty())
            {
                identities.insert(("user_id".to_owned(), value.to_owned()));
            }
        }
    }
    for token in ["id_token", "access_token"] {
        let claims = tokens
            .get(token)
            .and_then(Value::as_str)
            .and_then(|jwt| jwt.split('.').nth(1))
            .filter(|payload| payload.len() <= MAX_AUTH_BYTES)
            .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if let Some(claims) =
            claims.and_then(|claims| claims.get("https://api.openai.com/auth").cloned())
        {
            for (key, kind) in [
                ("chatgpt_account_id", "account_id"),
                ("chatgpt_user_id", "user_id"),
                ("user_id", "user_id"),
            ] {
                if let Some(value) = claims
                    .get(key)
                    .and_then(Value::as_str)
                    .map(python_trim)
                    .filter(|value| !value.is_empty())
                {
                    identities.insert((kind.to_owned(), value.to_owned()));
                }
            }
        }
    }
    if let Some(value) = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .map(python_trim)
        .filter(|value| !value.is_empty())
    {
        identities.insert(("access_token".to_owned(), value.to_owned()));
    }
    identities
}

/// Compare account and user identity without treating a shared workspace as a user.
/// Legacy opaque credentials retain account-ID matching; contradictory known
/// users/accounts never merge. Exact access tokens are the final fallback.
pub fn same_account_auth(left: &Value, right: &Value) -> bool {
    let left = auth_identities(left);
    let right = auth_identities(right);
    let left_accounts = left
        .iter()
        .filter(|(kind, _)| kind == "account_id")
        .map(|(_, value)| value)
        .collect::<BTreeSet<_>>();
    let right_accounts = right
        .iter()
        .filter(|(kind, _)| kind == "account_id")
        .map(|(_, value)| value)
        .collect::<BTreeSet<_>>();
    let users = |identities: &BTreeSet<(String, String)>| {
        identities
            .iter()
            .filter(|(kind, _)| kind == "user_id")
            .map(|(_, value)| value.clone())
            .collect::<BTreeSet<_>>()
    };
    let left_users = users(&left);
    let right_users = users(&right);
    if !left_users.is_empty() && !right_users.is_empty() && left_users != right_users {
        return false;
    }
    let same_token = left
        .iter()
        .any(|identity| identity.0 == "access_token" && right.contains(identity));
    if !left_accounts.is_empty() && !right_accounts.is_empty() {
        return left_accounts == right_accounts && (left_users == right_users || same_token);
    }
    same_token
}
