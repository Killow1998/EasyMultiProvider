use super::{account_writes::Pair, *};
use crate::{
    services::app_icon::{AppIcon, DEFAULT_PNG},
    tests::{live_journal, wait_for},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    fs, os::unix::fs::PermissionsExt, path::Path, process::Command, sync::atomic::AtomicBool,
};

const LANGUAGE: &str = "/api/language";
const ICON: &str = "/api/app-icon";
const USAGE: &str = "/api/usage/scan";
const EVENTS: &str = "/api/client-events";
const RUNTIME: &str = "/api/runtime/scan";
const PATHS: [&str; 5] = [LANGUAGE, ICON, USAGE, EVENTS, RUNTIME];
const LAUNCHER: &str = ".local/share/applications/easy-multi-provider.desktop";
const VALID_EVENT: &[u8] = br#"{"kind":"integration_load","phase":"render_completed"}"#;
const ASSETS: [&str; 2] = ["/assets/app-icon.png", "/assets/app-icon-source.png"];

fn with_temporary_home(test_name: &str, test: impl FnOnce(&Path)) {
    const CHILD_HOME: &str = "EMP_LOCAL_WRITES_TEST_HOME";
    if let Some(home) = std::env::var_os(CHILD_HOME) {
        assert_eq!(std::env::var_os("HOME").as_ref(), Some(&home));
        let home = Path::new(&home);
        let launcher = home.join(LAUNCHER);
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(
            launcher,
            "[Desktop Entry]\nName=EMP fixture\nIcon=original\n",
        )
        .unwrap();
        test(home);
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let (_, module) = module_path!().split_once("::").unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("{module}::{test_name}")])
        .env(CHILD_HOME, home.path())
        .env("HOME", home.path())
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("NVM_DIR")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        result.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn fixture(home: &Path, executable: &Path) -> Fixture {
    let directory = tempfile::tempdir_in(home).unwrap();
    let root = directory.path();
    let config = root.join("config.json");
    fs::write(
        &config,
        json!({
            "language":"en",
            "native_catalog_path":root.join("codex/models_cache.json"),
            "account_store_path":root.join("accounts"),
        })
        .to_string(),
    )
    .unwrap();
    let legacy = LegacyServer::start_with_config_options(
        LOOPBACK.ip(),
        0,
        &config,
        executable.to_str().unwrap(),
        root.join("codex/auth.json"),
    )
    .unwrap();
    Fixture {
        directory,
        legacy,
        upstream: Arc::default(),
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

async fn read(fixture: &Fixture, address: SocketAddr, path: &str) -> Value {
    let wire = request(fixture.port(), "GET", path, &[&fixture.session()], b"");
    let reply = exchange_wire(address, &wire, false).await;
    assert_status(&reply, 200);
    json_body(&reply)
}

fn event_fields(fixture: &Fixture) -> Vec<(Value, Value)> {
    live_journal(fixture.directory.path())
        .into_iter()
        .filter(|entry| entry["event"] == "web_client_phase")
        .map(|mut entry| (entry["level"].take(), entry["fields"].take()))
        .collect()
}

fn assert_icon(fixture: &Fixture, home: &Path, expected: &Value, metadata: &Value, png: &[u8]) {
    let root = fixture.directory.path().join("state");
    assert_eq!(&read_json(&root.join("appearance.json")), expected);
    assert_eq!(&AppIcon::new(&root).snapshot().metadata(), metadata);
    let image = root
        .join("app-icons")
        .join(format!("{}.png", metadata["version"].as_str().unwrap()));
    assert_eq!(fs::read(&image).unwrap(), png);
    assert_eq!(
        fs::read_to_string(home.join(LAUNCHER)).unwrap(),
        format!(
            "[Desktop Entry]\nName=EMP fixture\nIcon={}\n",
            image.display()
        ),
    );
}

async fn compare_icon(pair: &Pair<'_>, home: &Path) -> Value {
    let mut source = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut source, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[5, 10, 15, 255])
            .unwrap();
    }
    let expected = json!({
        "preset":"fixture-custom","monochrome":true,
        "png":STANDARD.encode(DEFAULT_PNG),"original_png":STANDARD.encode(&source),
    });
    let mut body = expected.clone();
    body["ignored"] = json!("x".repeat(3 * 1024 * 1024));
    let body = body.to_string();
    let old_request = json_request(
        pair.old.port(),
        ICON,
        &[&pair.old.session()],
        body.as_bytes(),
    );
    let new_request = json_request(
        pair.new.port(),
        ICON,
        &[&pair.new.session()],
        body.as_bytes(),
    );
    let old = exchange_wire(pair.old.legacy.local_addr(), &old_request, false).await;
    assert_status(&old, 200);
    let result = json_body(&old);
    assert_eq!(result["launcher_updated"], true);
    let metadata = &result["app_icon"];
    assert_eq!(metadata["preset"], "fixture-custom");
    assert_eq!(metadata["monochrome"], true);
    assert_icon(pair.old, home, &expected, metadata, DEFAULT_PNG);
    let new = exchange_wire(pair.candidate, &new_request, false).await;
    assert_wire_response_parity(&old_request, &old, &new);
    assert_icon(pair.new, home, &expected, metadata, DEFAULT_PNG);

    for (path, png) in ASSETS.into_iter().zip([DEFAULT_PNG, source.as_slice()]) {
        let old = request(pair.old.port(), "GET", path, &[], b"");
        let new = request(pair.new.port(), "GET", path, &[], b"");
        let reply = assert_wire_parity_with(
            [(pair.old.legacy.local_addr(), &old), (pair.candidate, &new)],
            false,
            |reply| reply,
        )
        .await;
        assert_status(&reply, 200);
        let (_, body) = reply.split_at(
            reply
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap()
                + 4,
        );
        assert_eq!(body, png);
    }
    metadata.clone()
}

async fn assert_catalog_refresh_queued(fixture: &Fixture) {
    let state = fixture.legacy.state.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let mut pending = tokio::task::spawn_blocking({
        let state = state.clone();
        let stop = stop.clone();
        move || {
            state
                .catalog_refresh
                .wait_for_work(|| stop.load(Ordering::Acquire))
        }
    });
    let result = timeout(Duration::from_secs(5), &mut pending).await;
    if result.is_err() {
        stop.store(true, Ordering::Release);
        state.catalog_refresh.stop();
        pending.await.unwrap();
    }
    assert_eq!(result.unwrap().unwrap(), Some(false));
    state.catalog_refresh.finish_pass();
}

#[test]
fn local_writes_match_legacy_wire_and_read_back_state() {
    with_temporary_home(
        "local_writes_match_legacy_wire_and_read_back_state",
        compare_local_writes,
    );
}

fn compare_local_writes(home: &Path) {
    let executable = home.join("fixture-runtime");
    let old = fixture(home, &executable);
    fixture(home, &executable).run_candidate(async |new, candidate| {
        let pair = Pair { old: &old, new, candidate };
        let oversized = gzip(&vec![b' '; 5 * 1024 * 1024 + 1], Vec::new());
        for path in PATHS {
            pair.parity(|port, _| json_request(port, path, &[], b"{"), 401, |reply| reply).await;
            pair.parity(
                |port, session| json_request(port, path, &[session, "Origin: https://other.invalid"], b"{"),
                403,
                |reply| reply,
            ).await;
            pair.post(path, b"{", 400).await;
            assert_eq!(
                pair.post(path, b"[]", 400).await["error"]["message"],
                "request body must be a JSON object"
            );
            let limit = pair.parity(
                |port, session| request(port, "POST", path, &[
                    session, "Content-Type: application/json", "Content-Length: 5242881",
                ], b""),
                413,
                |reply| reply,
            ).await;
            assert_eq!(limit["error"]["limit_bytes"], 5 * 1024 * 1024);
            let decoded = pair.parity(
                |port, session| json_request(port, path, &[session, "Content-Encoding: gzip"], &oversized),
                413,
                |reply| reply,
            ).await;
            assert_eq!(decoded["error"]["limit_bytes"], 5 * 1024 * 1024);
        }
        for body in [b"{}".as_slice(), br#"{"language":null}"#, br#"{"language":12}"#] {
            assert_eq!(
                pair.post(LANGUAGE, body, 400).await["error"]["message"],
                "language is required"
            );
        }
        assert_eq!(
            pair.post(LANGUAGE, br#"{"language":"fr"}"#, 400).await["error"]["message"],
            "language must be en or zh-CN"
        );
        assert_eq!(
            pair.post(LANGUAGE, br#"{"language":"zh-CN","ignored":true}"#, 200).await,
            json!({"language":"zh-CN"})
        );
        for body in [
            b"{}".as_slice(),
            br#"{"preset":"emp"}"#,
            br#"{"preset":"emp","monochrome":"false"}"#,
            br#"{"preset":"emp","monochrome":false,"original_png":17}"#,
            br#"{"preset":"emp","monochrome":false,"png":"bad"}"#,
        ] {
            assert_eq!(pair.post(ICON, body, 400).await["error"]["code"], "app_icon_invalid");
        }
        let icon = compare_icon(&pair, home).await;

        let mut history = pair.post(USAGE, br#"{"ignored":[1]}"#, 202).await;
        assert_eq!(history["command_id"], 1);
        assert_eq!(history["requested"], 1);
        assert_eq!(history["queued"], true);
        history.as_object_mut().unwrap().remove("command_id");

        for body in [
            b"{}".as_slice(),
            br#"{"kind":"window_error"}"#,
            br#"{"kind":10,"phase":"failed"}"#,
            br#"{"kind":"window_error","phase":false}"#,
            br#"{"kind":"window_error","phase":"unknown"}"#,
        ] {
            assert_eq!(
                pair.post(EVENTS, body, 400).await["error"]["message"],
                "invalid web client diagnostic event"
            );
        }
        assert_eq!(pair.post(EVENTS, VALID_EVENT, 200).await, json!({"ok":true}));
        let bounded = br#"{
            "kind":"window_error","phase":"failed","failure_class":null,"page_id":" panel/settings ",
            "duration_ms":"9000000","http_status":999,"line":-10,"column":{},"private":"ignored"
        }"#;
        assert_eq!(pair.post(EVENTS, bounded, 200).await, json!({"ok":true}));
        let events = vec![
            (json!("info"), json!({
                "kind":"integration_load","phase":"render_completed","failure_class":"none"
            })),
            (json!("warning"), json!({
                "kind":"window_error","phase":"failed","failure_class":"unknown","page_id":"panel/settings",
                "duration_ms":3_600_000,"http_status":599,"line":0
            })),
        ];

        let configured = |inventory: &Value| {
            inventory["runtimes"].as_array().unwrap().iter()
                .find(|runtime| runtime["source"] == "configured").unwrap().clone()
        };
        for fixture in [&old, new] {
            let before = fixture.legacy.state.backend.integration.inventory.snapshot(false);
            assert_eq!(configured(&before)["failure"], "not_found");
        }
        fs::write(&executable, "#!/bin/sh\nprintf 'codex-cli 0.159.2\\n'\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let inventory = pair.post(RUNTIME, br#"{"ignored":true}"#, 200).await;
        assert_eq!(configured(&inventory)["path"], json!(executable));
        assert_ne!(configured(&inventory)["failure"], "not_found");

        for (fixture, address) in [(&old, old.legacy.local_addr()), (new, candidate)] {
            let root = fixture.directory.path();
            assert_eq!(read_json(&root.join("config.json"))["language"], "zh-CN");
            let config = read(fixture, address, "/api/config").await;
            assert_eq!(config["language"], "zh-CN");
            assert_eq!(config["app_icon"], icon);
            assert_eq!(read(fixture, address, "/api/usage").await["history"], history);
            assert_eq!(event_fields(fixture), events);
            assert_catalog_refresh_queued(fixture).await;
            assert_eq!(
                read(fixture, address, "/api/integration").await["codex_compatibility"],
                inventory
            );
            assert!(fixture.legacy.state.updates.drain_for_test(Duration::ZERO));
        }
        for path in PATHS {
            assert_eq!(pair.post(path, b"{", 503).await["error"]["code"], "updating");
        }
        for (path, body) in [
            (LANGUAGE, br#"{"language":"en"}"#.as_slice()),
            (ICON, br#"{"preset":"emp","monochrome":false}"#),
            (USAGE, b"{}"),
            (EVENTS, VALID_EVENT),
            (RUNTIME, b"{}"),
        ] {
            assert_eq!(pair.post(path, body, 503).await["error"]["code"], "updating");
        }
        let denied = pair.parity(
            |port, _| json_request(port, EVENTS, &[], VALID_EVENT), 503, |reply| reply,
        ).await;
        assert_eq!(denied["error"]["code"], "updating");
        for (fixture, address) in [(&old, old.legacy.local_addr()), (new, candidate)] {
            let config = read(fixture, address, "/api/config").await;
            assert_eq!(config["language"], "zh-CN");
            assert_eq!(config["app_icon"], icon);
            assert_eq!(read(fixture, address, "/api/usage").await["history"], history);
            assert_eq!(event_fields(fixture), events);
        }
    });
    old.legacy.shutdown().unwrap();
}

#[test]
fn submitted_app_icon_write_finishes_once_after_disconnect() {
    with_temporary_home(
        "submitted_app_icon_write_finishes_once_after_disconnect",
        |home| {
            let executable = home.join("fixture-runtime");
            let old = fixture(home, &executable);
            fixture(home, &executable).run_candidate(async |new, candidate| {
                for (fixture, address) in [(&old, old.legacy.local_addr()), (new, candidate)] {
                    let state = &fixture.legacy.state;
                    let (locked, ready) = tokio::sync::oneshot::channel();
                    let (release, unlocked) = tokio::sync::oneshot::channel();
                    let gated = state.clone();
                    let hold_image = tokio::task::spawn_blocking(move || {
                        let _image = gated.backend.app_icon.lock_image_for_test();
                        locked.send(()).unwrap();
                        let _ = unlocked.blocking_recv();
                    });
                    ready.await.unwrap();

                    let mut client = TcpStream::connect(address).await.unwrap();
                    let selected = json!({
                        "preset":"fixture-disconnect",
                        "monochrome":true,
                        "png":STANDARD.encode(DEFAULT_PNG),
                    });
                    let wire = json_request(
                        fixture.port(),
                        ICON,
                        &[&fixture.session()],
                        selected.to_string().as_bytes(),
                    );
                    client.write_all(&wire).await.unwrap();
                    let saved = fixture.directory.path().join("state/appearance.json");
                    tokio::task::spawn_blocking(move || {
                        wait_for(Duration::from_secs(5), || saved.is_file())
                    })
                    .await
                    .unwrap();
                    assert_eq!(state.backend.app_icon.revision.load(Ordering::Acquire), 0);
                    assert!(!state.updates.drain_for_test(Duration::ZERO));
                    client.set_zero_linger().unwrap();
                    drop(client);
                    release.send(()).unwrap();
                    hold_image.await.unwrap();
                    let state = state.clone();
                    tokio::task::spawn_blocking(move || {
                        assert!(
                            state
                                .connection_admission
                                .quiesce(0, Duration::from_secs(5))
                                .is_some()
                        );
                        assert!(state.updates.drain_for_test(Duration::ZERO));
                        assert_eq!(state.backend.app_icon.revision.load(Ordering::Acquire), 1);
                    })
                    .await
                    .unwrap();
                    let metadata = read(fixture, address, "/api/config").await["app_icon"].clone();
                    assert_eq!(metadata["preset"], "fixture-disconnect");
                    assert_eq!(metadata["monochrome"], true);
                    assert_icon(fixture, home, &selected, &metadata, DEFAULT_PNG);
                }
            });
            old.legacy.shutdown().unwrap();
        },
    );
}
