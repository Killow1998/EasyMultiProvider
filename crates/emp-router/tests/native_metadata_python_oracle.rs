use emp_router::native_metadata::{
    native_response_headers, rewrite_native_model_event, rewrite_native_model_headers,
};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

#[test]
fn native_metadata_matches_live_python_and_existing_regressions() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        eprintln!("EMP_PYTHON_INTEROP is unset; native metadata live oracle skipped");
        return;
    };
    let script = r#"
import io, json, sys, unittest
from unittest.mock import patch
import easy_multi_provider.router as router
from tests.test_native_model_boundary import NativeModelBoundaryTests

records = []
originals = {name: getattr(router, name) for name in (
    '_rewrite_native_model_headers', '_rewrite_native_model_event', '_native_response_headers')}
def capture(name):
    def wrapped(value, provider=None, **kwargs):
        result = originals[name](value, provider, **kwargs)
        requested, expected = router._native_model_identity(provider, **kwargs)
        source = {'headers': getattr(value, 'headers', {})} if name == '_native_response_headers' else value
        records.append({'operation': name, 'value': source, 'requested': requested or '',
                        'upstream': expected or '', 'result': result})
        return result
    return wrapped

with patch.multiple(router, **{name: capture(name) for name in originals}):
    run = unittest.TextTestRunner(stream=io.StringIO()).run(
        unittest.defaultTestLoader.loadTestsFromTestCase(NativeModelBoundaryTests))
    assert run.wasSuccessful(), (run.errors, run.failures)
    provider = {'_emp_requested_model': 'native/model', '_emp_expected_upstream_model': 'MODEL'}
    for value in (None, [], 3, 'headers', {}, {'headers': None}, {'headers': []},
                  {'headers': 7, 'response': {'headers': []}},
                  {'response': ['headers']}, {'response': {'headers': 'unchanged'}},
                  {'headers': {'OpenAI-Model': 'model'}, 'response': {'model': 'MODEL'}},
                  {'headers': {'X-OpenAI-Model': 'different', 'Other': 7}}):
        router._rewrite_native_model_event(value, provider)
        router._rewrite_native_model_headers(value, provider)
    for selected in ('', 'native/model'):
        for upstream in ('', 'MODEL'):
            router._rewrite_native_model_headers({'openai-model': 'model'},
                requested_model=selected, expected_upstream_model=upstream)
    # Exercise every Unicode casefold/lowercase difference in the oracle runtime.
    for code in range(sys.maxunicode + 1):
        character = chr(code)
        if character.casefold() != character.lower():
            router._rewrite_native_model_headers({'openai-model': character},
                requested_model='alias', expected_upstream_model=character.casefold())
    headers = {name.upper(): 'value' for name in router._NATIVE_RESPONSE_HEADER_NAMES}
    headers.update({'Authorization': 'fixture-secret', 'Set-Cookie': 'fixture-cookie',
                    'Content-Type': 'application/json', 'openai-model': 'model',
                    'x-request-id': 1})
    for provider_id in ('', '-', '--', '-a', 'a-', 'a--b', 'a1', 'A1', 'é', '_', 'a.b'):
        for side in ('primary', 'secondary', 'tertiary'):
            for metric in ('used-percent', 'window-minutes', 'reset-at', 'remaining'):
                headers[f'x-{provider_id}-{side}-{metric}'] = '3'
        headers[f'x-{provider_id}-limit-name'] = 'fixture'
    router._native_response_headers(type('Response', (), {'headers': headers})(), provider)
    for headers in (None, [], 7):
        router._native_response_headers(type('Response', (), {'headers': headers})(), provider)

json.dump(records, sys.stdout, ensure_ascii=True)
"#;
    let output = Command::new(python)
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("run Python native metadata oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<Value> = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
    assert!(
        records.len() > 300,
        "existing tests and Unicode fixtures must execute"
    );
    for (index, record) in records.iter().enumerate() {
        let value = &record["value"];
        let requested = record["requested"].as_str().unwrap();
        let upstream = record["upstream"].as_str().unwrap();
        let actual = match record["operation"].as_str().unwrap() {
            "_native_response_headers" => {
                Value::Object(native_response_headers(value, requested, upstream))
            }
            "_rewrite_native_model_headers" => {
                Value::Object(rewrite_native_model_headers(value, requested, upstream))
            }
            "_rewrite_native_model_event" => rewrite_native_model_event(value, requested, upstream),
            _ => panic!("unknown oracle operation"),
        };
        assert_eq!(
            actual, record["result"],
            "native metadata fixture {index}: {record}"
        );
    }
}
