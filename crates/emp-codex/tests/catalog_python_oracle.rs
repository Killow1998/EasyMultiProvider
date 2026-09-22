use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use emp_codex::{
    account_auth_headers, load_native_catalog, native_catalog_owner, subscription_route_model,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

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
fn catalog_selection_matches_live_python_oracle_when_configured() {
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

    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        assert_eq!(rust["native_model"]["context_window"], 150);
        assert_eq!(rust["account_model"]["context_window"], 700);
        assert_eq!(rust["wrong_owner_fallback"]["context_window"], 500);
        return;
    };
    let script = r#"
import json, sys
from unittest.mock import patch
from easy_multi_provider.accounts import _auth_headers_from_value
from easy_multi_provider.catalog import account_catalog_owner_from_headers, load_native_catalog, subscription_route_model

fixture = json.load(sys.stdin)
config = fixture["config"]
good = fixture["headers"]["good"]
opaque = fixture["headers"]["opaque"]
accounts = fixture["accounts"]

def route(slug, account=None):
    with patch("easy_multi_provider.catalog.auth_headers", return_value=good):
        return subscription_route_model(config, slug, account)

json.dump({
    "owners": [
        account_catalog_owner_from_headers(good),
        account_catalog_owner_from_headers(opaque),
    ],
    "auth_headers": _auth_headers_from_value(fixture["auth"]),
    "auth_header_cases": [_auth_headers_from_value(value) for value in fixture["auth_header_cases"]],
    "native_catalog": load_native_catalog(config),
    "native_model": route("shared"),
    "unsupported": route("unsupported"),
    "account_model": route("shared", accounts[0]),
    "wrong_owner_fallback": route("shared", accounts[1]),
    "wrong_base_fallback": route("shared", accounts[2]),
}, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python catalog oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write fixture");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python catalog oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python: Value = serde_json::from_slice(&output.stdout).expect("Python oracle output");
    assert_eq!(rust, python);
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
