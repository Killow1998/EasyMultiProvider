use emp_codex::subscription_contexts::validate_subscription_contexts;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

#[test]
fn subscription_context_validation_replays_python_subscription_tests()
-> Result<(), Box<dyn std::error::Error>> {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return Ok(());
    };
    let script = r#"
import copy, importlib, io, json, sys, unittest
from unittest.mock import patch
from easy_multi_provider import catalog, server
from tests import test_subscription_context

original_options = catalog.subscription_model_options
option_cases = []

def observed_options(config, account=None):
    result = original_options(config, account)
    source = (
        catalog._account_catalog(config, account)
        if account is not None
        else catalog.load_native_catalog(config)
    )
    option_cases.append(copy.deepcopy({
        'native': catalog.load_native_catalog(config),
        'account': account,
        'account_catalog': source,
        'expected': result,
    }))
    return result

catalog.subscription_model_options = observed_options

original_validate = catalog.validate_subscription_contexts
validation_cases = []

def observed_validate(config, previous=None):
    try:
        original_validate(config, previous)
        error = None
    except ValueError as caught:
        error = str(caught)
    validation_cases.append(copy.deepcopy({
        'config': config,
        'previous': previous,
        'native': catalog.load_native_catalog(config),
        'account_catalogs': {
            account['id']: catalog._account_catalog(config, account)
            for account in config.get('accounts', [])
        },
        'error': error,
    }))
    if error is not None:
        raise ValueError(error)

catalog.validate_subscription_contexts = observed_validate
server.validate_subscription_contexts = observed_validate

suite = unittest.TestSuite([
    test_subscription_context.SubscriptionContextTests(
        'test_upper_bound_comes_from_catalog_and_reduced_limits_clamp_existing_settings'
    ),
    test_subscription_context.SubscriptionContextTests(
        'test_missing_and_invalid_maxima_do_not_invent_a_one_million_limit'
    ),
])
stream = io.StringIO()
result = unittest.TextTestRunner(stream=stream).run(suite)
if not result.wasSuccessful():
    sys.stderr.write(stream.getvalue())
    raise SystemExit(1)
json.dump({'options': option_cases, 'validations': validation_cases}, sys.stdout)
"#;
    let home = tempfile::tempdir()?;
    let output = Command::new(python)
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .env("CODEX_HOME", home.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("oracle fixtures");

    for case in oracle["options"].as_array().expect("option cases") {
        let catalog = &case["account_catalog"];
        // Reuse the public management projection for the same resolved source.
        assert_eq!(
            json!(emp_codex::management_views::subscription_model_options(
                catalog
            )),
            case["expected"]
        );
    }
    let validations = oracle["validations"].as_array().expect("validation cases");
    assert!(
        validations.len() >= 4,
        "existing Python tests must exercise validation"
    );
    for case in validations {
        let native = case["native"].clone();
        let account_catalogs: BTreeMap<String, Value> =
            serde_json::from_value(case["account_catalogs"].clone()).expect("account catalogs");
        let actual = validate_subscription_contexts(
            &case["config"],
            (!case["previous"].is_null()).then_some(&case["previous"]),
            &native,
            &account_catalogs,
        );
        match case["error"].as_str() {
            None => assert!(actual.is_ok()),
            Some(expected) => assert_eq!(
                actual.expect_err("expected ValueError").to_string(),
                expected
            ),
        }
    }
    Ok(())
}

#[test]
fn validation_reports_native_source_before_account_sources() {
    let catalog = json!({"models":[]});
    let config = json!({
        "native_model_context_windows":{"native":1},
        "accounts":[
            {"id":"first","model_context_windows":{"first":1}},
            {"id":"second","model_context_windows":{"second":1}},
        ],
    });
    let error = validate_subscription_contexts(&config, None, &catalog, &BTreeMap::new())
        .expect_err("native invalid settings are rejected first");
    assert_eq!(
        error.to_string(),
        "Context for native exceeds the subscription catalog limit (0 tokens); refresh models first"
    );

    let valid_native = json!({
        "native_model_context_windows":{},
        "accounts":[
            {"id":"first","model_context_windows":{"first":1}},
            {"id":"second","model_context_windows":{"second":1}},
        ],
    });
    let error = validate_subscription_contexts(&valid_native, None, &catalog, &BTreeMap::new())
        .expect_err("account sources follow native configuration order");
    assert_eq!(
        error.to_string(),
        "Context for first exceeds the subscription catalog limit (0 tokens); refresh models first"
    );
}

#[test]
fn changed_settings_follow_current_limits_and_unchanged_settings_remain_allowed() {
    let native = json!({"models":[
        {"slug":"model","context_window":272000,"max_context_window":700000},
        {"slug":"invalid","context_window":272000,"max_context_window":"1000000"},
    ]});
    let previous = json!({
        "native_model_context_windows":{"model":1000001},
        "accounts":[{"id":"account","model_context_windows":{"model":700001}}],
    });
    let config = json!({
        "native_model_context_windows":{"model":700000},
        "accounts":[{"id":"account","model_context_windows":{"model":700001}}],
    });
    let catalogs = BTreeMap::from([("account".to_owned(), native.clone())]);
    assert!(validate_subscription_contexts(&config, Some(&previous), &native, &catalogs).is_ok());

    let unchanged = json!({
        "native_model_context_windows":{"model":1000001},
        "accounts":[{"id":"account","model_context_windows":{"model":700001}}],
    });
    assert!(
        validate_subscription_contexts(&unchanged, Some(&previous), &native, &catalogs).is_ok()
    );

    let over_limit = json!({
        "native_model_context_windows":{"model":700001},
        "accounts":[{"id":"account","model_context_windows":{}}],
    });
    let error = validate_subscription_contexts(&over_limit, None, &native, &catalogs)
        .expect_err("unknown or over-limit settings must be rejected");
    assert_eq!(
        error.to_string(),
        "Context for model exceeds the subscription catalog limit (700000 tokens); refresh models first"
    );
}
