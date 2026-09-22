use std::io::Write;
use std::process::{Command, Stdio};

use emp_codex::quota_history::QuotaHistoryStore;
use serde_json::{Value, json};
use tempfile::TempDir;

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
                        "codex_bengalfox": {
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
    assert_eq!(all["sample_interval_seconds"], 300);
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
fn quota_history_matches_live_python_and_shares_its_database_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let directory = TempDir::new().expect("temporary directory");
    let rust_path = directory.path().join("rust.sqlite3");
    let python_path = directory.path().join("python.sqlite3");
    let fixture = fixture();
    let rust = run_rust(&rust_path, &fixture);
    let script = r#"
import json, pathlib, sys
from easy_multi_provider.quota_history import QuotaHistoryError, QuotaHistoryStore

payload = json.load(sys.stdin)
fixture = payload["fixture"]

def run(path):
    store = QuotaHistoryStore(pathlib.Path(path))
    counts = [store.append_snapshot(fixture["account"], item["quota"], observed_at=item["observed_at"]) for item in fixture["snapshots"]]
    queries = {name: store.query(fixture["account"], name, now=fixture["now"]) for name in ("1h", "1d", "1w", "all")}
    try:
        store.query(fixture["account"], "forever", now=fixture["now"])
    except QuotaHistoryError as exc:
        invalid = str(exc)
    return {"counts": counts, "queries": queries, "invalid": invalid}

python = run(payload["python_path"])
rust_database = QuotaHistoryStore(pathlib.Path(payload["rust_path"])).query(fixture["account"], "all", now=fixture["now"])
json.dump({"python": python, "rust_database": rust_database}, sys.stdout, ensure_ascii=False, separators=(",", ":"))
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
        .expect("spawn Python quota history oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&json!({
                "fixture": fixture,
                "python_path": python_path,
                "rust_path": rust_path,
            }))
            .expect("oracle input")
            .as_bytes(),
        )
        .expect("write oracle input");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python quota history oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("oracle output");
    assert_eq!(rust, oracle["python"]);
    assert_eq!(rust["queries"]["all"], oracle["rust_database"]);

    let rust_reads_python = QuotaHistoryStore::new(&python_path)
        .query(
            fixture["account"].as_str().expect("account"),
            "all",
            fixture["now"].as_i64().expect("now"),
        )
        .expect("read Python database");
    assert_eq!(rust["queries"]["all"], rust_reads_python);
}

#[test]
fn quota_history_migrates_the_python_schema_without_plan_type() {
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
    assert_eq!(
        error.to_string(),
        "quota history path must not be a symlink"
    );
    assert_eq!(std::fs::read(target).expect("target remains"), b"private");
}
