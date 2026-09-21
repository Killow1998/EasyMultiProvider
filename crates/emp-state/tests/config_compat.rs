use emp_state::{
    canonical_catalog_json, catalog_etag, normalize_catalog_presentations,
    normalize_codex_runtime_sources, normalize_subscription_search,
};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-config-etag-normalization.json"
    ))
    .expect("valid synthetic Python config/ETag fixture")
}

#[test]
fn presentation_normalization_matches_python_fixture() {
    let fixture = fixture();
    let case = &fixture["presentations"];
    assert_eq!(
        normalize_catalog_presentations(Some(&case["input"])).expect("presentations"),
        case["expected"]
    );
}

#[test]
fn search_and_runtime_selection_match_python_fixture() {
    let fixture = fixture();
    let search = &fixture["subscription_search"];
    assert_eq!(
        normalize_subscription_search(Some(&search["input"])).expect("subscription search"),
        search["expected"]
    );
    let sources = &fixture["runtime_sources"];
    assert_eq!(
        normalize_codex_runtime_sources(Some(&sources["input"])).expect("runtime sources"),
        sources["expected"]
    );
    assert_eq!(
        normalize_subscription_search(Some(&Value::Null)).expect("null search"),
        json!({"enabled": false, "account_id": ""})
    );
    assert_eq!(
        normalize_codex_runtime_sources(Some(&Value::Null)).expect("null sources"),
        json!(["auto"])
    );
}

#[test]
fn configuration_failures_match_python_messages() {
    let cases = [
        (
            normalize_catalog_presentations(Some(&json!([]))).unwrap_err(),
            "catalog_presentations must be an object",
        ),
        (
            normalize_catalog_presentations(Some(&json!({"bad route": {}}))).unwrap_err(),
            "catalog_presentations route contains unsupported characters",
        ),
        (
            normalize_catalog_presentations(Some(&json!({"route": {"catalog_alias": 1}})))
                .unwrap_err(),
            "catalog_alias must be a string",
        ),
        (
            normalize_catalog_presentations(Some(&json!({"route": {"show_context": "yes"}})))
                .unwrap_err(),
            "show_context must be boolean",
        ),
        (
            normalize_catalog_presentations(Some(&json!({
                "route": {"reasoning_summary": "raw-chain"}
            })))
            .unwrap_err(),
            "reasoning_summary must be auto, show, or hide",
        ),
        (
            normalize_subscription_search(Some(&json!({"enabled": 1}))).unwrap_err(),
            "subscription_search.enabled must be boolean",
        ),
        (
            normalize_codex_runtime_sources(Some(&json!([]))).unwrap_err(),
            "codex_runtime_sources must be a non-empty list",
        ),
        (
            normalize_codex_runtime_sources(Some(&json!(["cursor", true]))).unwrap_err(),
            "codex_runtime_sources[1] must be a string",
        ),
        (
            normalize_codex_runtime_sources(Some(&json!(["unknown"]))).unwrap_err(),
            "codex_runtime_sources contains an unsupported source",
        ),
        (
            normalize_codex_runtime_sources(Some(&json!(["auto", "cursor"]))).unwrap_err(),
            "codex_runtime_sources auto cannot be combined",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }

    let long_alias = "界".repeat(171);
    assert!(long_alias.len() > 512);
    assert_eq!(
        normalize_catalog_presentations(Some(&json!({
            "route": {"catalog_alias": long_alias}
        })))
        .unwrap_err()
        .to_string(),
        "catalog_alias is too long"
    );
}

#[test]
fn canonical_json_and_catalog_etag_match_python_bytes() {
    let fixture = fixture();
    let case = &fixture["canonical_etag"];
    let canonical = canonical_catalog_json(&case["input"]).expect("canonical encoding");
    assert_eq!(
        String::from_utf8(canonical).expect("UTF-8 canonical JSON"),
        case["canonical_json"].as_str().expect("canonical fixture")
    );
    assert_eq!(
        catalog_etag(&case["input"]).expect("catalog ETag"),
        case["etag"].as_str().expect("ETag fixture")
    );
}

#[test]
fn python_config_helpers_match_live_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import hashlib, json, sys
from easy_multi_provider.config import (
    _normalize_catalog_presentations,
    _normalize_subscription_search,
    _normalize_codex_runtime_sources,
)
value = json.load(sys.stdin)
catalog = value["canonical_etag"]["input"]
canonical = json.dumps(catalog, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
json.dump({
    "presentations": _normalize_catalog_presentations(value["presentations"]["input"]),
    "subscription_search": _normalize_subscription_search(value["subscription_search"]["input"], set()),
    "runtime_sources": _normalize_codex_runtime_sources(value["runtime_sources"]["input"]),
    "canonical_json": canonical,
    "etag": '"emp-' + hashlib.sha256(canonical.encode("utf-8")).hexdigest() + '"',
}, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn configured Python oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write Python fixture");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
    let input = &fixture["canonical_etag"]["input"];
    let rust = json!({
        "presentations": normalize_catalog_presentations(Some(&fixture["presentations"]["input"]))
            .expect("Rust presentations"),
        "subscription_search": normalize_subscription_search(Some(
            &fixture["subscription_search"]["input"]
        ))
        .expect("Rust subscription search"),
        "runtime_sources": normalize_codex_runtime_sources(Some(
            &fixture["runtime_sources"]["input"]
        ))
        .expect("Rust runtime sources"),
        "canonical_json": String::from_utf8(
            canonical_catalog_json(input).expect("Rust canonical JSON")
        )
        .expect("UTF-8 Rust canonical JSON"),
        "etag": catalog_etag(input).expect("Rust ETag"),
    });
    assert_eq!(rust, oracle);
}
