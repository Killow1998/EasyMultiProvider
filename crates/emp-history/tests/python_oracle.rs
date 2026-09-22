use emp_history::{
    HistoryAnchor, HistoryError, HistoryReader, HistorySnapshot, VisibleItem, prepare,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

struct Reader(Vec<VisibleItem>);

impl HistoryReader for Reader {
    fn read_compaction_history(
        &self,
        anchor: &HistoryAnchor,
        _: &Map<String, Value>,
    ) -> Result<HistorySnapshot, HistoryError> {
        Ok(HistorySnapshot {
            thread_id: anchor.thread_id.clone().expect("fixture thread"),
            items: self.0.clone(),
            source_model: Some("gpt-native".to_owned()),
        })
    }
}

fn visible(value: &Value) -> Vec<VisibleItem> {
    value
        .as_array()
        .expect("visible fixtures")
        .iter()
        .map(|item| VisibleItem {
            kind: item["kind"].as_str().unwrap().to_owned(),
            content: item.get("content").cloned().unwrap_or(Value::Null),
            item_id: item
                .get("item_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            turn_id: item
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            call_id: item
                .get("call_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            raw_type: item
                .get("raw_type")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .collect()
}

#[test]
fn continuity_matches_live_python_for_switch_and_tool_boundaries() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let cases = json!([
        {
            "body":{"model":"external/model","input":[{"type":"compaction","encrypted_content":"emp1:U3VtbWFyeSB0ZXh0Lg=="}]},
            "headers":{},"native":false,"visible":[]
        },
        {
            "body":{"model":"native/model","input":[{"type":"compaction","encrypted_content":"opaque"}]},
            "headers":{},"native":true,"visible":[]
        },
        {
            "body":{"model":"external/model","input":[{"type":"compaction","encrypted_content":"opaque"},{"type":"message","role":"user","content":"active"}]},
            "headers":{"thread-id":"thread","x-codex-turn-metadata":"{\"thread_id\":\"thread\",\"turn_id\":\"turn\"}"},
            "native":false,
            "visible":[
                {"kind":"user_message","content":"constraint"},
                {"kind":"tool_call","content":{"name":"read","arguments":"{}"},"call_id":"call-1","raw_type":"custom_tool_call"},
                {"kind":"compaction_marker","content":""},
                {"kind":"assistant_message","content":"duplicate active"}
            ]
        },
        {
            "body":{"model":"external/model","stream":true,"input":[{"type":"compaction","encrypted_content":"opaque"},{"type":"message","role":"user","content":"active"},{"type":"compaction_trigger"}]},
            "headers":{"x-codex-turn-metadata":"{\"thread_id\":\"thread\",\"turn_id\":\"turn\",\"window_id\":\"new\"}","x-codex-window-id":"stale"},
            "native":false,
            "visible":[{"kind":"compaction_summary","content":"checkpoint"}]
        },
        {
            "body":{"model":"external/model","input":[{"type":"compaction","encrypted_content":"opaque"}],"client_metadata":{"x-codex-turn-metadata":"{\"thread_id\":\"thread\",\"turn_id\":\"turn\"}"}},
            "headers":{"thread-id":"different"},"native":false,
            "visible":[{"kind":"compaction_summary","content":"checkpoint"}]
        }
    ]);
    let script = r#"
import json, sys
from easy_multi_provider.codex_history import HistorySnapshot, HistoryAnchor, HistoryCursor, VisibleItem
from easy_multi_provider.history_continuity import HistoryContinuityEngine

class Reader:
    def __init__(self, items): self.items = items
    def read_compaction_history(self, anchor, compaction):
        return HistorySnapshot(anchor=anchor, items=tuple(self.items),
            cursor=HistoryCursor(thread_id=anchor.thread_id), source='fixture', source_model='gpt-native')

out=[]
for case in json.load(sys.stdin):
    items=[VisibleItem(**item) for item in case['visible']]
    provider={'protocol':'responses','auth_mode':'forward' if case['native'] else 'api_key'}
    try:
        value=HistoryContinuityEngine(Reader(items)).prepare({}, provider, {}, case['body']['model'], case['body'], case['headers'])
        out.append({'ok':value})
    except Exception as error:
        out.append({'error':getattr(error,'reason',type(error).__name__)})
print(json.dumps(out, ensure_ascii=False, sort_keys=True))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn Python history oracle");
    serde_json::to_writer(child.stdin.take().unwrap(), &cases).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rust = cases
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            let headers = case["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap().to_owned()))
                .collect::<BTreeMap<_, _>>();
            match prepare(
                &case["body"],
                &headers,
                case["native"].as_bool().unwrap(),
                &Reader(visible(&case["visible"])),
            ) {
                Ok(value) => json!({"ok":value}),
                Err(error) => json!({"error":error.reason()}),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), python);
}

#[test]
fn context_estimates_and_decisions_match_live_python() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let cases = json!([
        {
            "provider":{"id":"demo","base_url":"https://example.com/v1","context_window":4096,"capability_sources":{"context_window":{"source":"manual","confidence":1.0}}},
            "model":{"id":"demo/model","upstream_id":"model","output_limit":512},
            "protocol":"responses",
            "payload":{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}],"tools":[]}
        },
        {
            "provider":{"id":"demo","base_url":"https://example.com/v1"},
            "model":{"id":"demo/model","upstream_id":"model","context_window":1200,"output_limit":64,"capability_sources":{"context_window":{"source":"official","confidence":0.95}}},
            "protocol":"chat_completions",
            "payload":{"messages":[{"role":"user","content":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}],"tools":[]}
        },
        {
            "provider":{"id":"demo","base_url":"https://example.com/v1"},
            "model":{"id":"demo/model","upstream_id":"model","context_window":10000,"output_limit":256,"capability_sources":{"context_window":{"source":"advertised","confidence":0.75}}},
            "protocol":"anthropic_messages",
            "payload":{"system":"system","messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"AAAAAAAAAAAAAAAA"}},{"type":"text","text":"look"}]}],"tools":[]}
        }
    ]);
    let script = r#"
import json, sys
from easy_multi_provider.context_guard import assess_context
keys=('input_estimate','output_reserve','context_limit','safe_input_limit','confidence','source','decision')
out=[]
for case in json.load(sys.stdin):
    value=assess_context(case['provider'],case['model'],case['protocol'],case['payload']).to_safe_dict()
    out.append({key:value[key] for key in keys})
print(json.dumps(out, sort_keys=True))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.take().unwrap(), &cases).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
    let actual = cases
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            let assessment = emp_history::context::assess(
                case["provider"].as_object().unwrap(),
                case["model"].as_object().unwrap(),
                case["protocol"].as_str().unwrap(),
                &case["payload"],
            );
            json!({
                "input_estimate":assessment.input_estimate,
                "output_reserve":assessment.output_reserve,
                "context_limit":assessment.context_limit,
                "safe_input_limit":assessment.safe_input_limit,
                "confidence":assessment.confidence,
                "source":assessment.source,
                "decision":assessment.decision,
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(actual), expected);
}

#[test]
fn destination_compaction_matches_live_python_for_switch_fixture() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let body = json!({
        "model":"external/model","max_output_tokens":64,
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"x".repeat(500)}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"y".repeat(500)}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"z".repeat(500)}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"w".repeat(500)}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"active request"}]}
        ]
    });
    let script = r#"
import json, sys
from easy_multi_provider.context_guard import ContextAssessment, context_identity
from easy_multi_provider.destination_context import DestinationContextCompactor
case=json.load(sys.stdin)
provider={'id':'external','protocol':'chat_completions','base_url':'https://example.com/v1'}
model={'id':'external/model','upstream_id':'model','max_output_tokens':64}
assessment=ContextAssessment(context_identity(provider,model,'chat_completions'),
    'external','external/model','fixture',2000,64,256,320,1200,880,1.0,'manual','high','block','compact','blocked')
result=DestinationContextCompactor(lambda request: 'checkpoint').compact(
    provider,model,'external/model',case,assessment)
print(json.dumps(result,ensure_ascii=False,sort_keys=True,separators=(',',':')))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.take().unwrap(), &body).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
    let model = json!({"id":"external/model","upstream_id":"model","max_output_tokens":64});
    let actual = emp_history::context::compact_with(&body, model.as_object().unwrap(), 880, |_| {
        Ok("checkpoint".to_owned())
    })
    .unwrap();
    assert_eq!(actual, expected);
}
