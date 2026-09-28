use emp_state::{
    canonical_catalog_json, catalog_etag, normalize_catalog_presentations,
    normalize_codex_runtime_sources, normalize_provider_base_url, normalize_provider_id,
    normalize_subscription_search,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-config-etag-normalization.json"
    ))
    .expect("valid synthetic config/ETag fixture")
}

#[test]
fn presentation_normalization_matches_fixture() {
    let fixture = fixture();
    let case = &fixture["presentations"];
    assert_eq!(
        normalize_catalog_presentations(Some(&case["input"])).expect("presentations"),
        case["expected"]
    );
}

#[test]
fn search_and_runtime_selection_match_fixture() {
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
fn provider_identifiers_and_urls_match_fixture() {
    let fixture = fixture();
    for case in fixture["provider_ids"]["valid"]
        .as_array()
        .expect("provider ID cases")
    {
        assert_eq!(
            normalize_provider_id(Some(&case["input"])).expect("valid provider ID"),
            case["expected"].as_str().expect("provider ID expected")
        );
    }
    for case in fixture["provider_base_urls"]["valid"]
        .as_array()
        .expect("provider URL cases")
    {
        assert_eq!(
            normalize_provider_base_url(Some(&case["input"])).expect("valid provider URL"),
            case["expected"].as_str().expect("provider URL expected")
        );
    }
}

#[test]
fn configuration_failures_are_bounded_and_specific() {
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

    let fixture = fixture();
    for case in fixture["provider_ids"]["invalid"]
        .as_array()
        .expect("invalid provider ID cases")
    {
        assert_eq!(
            normalize_provider_id(Some(&case["input"]))
                .expect_err("invalid provider ID")
                .to_string(),
            case["error"].as_str().expect("provider ID error")
        );
    }
    for case in fixture["provider_base_urls"]["invalid"]
        .as_array()
        .expect("invalid provider URL cases")
    {
        assert_eq!(
            normalize_provider_base_url(Some(&case["input"]))
                .expect_err("invalid provider URL")
                .to_string(),
            case["error"].as_str().expect("provider URL error")
        );
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
fn canonical_json_and_catalog_etag_are_stable_bytes() {
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
