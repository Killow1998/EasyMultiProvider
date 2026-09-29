use emp_state::{merge_web_update_with_time, normalize_configuration};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-web-update-merge.json"
    ))
    .expect("valid Web-update merge fixture")
}

#[test]
fn web_update_merge_matches_frozen_fixture() {
    let fixture = fixture();
    let observed_at = fixture["fixed_observed_at"]
        .as_str()
        .expect("fixed observed_at");
    for case in fixture["valid"].as_array().expect("valid merge cases") {
        let current =
            normalize_configuration(Some(&case["current"])).expect("valid current configuration");
        let actual = merge_web_update_with_time(&current, &case["incoming"], observed_at)
            .expect("valid Web update");
        assert_eq!(
            actual["catalog_show_context"], current["catalog_show_context"],
            "case: {}",
            case["name"]
        );
        for pointer in case["select"].as_array().expect("selected paths") {
            let pointer = pointer.as_str().expect("JSON pointer");
            assert_eq!(
                actual.pointer(pointer),
                case["expected"].get(pointer),
                "case: {}, pointer: {pointer}",
                case["name"]
            );
        }
    }
    for case in fixture["invalid"].as_array().expect("invalid merge cases") {
        let current =
            normalize_configuration(Some(&case["current"])).expect("valid current configuration");
        let error = merge_web_update_with_time(&current, &case["incoming"], observed_at)
            .expect_err("invalid Web update");
        assert_eq!(
            error.python_type(),
            case["error_type"],
            "case: {}",
            case["name"]
        );
        assert_eq!(error.to_string(), case["error"], "case: {}", case["name"]);
    }
}

fn provider_current(api_key: &str, api_key_file: &str) -> Value {
    normalize_configuration(Some(&json!({
        "secret_store_path": "/managed/secrets",
        "providers": [{
            "id": "deepseek",
            "name": "DeepSeek",
            "base_url": "https://api.deepseek.com/v1",
            "protocol": "chat_completions",
            "api_key": api_key,
            "api_key_file": api_key_file,
        }],
    })))
    .expect("valid current configuration")
}

fn provider_update(base_url: &str, api_key: Option<&str>) -> Value {
    let mut provider = json!({
        "id": "deepseek",
        "name": "DeepSeek",
        "base_url": base_url,
        "protocol": "chat_completions",
    });
    if let Some(api_key) = api_key {
        provider["api_key"] = Value::from(api_key);
    }
    json!({"providers": [provider]})
}

const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";
const AT: &str = "2026-08-22T00:00:00+00:00";

#[test]
fn changing_provider_origin_does_not_carry_over_a_stored_key() {
    for base_url in [
        "https://attacker.example/v1",
        "https://api.deepseek.com:8443/v1",
        "http://127.0.0.1:8080/v1",
    ] {
        for api_key in [None, Some(MASK)] {
            let current = provider_current("secret-value", "");
            let merged =
                merge_web_update_with_time(&current, &provider_update(base_url, api_key), AT)
                    .expect("valid Web update");
            assert_eq!(
                merged["providers"][0]["api_key"], "",
                "{base_url} {api_key:?}"
            );
            assert_eq!(merged["providers"][0]["api_key_file"], "", "{base_url}");

            let current = provider_current("", "/managed/secrets/deepseek.key.enc");
            let merged =
                merge_web_update_with_time(&current, &provider_update(base_url, api_key), AT)
                    .expect("valid Web update");
            assert_eq!(merged["providers"][0]["api_key"], "", "{base_url}");
            assert_eq!(
                merged["providers"][0]["api_key_file"], "",
                "managed secret must not follow {base_url}"
            );
        }
    }
}

#[test]
fn rejected_provider_urls_never_reach_the_key_carry_over() {
    // Plain HTTP to a remote host and URLs with userinfo are refused outright,
    // so the stored key cannot follow them either.
    for base_url in [
        "http://api.deepseek.com/v1",
        "https://user@attacker.example/v1",
    ] {
        for api_key in [None, Some(MASK)] {
            let current = provider_current("secret-value", "");
            merge_web_update_with_time(&current, &provider_update(base_url, api_key), AT)
                .expect_err(base_url);
        }
    }
}

#[test]
fn changing_provider_origin_accepts_an_explicit_new_key() {
    let current = provider_current("", "/managed/secrets/deepseek.key.enc");
    let merged = merge_web_update_with_time(
        &current,
        &provider_update("https://other.example/v1", Some("new-secret")),
        AT,
    )
    .expect("valid Web update");
    assert_eq!(merged["providers"][0]["api_key"], "new-secret");
    assert_eq!(merged["providers"][0]["api_key_file"], "");
}

#[test]
fn same_origin_edits_keep_the_stored_key() {
    for base_url in [
        "https://api.deepseek.com/v2",
        "https://API.DeepSeek.com:443/v1",
        "https://api.deepseek.com/v1/",
    ] {
        let current = provider_current("secret-value", "");
        let merged =
            merge_web_update_with_time(&current, &provider_update(base_url, Some(MASK)), AT)
                .expect("valid Web update");
        assert_eq!(
            merged["providers"][0]["api_key"], "secret-value",
            "{base_url}"
        );

        let current = provider_current("", "/managed/secrets/deepseek.key.enc");
        let merged = merge_web_update_with_time(&current, &provider_update(base_url, None), AT)
            .expect("valid Web update");
        assert_eq!(
            merged["providers"][0]["api_key_file"], "/managed/secrets/deepseek.key.enc",
            "{base_url}"
        );
    }
}

#[test]
fn unrelated_web_edit_preserves_hidden_catalog_context_labels() {
    let current = normalize_configuration(Some(&json!({"catalog_show_context": false})))
        .expect("valid current configuration");
    let incoming = json!({"subscription_search": {"enabled": true}});
    let merged = merge_web_update_with_time(&current, &incoming, AT).expect("valid Web update");

    assert_eq!(
        merged["subscription_search"]["enabled"].as_bool(),
        Some(true)
    );
    assert_eq!(merged["catalog_show_context"].as_bool(), Some(false));

    let explicit_update = json!({
        "catalog_show_context": true,
        "subscription_search": {"enabled": false}
    });
    let explicitly_updated =
        merge_web_update_with_time(&current, &explicit_update, AT).expect("valid Web update");
    assert_eq!(
        explicitly_updated["catalog_show_context"].as_bool(),
        Some(true)
    );

    let invalid_update = json!({"catalog_show_context": "false"});
    let error = merge_web_update_with_time(&current, &invalid_update, AT)
        .expect_err("non-boolean context preference");
    assert!(error.to_string().contains("catalog_show_context"));
}
