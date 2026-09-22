use emp_codex::management_views::{catalog_families, model_views, subscription_model_options};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

#[test]
fn management_views_replay_python_ui_and_subscription_tests() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let directory = tempfile::tempdir().expect("isolated Codex home");
    let script = r#"
import copy, importlib, io, json, sys, unittest
from easy_multi_provider import catalog, management_views as views
original_view = views.management_config
original_options = catalog.subscription_model_options
view_cases, option_cases = [], []
def observed_view(config, native_account=None):
    result = original_view(config, native_account)
    view_cases.append(copy.deepcopy({
        'config':config, 'native':catalog.load_native_catalog(config),
        'accounts':{a['id']:catalog._account_catalog(config,a) for a in config.get('accounts',[])},
        'duplicates':catalog.duplicate_account_status(config.get('accounts',[])),
        'expected':{key:result[key] for key in ('subscription_models','catalog_models','catalog_families')}
    }))
    return result
def observed_options(config, account=None):
    result = original_options(config, account)
    source = catalog._account_catalog(config, account) if account is not None else catalog.load_native_catalog(config)
    option_cases.append(copy.deepcopy({'catalog':source, 'expected':result}))
    return result
views.management_config = observed_view
catalog.subscription_model_options = observed_options
module = importlib.import_module('tests.test_server')
subscription = importlib.import_module('tests.test_subscription_context')
suite = unittest.TestSuite([
    module.ServerAccountTests('test_management_config_exposes_only_safe_catalog_display_rows'),
    module.ServerAccountTests('test_management_config_groups_subscription_route_with_native_family'),
    subscription.SubscriptionContextTests('test_missing_and_invalid_maxima_do_not_invent_a_one_million_limit'),
    subscription.SubscriptionContextTests('test_omitted_effective_percentage_matches_codex_default_and_preserves_explicit_values'),
    subscription.SubscriptionContextTests('test_subscription_model_visibility_follows_catalog_and_user_settings'),
])
stream = io.StringIO()
result = unittest.TextTestRunner(stream=stream).run(suite)
if not result.wasSuccessful():
    sys.stderr.write(stream.getvalue())
    raise SystemExit(1)
json.dump({'views':view_cases, 'options':option_cases},sys.stdout,ensure_ascii=False)
"#;
    let output = Command::new(python)
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .env("CODEX_HOME", directory.path())
        .output()
        .expect("live Python management tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Value = serde_json::from_slice(&output.stdout).expect("oracle fixtures");
    let views = cases["views"].as_array().expect("view cases");
    assert_eq!(views.len(), 2);
    for case in views {
        let accounts: BTreeMap<String, Value> =
            serde_json::from_value(case["accounts"].clone()).expect("account sources");
        let duplicates: BTreeMap<String, String> =
            serde_json::from_value(case["duplicates"].clone()).expect("duplicate labels");
        assert_eq!(
            model_views(&case["config"], &case["native"], &accounts, &duplicates),
            case["expected"]
        );
    }
    let options = cases["options"].as_array().expect("option cases");
    assert!(options.len() >= 5);
    for case in options {
        assert_eq!(
            json!(subscription_model_options(&case["catalog"])),
            case["expected"]
        );
    }
}

#[test]
fn family_controls_prefer_native_limits_and_intersect_summary_support() {
    let models = json!([
        {"id":"provider/model","family_id":"model","default_display_name":"External","context_window":128000,"source_type":"provider","source_id":"provider","supports_reasoning_summaries":false},
        {"id":"account/model","family_id":"model","default_display_name":"Account","context_window":256000,"source_type":"account","source_id":"account","supports_reasoning_summaries":true},
        {"id":"model","family_id":"model","default_display_name":"Native","context_window":512000,"source_type":"native","source_id":"","supports_reasoning_summaries":true},
    ]);
    let config = json!({"catalog_family_presentations":{"model":{"catalog_alias":"Daily","show_context":false,"reasoning_summary":"show"}}});
    let actual = catalog_families(&config, models.as_array().expect("models"));
    assert_eq!(actual[0]["default_display_name"], "Native");
    assert_eq!(actual[0]["context_window"], 512000);
    assert_eq!(actual[0]["supports_reasoning_summaries"], false);
    assert_eq!(actual[0]["display_name"], "Daily");
    if let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") {
        let mut child = Command::new(python)
            .args([
                "-c",
                r#"
import json,sys
from easy_multi_provider.management_views import management_catalog_families
config,models = json.load(sys.stdin)
json.dump(management_catalog_families(config,models),sys.stdout)
"#,
            ])
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("Python family oracle");
        serde_json::to_writer(child.stdin.take().expect("stdin"), &json!([config, models]))
            .expect("fixture");
        let output = child.wait_with_output().expect("oracle output");
        assert!(output.status.success());
        let expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
        assert_eq!(json!(actual), expected);
    }
}
