use emp_state::{
    canonical_catalog_json, catalog_etag, normalize_catalog_presentations,
    normalize_codex_runtime_sources, normalize_provider_base_url, normalize_provider_id,
    normalize_subscription_search,
};
use serde_json::{Value, json};

/// Frozen config/ETag fixture. Value expectations come from the fixture;
/// failure cases assert error kind plus one stable key fragment.
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
fn configuration_failures_name_the_offending_field() {
    for (error, fragment) in [
        (
            normalize_catalog_presentations(Some(&json!([]))).unwrap_err(),
            "catalog_presentations",
        ),
        (
            normalize_catalog_presentations(Some(&json!({"bad route": {}}))).unwrap_err(),
            "route",
        ),
        (
            normalize_catalog_presentations(Some(&json!({"route": {"catalog_alias": 1}})))
                .unwrap_err(),
            "catalog_alias",
        ),
        (
            normalize_catalog_presentations(Some(&json!({"route": {"show_context": "yes"}})))
                .unwrap_err(),
            "show_context",
        ),
        (
            normalize_catalog_presentations(Some(&json!({
                "route": {"reasoning_summary": "raw-chain"}
            })))
            .unwrap_err(),
            "reasoning_summary",
        ),
        (
            normalize_subscription_search(Some(&json!({"enabled": 1}))).unwrap_err(),
            "subscription_search.enabled",
        ),
        (
            normalize_codex_runtime_sources(Some(&json!([]))).unwrap_err(),
            "codex_runtime_sources",
        ),
        (
            normalize_codex_runtime_sources(Some(&json!(["cursor", true]))).unwrap_err(),
            "codex_runtime_sources[1]",
        ),
        (
            normalize_codex_runtime_sources(Some(&json!(["unknown"]))).unwrap_err(),
            "unsupported",
        ),
        (
            normalize_codex_runtime_sources(Some(&json!(["auto", "cursor"]))).unwrap_err(),
            "auto",
        ),
    ] {
        assert!(
            error.to_string().contains(fragment),
            "{error:?} must name {fragment:?}"
        );
    }
}

#[test]
fn provider_identifier_and_url_failures_name_the_field() {
    let fixture = fixture();
    for case in fixture["provider_ids"]["invalid"]
        .as_array()
        .expect("invalid provider ID cases")
    {
        let error = normalize_provider_id(Some(&case["input"])).expect_err("invalid provider ID");
        assert!(
            error.to_string().contains("provider.id"),
            "{error:?} must name provider.id"
        );
    }
    for case in fixture["provider_base_urls"]["invalid"]
        .as_array()
        .expect("invalid provider URL cases")
    {
        let error =
            normalize_provider_base_url(Some(&case["input"])).expect_err("invalid provider URL");
        let rendered = error.to_string();
        let fragment = if rendered.contains("credentials") {
            "credentials"
        } else if rendered.contains("query") {
            "query"
        } else if rendered.contains("HTTPS") {
            "HTTPS"
        } else {
            "provider.base_url"
        };
        assert!(
            rendered.contains("provider.base_url") && rendered.contains(fragment),
            "{rendered:?} must name provider.base_url and {fragment:?}"
        );
    }
}

#[test]
fn oversized_presentation_values_fail_closed() {
    let long_alias = "界".repeat(171);
    assert!(long_alias.len() > 512);
    let error = normalize_catalog_presentations(Some(&json!({
        "route": {"catalog_alias": long_alias}
    })))
    .unwrap_err();
    assert!(error.to_string().contains("catalog_alias"));
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
