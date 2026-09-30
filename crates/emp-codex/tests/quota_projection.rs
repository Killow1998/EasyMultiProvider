use emp_codex::quota::{
    parse_app_server_output_at, quota_rpc_error, reset_outcome, validated_reset_idempotency_key,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    json!({
        "observed_at": 1_797_891_234,
        "transcript": [
            "diagnostic output that is not JSON",
            {"id": 1, "result": {}},
            {"id": 2, "result": {
                "account": {"email": "user@example.com", "planType": "pro"},
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
                    "other": {"primary": {"usedPercent": 1, "windowDurationMins": 43200}}
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
        ],
        "idempotency_keys": [
            "123e4567-e89b-12d3-a456-426614174000",
            "123E4567-E89B-12D3-A456-426614174000",
            "123e4567e89b12d3a456426614174000",
            "{123e4567-e89b-12d3-a456-426614174000}",
            "retry-me"
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
    let idempotency_keys = fixture["idempotency_keys"]
        .as_array()
        .expect("idempotency keys")
        .iter()
        .map(|value| {
            match validated_reset_idempotency_key(value.as_str().expect("idempotency key")) {
                Ok(value) => json!({"ok": true, "value": value}),
                Err(failure) => json!({"ok": false, "error": error(&failure)}),
            }
        })
        .collect::<Vec<_>>();
    json!({
        "parsed": parsed,
        "parse_failures": parse_failures,
        "rpc_errors": rpc_errors,
        "reset": reset,
        "idempotency_keys": idempotency_keys,
    })
}

#[test]
fn quota_projection_redacts_private_details_and_parses_app_server_output() {
    let fixture = fixture();
    let rust = rust_result(&fixture);
    assert_eq!(rust["parsed"]["account_label"], "u***@example.com");
    assert_eq!(rust["parsed"]["plan_type"], "free");
    assert_eq!(
        rust["parsed"]["credits"]["reset_credits"]["available_count"],
        2
    );
    assert_eq!(
        rust["parsed"]["credits"]["reset_credits"]["credits"][0]["id"],
        "opaque-reset-id"
    );
    assert!(!rust["rpc_errors"].to_string().contains("private-token"));
}

#[test]
fn workspace_routing_failures_flag_the_imported_refresh_retry() {
    for (method, message, expected_code, expected_retry) in [
        (
            "account/read",
            "workspace routing discovery timed out",
            "quota_transport_error",
            true,
        ),
        (
            "account/read",
            "  WORKSPACE ROUTING DISCOVERY FAILED  ",
            "quota_transport_error",
            true,
        ),
        (
            "account/read",
            "private backend detail",
            "quota_account_read_failed",
            false,
        ),
        (
            "account/rateLimits/read",
            "workspace routing discovery failed",
            "quota_fetch_failed",
            false,
        ),
    ] {
        let error = quota_rpc_error(method, &json!({"message": message}));
        assert_eq!(error.code(), expected_code, "{method}: {message}");
        assert_eq!(
            error.should_retry_imported_refresh(),
            expected_retry,
            "{method}: {message}"
        );
    }
}

#[test]
fn missing_rpc_is_local_to_the_requested_operation() {
    for (method, detail) in [
        ("account/rateLimits/read", "quota reads"),
        ("account/rateLimitResetCredit/consume", "quota reset"),
        ("account/read", "account reads"),
    ] {
        let unavailable =
            quota_rpc_error(method, &json!({"code":-32601,"message":"Method not found"}));
        assert_eq!(unavailable.code(), "quota_operation_unavailable");
        assert!(unavailable.to_string().contains(detail));
        assert_ne!(
            quota_rpc_error(
                method,
                &json!({"code":-32603,"message":"HTTP 401 Unauthorized"})
            )
            .code(),
            "quota_operation_unavailable"
        );
        assert_ne!(
            quota_rpc_error(method, &json!({"code":-32603,"message":"network timeout"})).code(),
            "quota_operation_unavailable"
        );
    }
}
