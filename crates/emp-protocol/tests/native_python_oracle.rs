use emp_protocol::{native_responses, portable_responses};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

#[test]
fn native_history_matches_live_python_and_existing_regressions() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        eprintln!("EMP_PYTHON_INTEROP is unset; native history live oracle skipped");
        return;
    };
    let script = r#"
import base64, copy, io, json, sys, unittest
from unittest.mock import patch
from easy_multi_provider.dialects import project_request, ProjectionError, classify_dialect
from tests import test_history_regressions_v098 as history, test_v06_dialects as dialects
records = []
native = {'protocol': 'responses', 'auth_mode': 'forward'}
portable = {'protocol': 'responses', 'auth_mode': 'api_key'}
def capture(provider, body, *args, **kwargs):
    original = copy.deepcopy(body)
    try:
        value = project_request(provider, body, *args, **kwargs)
        result = {'ok': True, 'value': value}
    except ProjectionError as exc:
        result = {'ok': False, 'type': type(exc).__name__, 'message': str(exc),
                  'index': exc.index, 'item_type': exc.item_type,
                  'part_types': exc.part_types, 'failure_class': exc.failure_class}
        raise
    except TypeError:
        # The server exposes the generic 500 boundary, not interpreter-version
        # specific TypeError wording. Preserve the exception type as well.
        result = {'ok': False, 'type': 'TypeError', 'status': 500}
        raise
    finally:
        assert body == original, 'Python mutated input'
        records.append({'native': classify_dialect(provider) == 'codex_native',
                        'provider': provider, 'body': original, 'result': result})
    return value
suite = unittest.TestSuite([
    unittest.defaultTestLoader.loadTestsFromTestCase(history.NativeToolHistoryRegressionTests),
    dialects.ResponsesDialectTests('test_native_projection_drops_plaintext_reasoning_but_keeps_final_output'),
    dialects.ResponsesDialectTests('test_native_projection_decodes_emp_compaction_and_preserves_surrounding_history'),
    dialects.ResponsesDialectTests('test_encrypted_agent_task_is_not_silently_discarded'),
])
with patch.object(history, 'project_request', capture), patch.object(dialects, 'project_request', capture):
    run = unittest.TextTestRunner(stream=io.StringIO()).run(suite)
    assert run.wasSuccessful(), (run.errors, run.failures)

def add(provider, body):
    try: capture(provider, body)
    except (ProjectionError, TypeError): pass

for value in (None, 'hello', 7, False, {'future': [1]}, [], [None, 7, 'unknown', {'type': 'future', 'opaque': 'keep'}]):
    add(native, {'input': value, 'previous_response_id': 'resp_native', 'future': [1], 'stream': 'untouched'})
add(native, {'model': 'native/model'})
for kind in ([], {}, None, False, 0):
    add(native, {'input': [{'type': kind}]})
for encrypted in (None, '', False, 1, [], {}, 'opaque'):
    add(native, {'input': [{'type': 'reasoning', 'encrypted_content': encrypted, 'summary': [],
        'content': ['private'], 'text': 'private', 'thinking': 'private', 'reasoning_text': 'private',
        'future': 42}]})
for kind, prefix in (('function_call', 'fc'), ('custom_tool_call', 'ctc')):
    for identifier in (None, 0, {}, [], '', 'foreign', prefix, prefix + '_native'):
        call = {'type': kind, 'id': identifier, 'call_id': 'pair', 'arguments': '{}', 'input': 'tool input'}
        output = {'type': kind + '_output', 'id': 'output_id', 'call_id': 'pair', 'output': 'result'}
        for sequence in ([call, output], [call], [output, call], [call, output, output]):
            add(native, {'input': sequence})
    for call_id in (None, '', 0, [], {}):
        add(native, {'input': [{'type': kind, 'id': 'foreign', 'call_id': call_id}]})

# emp1: marks an EMP-owned portable summary, not opaque encrypted task data.
# Shared decoder cases cover UTF-8, mixed URL-safe/standard alphabets, and unused bits.
# Excess padding is Python-version-dependent (3.11 accepts Zm9v=, 3.14 rejects it)
# and remains invalid in Rust; test that safety rule separately below.
encodings = ['', 'Zg', 'Zg=', 'Zg==', 'Zh==', 'Zm9=', 'Zm9v',
             '====', 'abc!','/w==', 'Zg==\n', '中文']
for summary in ('summary', '\uFFFF\uFFFF', '\U0010FFFF', 'constraint\n工具🧪'):
    encoded = base64.b64encode(summary.encode()).decode()
    encodings.extend([encoded, encoded.replace('+', '-'), encoded.replace('/', '_')])
for encoded in encodings:
    for provider in (native, portable):
        add(provider, {'input': [{'type': 'compaction', 'encrypted_content': 'emp1:' + encoded}]})
for opaque in (None, '', 3, 'native-opaque'):
    add(native, {'input': [{'type': 'compaction', 'encrypted_content': opaque}]})
json.dump(records, sys.stdout, ensure_ascii=True)
"#;
    let output = Command::new(python)
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("run Python native projection oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<Value> = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
    assert!(
        records.len() > 150,
        "existing history regressions must execute"
    );
    for (index, record) in records.iter().enumerate() {
        let error_value = |error: &portable_responses::PortableProjectionError| {
            json!({"ok": false, "type": "ProjectionError", "message": error.to_string(),
                "index": error.index(), "item_type": error.item_type(), "part_types": error.part_types(),
                "failure_class": error.failure_class()})
        };
        let actual = if record["native"] == true {
            match native_responses::project_request(record["body"].as_object().unwrap()) {
                Ok(value) => json!({"ok": true, "value": value}),
                Err(native_responses::NativeProjectionError::Projection(error)) => {
                    error_value(&error)
                }
                Err(native_responses::NativeProjectionError::UnhashableItemType) => {
                    json!({"ok": false, "type": "TypeError", "status": 500})
                }
            }
        } else {
            match portable_responses::project_request(
                record["provider"].as_object().unwrap(),
                &record["body"],
                false,
            ) {
                Ok(value) => json!({"ok": true, "value": value}),
                Err(error) => error_value(&error),
            }
        };
        assert_eq!(
            actual, record["result"],
            "native history fixture {index}: {record}"
        );
    }
}

#[test]
fn encrypted_tasks_and_malformed_compactions_remain_opaque() {
    let portable = json!({"protocol": "responses", "auth_mode": "api_key"});
    let task = json!({"input": [{"type": "agent_message", "content": [
        {"type": "encrypted_content", "encrypted_content": "private-task-ciphertext"}
    ]}]});
    let native_result = native_responses::project_request(task.as_object().unwrap()).unwrap();
    assert_eq!(native_result["input"], task["input"]);
    let portable_error =
        portable_responses::project_request(portable.as_object().unwrap(), &task, false)
            .unwrap_err();
    assert_eq!(
        portable_error.failure_class(),
        "encrypted_agent_task_requires_plaintext"
    );

    for malformed in ["emp1:Zg===", "emp1:Zm9v=", "emp1:Zm9v===="] {
        let body = json!({"input": [{"type": "compaction", "encrypted_content": malformed}]});
        let native_error =
            native_responses::project_request(body.as_object().unwrap()).unwrap_err();
        assert!(matches!(
            native_error,
            native_responses::NativeProjectionError::Projection(error)
                if error.failure_class() == "invalid_compaction"
        ));
        let portable_error =
            portable_responses::project_request(portable.as_object().unwrap(), &body, false)
                .unwrap_err();
        assert_eq!(portable_error.failure_class(), "invalid_compaction");
    }
}
