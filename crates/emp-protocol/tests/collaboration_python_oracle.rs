use emp_protocol::collaboration::{
    CollaborationError, collaboration_summary, prepare_collaboration, restore_collaboration,
};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

fn error_value(error: CollaborationError) -> Value {
    let mut result = json!({"ok": false, "type": error.python_type(), "status": error.status(),
        "failure_reason": error.failure_reason()});
    if matches!(
        error,
        CollaborationError::NamespaceCollision | CollaborationError::UnexpectedEncryptedArguments
    ) {
        result["message"] = error.to_string().into();
    }
    result
}

#[test]
fn collaboration_matches_python_regressions_and_deterministic_schema_ids() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        eprintln!("EMP_PYTHON_INTEROP is unset; collaboration live oracle skipped");
        return;
    };
    let script = r#"
import copy, io, json, sys, unittest
from unittest.mock import patch
import easy_multi_provider.collaboration_transport as collaboration
from tests import test_collaboration_transport as regression
records = []
originals = {name: getattr(collaboration, name) for name in
             ('prepare_collaboration', 'restore_collaboration', 'collaboration_summary')}
def capture(name):
    def wrapped(value):
        original = copy.deepcopy(value)
        try:
            output = originals[name](value)
            result = {'ok': True, 'value': output}
        except Exception as exc:
            result = {'ok': False, 'type': type(exc).__name__, 'status': getattr(exc, 'status', 400 if isinstance(exc, ValueError) else 500),
                      'failure_reason': getattr(exc, 'failure_reason', None)}
            if type(exc).__name__ in ('CollaborationNamespaceCollision', 'ValueError'):
                result['message'] = str(exc)
            raise
        finally:
            assert value == original, 'Python mutated caller input'
            records.append({'operation': name, 'input': original, 'result': result})
        return output
    return wrapped
with patch.multiple(regression, **{name: capture(name) for name in originals}):
    run = unittest.TextTestRunner(stream=io.StringIO()).run(
        unittest.defaultTestLoader.loadTestsFromTestCase(regression.CollaborationTransportTests))
    assert run.wasSuccessful(), (run.errors, run.failures)

def add(name, value):
    try: return capture(name)(value)
    except Exception: pass

def namespace(name='collaboration'):
    return {'type': 'namespace', 'name': name, 'tools': [
        {'type': 'function', 'name': child, 'parameters': {'properties': {
            'message': {'type': 'string', 'encrypted': True, 'description': '約束🧪\u007f'},
            'threshold': {'default': 1e-7, 'maximum': 1e16}, 'other': {'encrypted': True}}}}
        for child in ('spawn_agent', 'send_message', 'followup_task', 'wait_agent')]}

for identifier in ('at_original', '附加工具🧪', None, True, False, 0, 42, 1e-7, 1e16):
    body = {'input': [{'type': 'additional_tools', 'id': identifier,
                      'tools': [namespace()]}], 'tools': None, 'instructions': 'keep'}
    projected = add('prepare_collaboration', body)
    if projected:
        add('collaboration_summary', body)
        add('collaboration_summary', projected[0])
        add('prepare_collaboration', projected[0]) # reserved-namespace failure, not idempotence
for marker in (None, [], ['message'], False, 0, '', {}, [None]):
    item = {'type': 'function_call', 'namespace': 'collaboration',
            'name': 'spawn_agent', 'encrypted_function_args': marker, 'arguments': 'opaque-or-text'}
    add('prepare_collaboration', {'tools': [namespace()], 'input': [item]})
    add('prepare_collaboration', {'tools': [], 'input': [item]})
    output = dict(item, namespace='emp_collaboration')
    for event in (output, {'type': 'response.output_item.added', 'item': output},
                  {'type': 'response.output_item.done', 'item': output},
                  {'type': 'response.completed', 'response': {'output': [output]}},
                  {'type': 'response.failed', 'response': {'output': [output]}},
                  {'type': 'response.incomplete', 'response': {'output': [output]}},
                  {'type': 'future', 'output': [output]}):
        add('restore_collaboration', event)
for value in (None, [], 3, 'text', {}, {'type': 'response.output_text.delta', 'delta': 'emp_collaboration'},
              {'type': [], 'output': []}, {'type': {}, 'output': []}):
    add('restore_collaboration', value)
for tools in (None, [], {}, '', 0, False, 1, True, {'key': 'value'}, 'text', [None, 1, 'tool'], [namespace('emp_collaboration')]):
    for body in ({'tools': tools}, {'input': [{'type': 'additional_tools', 'tools': tools}]},
                 {'tools': [namespace('emp_collaboration')], 'input': [{'type': 'additional_tools', 'tools': tools}]}):
        add('prepare_collaboration', body)
        add('collaboration_summary', body)
for children in (None, {}, [], '', 'child', 0, False, 1, [None], [{'name': []}], [{'name': {}}]):
    tool = namespace(); tool['tools'] = children
    add('prepare_collaboration', {'tools': [tool]})
for parameters in (None, [], 0, '', {'properties': None}, {'properties': []}, {'properties': {'message': 'text'}}):
    tool = namespace(); tool['tools'][0]['parameters'] = parameters
    add('prepare_collaboration', {'tools': [tool]})
for value in (None, [], {}, '', 0, False, 1, True, 'output', [None, 1, 'item']):
    add('restore_collaboration', {'output': value})
    add('restore_collaboration', {'type': 'response.completed', 'response': {'output': value}})
for name in ('spawn_agent', 'send_message', 'followup_task', 'wait_agent', [], {}, None):
    add('restore_collaboration', {'type': 'function_call', 'namespace': 'emp_collaboration', 'name': name})
json.dump(records, sys.stdout, ensure_ascii=True)
"#;
    let output = Command::new(python)
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("run Python collaboration oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<Value> = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    assert!(
        records.len() > 200,
        "regression and schema fixtures must execute"
    );
    for (index, record) in records.iter().enumerate() {
        let value = &record["input"];
        let original = value.clone();
        let actual = match record["operation"].as_str().unwrap() {
            "prepare_collaboration" => match prepare_collaboration(value.as_object().unwrap()) {
                Ok((body, changed)) => json!({"ok": true, "value": [body, changed]}),
                Err(error) => error_value(error),
            },
            "restore_collaboration" => match restore_collaboration(value) {
                Ok(value) => json!({"ok": true, "value": value}),
                Err(error) => error_value(error),
            },
            "collaboration_summary" => {
                json!({"ok": true, "value": collaboration_summary(value.as_object().unwrap())})
            }
            _ => panic!("unknown oracle operation"),
        };
        assert_eq!(*value, original, "Rust input mutation in case {index}");
        assert_eq!(
            actual, record["result"],
            "collaboration fixture {index}: {record}"
        );
    }
}

#[test]
fn encrypted_tasks_are_preserved_and_never_relabelled() {
    let encrypted = json!({"type": "function_call", "namespace": "collaboration",
        "name": "spawn_agent", "encrypted_function_args": ["message"], "arguments": "opaque"});
    assert_eq!(restore_collaboration(&encrypted).unwrap(), encrypted);
    let mut invalid_plaintext = encrypted;
    invalid_plaintext["namespace"] = "emp_collaboration".into();
    assert_eq!(
        restore_collaboration(&invalid_plaintext),
        Err(CollaborationError::UnexpectedEncryptedArguments)
    );
}
