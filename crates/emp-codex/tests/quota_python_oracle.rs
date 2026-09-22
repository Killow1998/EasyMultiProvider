use emp_codex::quota::{parse_app_server_output_at, quota_rpc_error, reset_outcome};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    json!({
        "observed_at": 1_797_891_234,
        "transcript": [
            "diagnostic output that is not JSON",
            {"id": 1, "result": {}},
            {"id": 2, "result": {
                "account": {"email": "xian@example.com", "planType": "pro"},
                "rateLimits": {
                    "limitId": "codex",
                    "primary": {"usedPercent": 30, "windowDurationMins": 300},
                    "credits": {"hasCredits": true, "balance": 9, "private": "drop"}
                }
            }},
            {"method": "account/rateLimits/updated", "params": {
                "rateLimitsByLimitId": {
                    "codex": {
                        "planType": "fallback-plan",
                        "primary": {"usedPercent": 20, "windowDurationMins": 300, "resetsAt": 123},
                        "secondary": {"usedPercent": 70, "windowDurationMins": 10080, "resetsAt": 456},
                        "credits": {"hasCredits": true, "unlimited": false, "balance": 1818},
                        "individualLimit": {"limit": 100, "used": 4, "remainingPercent": 96, "resetsAt": null, "secret": "drop"},
                        "spendControlReached": false
                    },
                    "other": {"primary": {"usedPercent": 1}}
                },
                "rateLimitResetCredits": {
                    "availableCount": 2,
                    "credits": [{
                        "id": "opaque-reset-id",
                        "resetType": "weekly",
                        "status": "available",
                        "grantedAt": 100,
                        "expiresAt": 200,
                        "title": "Full reset",
                        "description": "Reset quota",
                        "private": "drop"
                    }]
                }
            }}
        ],
        "parse_failures": [
            {"name": "missing", "transcript": "noise only"},
            {"name": "non_object", "transcript": "[]\n"}
        ],
        "rpc_errors": [
            {"method": "account/rateLimits/read", "error": {"message": "codex account authentication required to read rate limits"}},
            {"method": "account/rateLimits/read", "error": {"message": "failed to fetch codex rate limits: GET https://example.invalid failed: 401 Unauthorized; content-type=text/plain; body=private-token"}},
            {"method": "account/rateLimits/read", "error": {"message": "failed to fetch codex rate limits: GET https://example.invalid failed: 403 Forbidden; content-type=text/plain; body=private-token"}},
            {"method": "account/rateLimits/read", "error": {"message": "failed to fetch codex rate limits: GET https://example.invalid failed: 429 Too Many Requests; content-type=text/plain; body=private-token"}},
            {"method": "account/rateLimits/read", "error": {"message": "failed to fetch codex rate limits: private-token"}},
            {"method": "account/rateLimits/read", "error": {"message": "error sending request for url (https://example.invalid/private-token)"}},
            {"method": "account/read", "error": {"message": "private"}},
            {"method": "account/rateLimitResetCredit/consume", "error": {"message": "authentication required"}},
            {"method": "account/rateLimitResetCredit/consume", "error": {"message": "private"}},
            {"method": "initialize", "error": "wrong shape"}
        ],
        "reset_transcripts": [
            "noise\n{\"id\":3,\"result\":{\"outcome\":\"reset\"}}\n",
            "{\"id\":3,\"result\":{\"outcome\":\"nothingToReset\"}}\n",
            "{\"id\":3,\"result\":{\"outcome\":\"noCredit\"}}\n",
            "{\"id\":3,\"result\":{\"outcome\":\"alreadyRedeemed\"}}\n",
            "{\"id\":3,\"result\":{\"outcome\":\"private\"}}\n",
            "[]\n"
        ]
    })
}

fn error(error: &emp_codex::quota::QuotaError) -> Value {
    json!({"message": error.to_string(), "code": error.code()})
}

fn rust_result(fixture: &Value) -> Value {
    let transcript = fixture["transcript"]
        .as_array()
        .expect("transcript")
        .iter()
        .map(|line| {
            line.as_str().map_or_else(
                || serde_json::to_string(line).expect("transcript JSON"),
                str::to_owned,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let parsed = parse_app_server_output_at(
        &transcript,
        fixture["observed_at"].as_u64().expect("observed_at"),
    )
    .expect("quota snapshot");
    let parse_failures = fixture["parse_failures"]
        .as_array()
        .expect("parse failures")
        .iter()
        .map(|case| {
            let failure = parse_app_server_output_at(
                case["transcript"].as_str().expect("failure transcript"),
                fixture["observed_at"].as_u64().expect("observed_at"),
            )
            .expect_err("parse failure");
            error(&failure)
        })
        .collect::<Vec<_>>();
    let rpc_errors = fixture["rpc_errors"]
        .as_array()
        .expect("RPC errors")
        .iter()
        .map(|case| {
            error(&quota_rpc_error(
                case["method"].as_str().expect("method"),
                &case["error"],
            ))
        })
        .collect::<Vec<_>>();
    let reset = fixture["reset_transcripts"]
        .as_array()
        .expect("reset transcripts")
        .iter()
        .map(
            |transcript| match reset_outcome(transcript.as_str().expect("transcript"), 3) {
                Ok(outcome) => json!({"ok": true, "outcome": outcome}),
                Err(failure) => json!({"ok": false, "error": error(&failure)}),
            },
        )
        .collect::<Vec<_>>();
    json!({
        "parsed": parsed,
        "parse_failures": parse_failures,
        "rpc_errors": rpc_errors,
        "reset": reset,
    })
}

#[test]
fn quota_projection_matches_live_python_oracle_when_configured() {
    let fixture = fixture();
    let rust = rust_result(&fixture);
    assert_eq!(rust["parsed"]["account_label"], "x***@example.com");
    assert_eq!(
        rust["parsed"]["credits"]["reset_credits"]["available_count"],
        2
    );
    assert!(!rust["parsed"].to_string().contains("opaque-reset-id"));
    assert!(!rust["rpc_errors"].to_string().contains("private-token"));

    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let script = r#"
import json, sys
from unittest.mock import patch
from easy_multi_provider.quota import QuotaError, _quota_rpc_error, _reset_outcome, parse_app_server_output

fixture = json.load(sys.stdin)
transcript = "\n".join(line if isinstance(line, str) else json.dumps(line, ensure_ascii=False, separators=(",", ":")) for line in fixture["transcript"])

def error(exc):
    return {"message": str(exc), "code": exc.code}

with patch("easy_multi_provider.quota.time.time", return_value=fixture["observed_at"]):
    parsed = parse_app_server_output(transcript)

parse_failures = []
for case in fixture["parse_failures"]:
    try:
        with patch("easy_multi_provider.quota.time.time", return_value=fixture["observed_at"]):
            parse_app_server_output(case["transcript"])
    except QuotaError as exc:
        parse_failures.append(error(exc))

rpc_errors = [error(_quota_rpc_error(case["method"], case["error"])) for case in fixture["rpc_errors"]]
reset = []
for transcript in fixture["reset_transcripts"]:
    try:
        reset.append({"ok": True, "outcome": _reset_outcome(transcript)})
    except QuotaError as exc:
        reset.append({"ok": False, "error": error(exc)})

json.dump({
    "parsed": parsed,
    "parse_failures": parse_failures,
    "rpc_errors": rpc_errors,
    "reset": reset,
}, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python quota oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write fixture");
    let output = child
        .wait_with_output()
        .expect("wait for Python quota oracle");
    assert!(
        output.status.success(),
        "Python quota oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python: Value = serde_json::from_slice(&output.stdout).expect("Python quota output");
    assert_eq!(rust, python);
}
