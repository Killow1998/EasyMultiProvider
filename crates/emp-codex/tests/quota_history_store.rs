use emp_codex::quota_history::QuotaHistoryStore;
use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
fn failed_observations_explain_gaps_without_creating_quota_points() {
    let root = TempDir::new().unwrap();
    let store = QuotaHistoryStore::new(root.path().join("history.sqlite3"));
    let error = emp_codex::quota::quota_rpc_error(
        "account/rateLimits/read",
        &json!({"message":"error sending request: dns error https://private.invalid/?token=PRIVATE"}),
    );
    assert_eq!(error.diagnostics()["rpc_method"], "account/rateLimits/read");
    assert_eq!(error.diagnostics()["transport_cause"], "dns");
    for at in [1000, 1044, 1088] {
        store.append_failure("account", &error, at).unwrap();
    }
    let history = store.query_period("account", 900, 1200).unwrap();
    assert_eq!(history["series"], json!([]));
    assert_eq!(history["failures"][0]["count"], 3);
    assert_eq!(
        history["failures"][0]["diagnostics"]["code"],
        "quota_transport_error"
    );
    assert!(!history.to_string().contains("PRIVATE"));
    assert!(!history.to_string().contains("private.invalid"));
    assert_eq!(
        store.query_period("other", 900, 1200).unwrap()["failures"],
        json!([])
    );
    store.adopt_legacy_key("account", "owner").unwrap();
    assert_eq!(
        store.query_period("owner", 900, 1200).unwrap()["failures"][0]["count"],
        3
    );
    store
        .append_snapshot(
            "owner",
            &json!({"rate_limits":{"primary":{"usedPercent":10,"windowDurationMins":300}}}),
            1088,
        )
        .unwrap();
    let recovered = store.query_period("owner", 900, 1200).unwrap();
    assert_eq!(recovered["failures"][0]["count"], 2);
    assert_eq!(
        recovered["series"][0]["points"].as_array().unwrap().len(),
        1
    );
    store.delete_account("owner").unwrap();
    assert_eq!(
        store.query_period("owner", 900, 1200).unwrap()["failures"],
        json!([])
    );
}

fn fixture() -> Value {
    json!({
        "account": "ship",
        "now": 2_000_700,
        "snapshots": [
            {
                "observed_at": 2_000_100,
                "quota": {
                    "plan_type": "plus",
                    "rate_limits": {
                        "limitId": "codex",
                        "primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": 2_100_000},
                        "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 3_000_000}
                    }
                }
            },
            {
                "observed_at": 2_000_400,
                "quota": {
                    "planType": "ProLite",
                    "rate_limits": {
                        "limit_id": "codex",
                        "primary": {"used_percent": 50, "window_minutes": 10080, "resets_at": 3_000_000},
                        "secondary": {"used_percent": 20, "window_duration_mins": 300, "resets_at": 2_100_000}
                    }
                }
            },
            {
                "observed_at": 2_000_700,
                "quota": {
                    "plan_type": "pro",
                    "rate_limits_by_limit_id": {
                        "codex": {
                            "primary": {"usedPercent": 12.345, "windowDurationMins": 300, "resetsAt": null},
                            "secondary": {"usedPercent": 101, "windowDurationMins": 10080, "resetsAt": 3_000_000}
                        },
                        "codex-secondary": {
                            "primary": {"usedPercent": -1, "windowDurationMins": 300}
                        }
                    }
                }
            }
        ]
    })
}

fn run_rust(path: &std::path::Path, fixture: &Value) -> Value {
    let store = QuotaHistoryStore::new(path);
    let counts = fixture["snapshots"]
        .as_array()
        .expect("snapshots")
        .iter()
        .map(|item| {
            store
                .append_snapshot(
                    fixture["account"].as_str().expect("account"),
                    &item["quota"],
                    item["observed_at"].as_i64().expect("observed_at"),
                )
                .expect("append snapshot")
        })
        .collect::<Vec<_>>();
    let now = fixture["now"].as_i64().expect("now");
    let queries = ["1h", "1d", "1w", "all"]
        .into_iter()
        .map(|range| {
            (
                range.to_owned(),
                store
                    .query(fixture["account"].as_str().expect("account"), range, now)
                    .expect("query history"),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let invalid = store
        .query(
            fixture["account"].as_str().expect("account"),
            "forever",
            now,
        )
        .expect_err("invalid range")
        .to_string();
    json!({"counts": counts, "queries": queries, "invalid": invalid})
}

#[test]
fn quota_history_has_a_standalone_storage_contract() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("history.sqlite3");
    let fixture = fixture();
    let result = run_rust(&path, &fixture);
    assert_eq!(result["counts"], json!([2, 2, 3]));
    assert_eq!(result["invalid"], "unsupported quota history range");
    let all = &result["queries"]["all"];
    assert_eq!(all["sample_interval_seconds"], 44);
    assert_eq!(all["retention_days"], 15);
    assert_eq!(all["plans"][1]["plan_type"], "pro_lite");
    assert!(
        all["series"]
            .as_array()
            .is_some_and(|series| series.len() == 3)
    );

    let store = QuotaHistoryStore::new(&path);
    store.delete_account("ship").expect("delete account");
    assert_eq!(
        store
            .query("ship", "all", fixture["now"].as_i64().expect("now"))
            .expect("empty history")["series"],
        json!([])
    );
}

#[test]
fn quota_history_migrates_the_legacy_schema_without_plan_type() {
    let directory = TempDir::new().expect("temporary directory");
    let path = directory.path().join("legacy.sqlite3");
    let connection = rusqlite::Connection::open(&path).expect("legacy database");
    connection
        .execute_batch(
            "CREATE TABLE quota_samples (
                account_key TEXT NOT NULL,
                observed_at INTEGER NOT NULL,
                limit_id TEXT NOT NULL,
                window_kind TEXT NOT NULL,
                window_minutes INTEGER,
                used_percent REAL NOT NULL,
                resets_at INTEGER,
                PRIMARY KEY (account_key, observed_at, limit_id, window_kind)
            );",
        )
        .expect("legacy schema");
    drop(connection);
    let store = QuotaHistoryStore::new(&path);
    store
        .append_snapshot("ship", &fixture()["snapshots"][0]["quota"], 2_000_100)
        .expect("append migrated history");
    let connection = rusqlite::Connection::open(path).expect("migrated database");
    let has_plan_type: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('quota_samples') WHERE name='plan_type')",
            [],
            |row| row.get(0),
        )
        .expect("schema check");
    assert!(has_plan_type);
}

#[cfg(unix)]
#[test]
fn quota_history_rejects_a_symlink_database() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new().expect("temporary directory");
    let target = directory.path().join("target.sqlite3");
    std::fs::write(&target, b"private").expect("write target");
    let link = directory.path().join("history.sqlite3");
    symlink(&target, &link).expect("symlink database");
    let store = QuotaHistoryStore::new(link);
    let error = store
        .append_snapshot("ship", &fixture()["snapshots"][0]["quota"], 2_000_100)
        .expect_err("reject symlink");
    let rendered = error.to_string();
    assert!(rendered.contains("must not be a symlink"), "{rendered}");
    assert_eq!(std::fs::read(target).expect("target remains"), b"private");
}
