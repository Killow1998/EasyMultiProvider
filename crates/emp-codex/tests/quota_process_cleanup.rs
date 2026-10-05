#![cfg(unix)]

use emp_codex::quota::{QuotaControl, run_quota_query_persisting};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

// The launcher models npm Codex: its native child inherits both output pipes.
// Detaching the child additionally verifies that reader joins are cancellable.
const LAUNCHER: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys, time
root = pathlib.Path(__file__).parent
mode = (root / 'mode').read_text()
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(5)'],
    stdin=subprocess.DEVNULL, start_new_session=(mode == 'detached'))
(root / 'child').write_text(str(child.pid))
for line in sys.stdin:
    request = json.loads(line)
    if request.get('id') is None:
        continue
    if request['id'] == 2:
        auth = pathlib.Path(os.environ['CODEX_HOME']) / 'auth.json'
        value = json.loads(auth.read_text())
        value['tokens']['access_token'] = 'rotated-fixture'
        auth.write_text(json.dumps(value))
        (root / 'ready').touch()
        if mode != 'success':
            time.sleep(5)
    result = {'rateLimits': {'limitId':'codex', 'primary':{'usedPercent':7}}} if request['id'] == 3 else {}
    print(json.dumps({'id':request['id'], 'result':result}), flush=True)
"#;

fn check_launcher(mode: &str, cancel: bool) {
    let root = tempfile::tempdir().unwrap();
    let launcher = root.path().join("codex");
    fs::write(&launcher, LAUNCHER).unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.path().join("mode"), mode).unwrap();
    let cancelled = AtomicBool::new(false);
    let started = Instant::now();
    let mut saved = None;
    let result = thread::scope(|scope| {
        if cancel {
            let ready = root.path().join("ready");
            let flag = &cancelled;
            scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !ready.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                flag.store(true, Ordering::Release);
            });
        }
        run_quota_query_persisting(
            &json!({"tokens":{"access_token":"fixture", "account_id":"fixture"}}),
            launcher.to_str().unwrap(),
            QuotaControl {
                timeout: if cancel || mode == "success" {
                    Duration::from_secs(4)
                } else {
                    Duration::from_millis(500)
                },
                cancelled: cancel.then_some(&cancelled),
            },
            true,
            |auth| {
                saved = Some(auth.clone());
                Ok(())
            },
        )
    });
    let elapsed = started.elapsed();
    // Always clean up the deliberately detached fixture before assertions.
    let pid: i32 = fs::read_to_string(root.path().join("child"))
        .unwrap()
        .parse()
        .unwrap();
    let running = unsafe { libc::kill(pid, 0) } == 0;
    #[cfg(target_os = "linux")]
    let running = running
        && fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            !stat
                .rsplit_once(')')
                .unwrap()
                .1
                .trim_start()
                .starts_with('Z')
        });
    unsafe { libc::kill(pid, libc::SIGKILL) };

    if mode == "success" {
        assert!(result.is_ok(), "{result:?}");
    } else {
        assert_eq!(
            result.unwrap_err().code(),
            if cancel {
                "quota_cancelled"
            } else {
                "quota_timeout"
            }
        );
    }
    assert!(
        elapsed < Duration::from_secs(3),
        "helper retained the pipes for {elapsed:?}"
    );
    assert_eq!(saved.unwrap()["tokens"]["access_token"], "rotated-fixture");
    #[cfg(target_os = "linux")]
    if mode != "detached" {
        assert!(!running, "the launcher child survived quota cleanup");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = running;
}

#[test]
fn timeout_stops_the_launcher_tree_and_preserves_rotated_credentials() {
    check_launcher("timeout", false);
}

#[test]
fn cancellation_stops_the_launcher_tree_and_preserves_rotated_credentials() {
    check_launcher("cancel", true);
}

#[test]
fn successful_launcher_exit_also_cleans_up_inherited_pipes() {
    check_launcher("success", false);
}

#[test]
fn detached_pipe_holder_cannot_block_reader_cleanup() {
    check_launcher("detached", false);
}
