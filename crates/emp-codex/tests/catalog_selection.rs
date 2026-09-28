use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use emp_codex::{
    account_auth_headers, load_native_catalog, native_catalog_owner, subscription_route_model,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;

fn jwt(user: &str, suffix: &str) -> String {
    let claims = json!({"https://api.openai.com/auth": {"chatgpt_user_id": user}});
    format!(
        "fixture.{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims JSON")),
        suffix
    )
}

fn headers(user: &str, workspace: &str, suffix: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "Authorization".to_owned(),
            format!("Bearer {}", jwt(user, suffix)),
        ),
        ("chatgpt-account-id".to_owned(), workspace.to_owned()),
    ])
}

fn model(
    config: &Value,
    slug: &str,
    account: Option<&Value>,
    headers: &BTreeMap<String, String>,
) -> Value {
    subscription_route_model(
        config.as_object().expect("config object"),
        slug,
        account.and_then(Value::as_object),
        |_| Some(headers.clone()),
    )
    .map(Value::Object)
    .unwrap_or(Value::Null)
}

fn rust_result(fixture: &Value) -> Value {
    let config = &fixture["config"];
    let good_headers =
        serde_json::from_value::<BTreeMap<String, String>>(fixture["headers"]["good"].clone())
            .expect("good headers");
    let opaque_headers =
        serde_json::from_value::<BTreeMap<String, String>>(fixture["headers"]["opaque"].clone())
            .expect("opaque headers");
    let accounts = fixture["accounts"].as_array().expect("accounts");
    json!({
        "owners": [
            native_catalog_owner(&good_headers),
            native_catalog_owner(&opaque_headers),
        ],
        "auth_headers": account_auth_headers(&fixture["auth"]),
        "auth_header_cases": fixture["auth_header_cases"].as_array().unwrap().iter()
            .map(account_auth_headers).collect::<Vec<_>>(),
        "native_catalog": load_native_catalog(config),
        "native_model": model(config, "shared", None, &good_headers),
        "unsupported": model(config, "unsupported", None, &good_headers),
        "account_model": model(config, "shared", Some(&accounts[0]), &good_headers),
        "wrong_owner_fallback": model(config, "shared", Some(&accounts[1]), &good_headers),
        "wrong_base_fallback": model(config, "shared", Some(&accounts[2]), &good_headers),
    })
}

#[test]
fn catalog_selection_resolves_context_windows_and_owner_fallbacks() {
    let root = tempfile::tempdir().expect("temporary root");
    let native = root.path().join("models_cache.json");
    let preserved = root
        .path()
        .join("easy-multi-provider")
        .join("native-catalog.json");
    fs::create_dir_all(preserved.parent().expect("preserved parent")).expect("create preserved");
    fs::write(
        &native,
        serde_json::to_vec(&json!({
            "etag": "\"emp-generated\"",
            "models": [{"slug": "generated-only"}],
        }))
        .expect("generated catalog"),
    )
    .expect("write generated catalog");
    fs::write(
        &preserved,
        serde_json::to_vec(&json!({
            "etag": "native-etag",
            "models": [
                {
                    "slug": "shared",
                    "context_window": 200,
                    "max_context_window": 500,
                    "auto_compact_token_limit": 190,
                },
                {"slug": "unsupported", "supported_in_api": false},
            ],
        }))
        .expect("native catalog"),
    )
    .expect("write preserved catalog");

    let good_headers = headers("user-a", "workspace-a", "one");
    let opaque_headers = BTreeMap::from([
        ("Authorization".to_owned(), "Bearer opaque-token".to_owned()),
        ("chatgpt-account-id".to_owned(), "workspace-a".to_owned()),
    ]);
    let mut accounts = Vec::new();
    for (name, owner, base_url) in [
        (
            "good",
            native_catalog_owner(&good_headers),
            "https://chatgpt.example/codex",
        ),
        (
            "wrong-owner",
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned(),
            "https://chatgpt.example/codex",
        ),
        (
            "wrong-base",
            native_catalog_owner(&good_headers),
            "https://other.example/codex",
        ),
    ] {
        let directory = root.path().join(name);
        fs::create_dir_all(&directory).expect("create account directory");
        fs::write(
            directory.join("models_cache.json"),
            serde_json::to_vec(&json!({
                "account_owner": owner,
                "base_url": base_url,
                "models": [{
                    "slug": "shared",
                    "context_window": 1000,
                    "max_context_window": 900,
                    "auto_compact_token_limit": 800,
                }],
            }))
            .expect("account catalog"),
        )
        .expect("write account catalog");
        accounts.push(json!({
            "id": name,
            "auth_file": directory.join("auth.json.enc"),
            "model_context_windows": {"shared": 700},
        }));
    }
    let fixture = json!({
        "config": {
            "native_catalog_path": native,
            "codex_base_url": "https://chatgpt.example/codex",
            "native_model_context_windows": {"shared": 150},
        },
        "headers": {"good": good_headers, "opaque": opaque_headers},
        "auth": {
            "tokens": {
                "access_token": jwt("user-a", "auth"),
                "account_id": "workspace-a",
            }
        },
        "auth_header_cases": [
            {"tokens":{"access_token":"token","account_id":""},"account_id":"root"},
            {"tokens":{"access_token":"token","account_id":null},"account_id":"root"},
            {"tokens":{"access_token":"token","account_id":false},"account_id":"root"},
            {"tokens":{"access_token":"token","account_id":0},"account_id":"root"},
            {"tokens":{"access_token":"token","account_id":[]},"account_id":"root"},
            {"tokens":{"access_token":"token","account_id":{}},"account_id":"root"},
            {"tokens":{"access_token":"token","account_id":["not-a-string"]},"account_id":"root"},
            {"tokens":{"access_token":"token","account_id":"nested"},"account_id":"root"}
        ],
        "accounts": accounts,
    });
    let rust = rust_result(&fixture);

    // Direct user-facing expectations, previously only checked against the
    // Python oracle: catalog context windows resolve native > manual account
    // > preserved native fallback, and unsupported models stay unroutable.
    assert_ne!(rust["owners"][0], rust["owners"][1]);
    assert_eq!(rust["unsupported"], Value::Null);
    assert_eq!(rust["native_model"]["context_window"], 150);
    assert_eq!(rust["account_model"]["context_window"], 700);
    assert_eq!(rust["wrong_owner_fallback"]["context_window"], 500);
    assert_eq!(rust["wrong_base_fallback"]["context_window"], 500);
    assert_eq!(
        rust["auth_headers"],
        json!({"Authorization": format!("Bearer {}", jwt("user-a", "auth")), "chatgpt-account-id": "workspace-a"})
    );
    // Falsy token account ids (null, false, 0, empty, empty containers) fall
    // back to the root account id. A non-empty non-string id is kept but
    // unusable, so no header is emitted, while a stringy token account id
    // wins over the root fallback.
    for case in &rust["auth_header_cases"].as_array().unwrap()[..6] {
        assert_eq!(case["chatgpt-account-id"], json!("root"), "{case}");
    }
    assert!(
        rust["auth_header_cases"][6]
            .get("chatgpt-account-id")
            .is_none(),
        "{}",
        rust["auth_header_cases"][6]
    );
    assert_eq!(
        rust["auth_header_cases"][7]["chatgpt-account-id"],
        json!("nested")
    );
}

#[test]
fn owner_changes_for_opaque_credentials_but_not_rotated_jwts() {
    assert_eq!(
        native_catalog_owner(&headers("user", "workspace", "one")),
        native_catalog_owner(&headers("user", "workspace", "two")),
    );
    let opaque = |token: &str| {
        BTreeMap::from([
            ("Authorization".to_owned(), format!("Bearer opaque-{token}")),
            ("chatgpt-account-id".to_owned(), "workspace".to_owned()),
        ])
    };
    assert_ne!(
        native_catalog_owner(&opaque("one")),
        native_catalog_owner(&opaque("two")),
    );
}
