use emp_router::native_request::{NativeAuth, request_headers};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

#[test]
fn native_credentials_and_context_headers_match_python() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        eprintln!("EMP_PYTHON_INTEROP is unset; native header live oracle skipped");
        return;
    };
    let script = r#"
import ast, inspect, json, sys
from unittest.mock import patch
import easy_multi_provider
import easy_multi_provider.router as router
from easy_multi_provider.accounts import AccountError

# The archived Python oracle stays at its release version; the differential
# contract compares header logic, so pin its identity to the Rust build.
router.__version__ = "{version}"

# Exercise every current Python allowlisted context header, including additions.
function = ast.parse(inspect.getsource(router._headers))
context = next(ast.literal_eval(node.value) for node in ast.walk(function)
               if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name)
               and target.id == 'native_headers' for target in node.targets))
incoming = {name.upper(): 'fixture-' + name for name in context}
incoming.update({'AUTHORIZATION': 'Bearer caller', 'CHATGPT-ACCOUNT-ID': 'caller-owner',
    'Cookie': 'private-cookie', 'Proxy-Authorization': 'private-proxy',
    'User-Agent': 'untrusted-agent', 'Accept': 'application/untrusted',
    'Content-Type': 'application/untrusted', 'X-EMP-Request-ID': '0123456789abcdef'})
credentials = {'Authorization': 'Bearer selected', 'chatgpt-account-id': 'selected-owner'}
records = []
for mode in ('forward', 'account', 'implicit', 'implicit_missing', 'implicit_empty'):
    for stream in (True, False):
        for headers in (incoming, {}, {'authorization': ''},
                        dict(incoming, **{'THREAD-ID': '', 'X-EMP-Request-ID': 'ABCDEF0123456789'}),
                        {'authorization': 'Bearer caller', 'x-emp-request-id': '0123456789abcdef'},
                        {'authorization': 'Bearer caller', 'X-EMP-Request-ID': '0123456789abcde'}):
            provider = {'id': 'native', 'protocol': 'responses', 'auth_mode': 'account' if mode == 'account' else 'forward'}
            if mode == 'account': provider['account'] = {'id': 'fixture-account'}
            if mode.startswith('implicit'): provider['implicit_native'] = True
            selected = {} if mode == 'implicit_empty' else credentials
            with patch.object(router, 'auth_headers', return_value=selected), \
                 patch.object(router, 'native_auth_headers', return_value=selected,
                    side_effect=AccountError('fixture unreadable login') if mode == 'implicit_missing' else None):
                try:
                    value = router._headers(provider, dict(sorted(headers.items())), stream,
                        resolved_native_auth={} if mode == 'implicit_empty' else None)
                    result = {'ok': True, 'headers': value}
                except router.RouterError as exc:
                    result = {'ok': False, 'type': type(exc).__name__, 'status': exc.status, 'message': str(exc)}
            records.append({'mode': mode, 'stream': stream, 'incoming': headers,
                            'selected': selected, 'result': result})
json.dump(records, sys.stdout)
"#;
    let script = script.replace("{version}", env!("CARGO_PKG_VERSION"));
    let output = Command::new(python)
        .args(["-c", script.as_str()])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("run Python native headers oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<Value> = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    assert_eq!(records.len(), 60);
    for (index, record) in records.iter().enumerate() {
        let selected: BTreeMap<String, String> =
            serde_json::from_value(record["selected"].clone()).unwrap();
        let incoming = serde_json::from_value(record["incoming"].clone()).unwrap();
        let auth = match record["mode"].as_str().unwrap() {
            "forward" => NativeAuth::Forward,
            "account" => NativeAuth::Account(&selected),
            "implicit" | "implicit_empty" => NativeAuth::Implicit(Some(&selected)),
            "implicit_missing" => NativeAuth::Implicit(None),
            _ => panic!("unknown fixture auth mode"),
        };
        let actual = match request_headers(auth, &incoming, record["stream"].as_bool().unwrap()) {
            Ok(headers) => json!({"ok": true, "headers": headers}),
            Err(error) => {
                json!({"ok": false, "type": "RouterError", "status": error.status(), "message": error.to_string()})
            }
        };
        assert_eq!(actual, record["result"], "native headers case {index}");
    }
}
