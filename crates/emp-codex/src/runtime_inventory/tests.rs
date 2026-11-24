use super::{
    RuntimeInventory,
    trust::tests::{private_dir, script},
};
use emp_test_support::{Deadline, test_timeout};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[test]
fn prepared_executable_reprobes_when_a_cached_runtime_is_replaced_or_removed() {
    let root = private_dir();
    let codex_home = root.path().join("codex-home");
    let user_home = root.path().join("user-home");
    std::fs::create_dir(&codex_home).unwrap();
    std::fs::create_dir(&user_home).unwrap();
    let binary = script(root.path(), "codex", "echo codex-cli 0.158.0", 0o700);
    let inventory = RuntimeInventory::with_discovery(
        codex_home,
        user_home,
        Some(binary.clone()),
        Some(OsString::new()),
        Vec::new(),
    );

    assert_eq!(inventory.prepared_executable().unwrap().path(), binary);
    assert_eq!(inventory.snapshot(false)["helper_source"], "configured");
    assert_eq!(
        inventory.snapshot(false)["outside_maintenance_window"],
        true
    );

    let replacement = root.path().join("replacement");
    script(root.path(), "replacement", "echo codex-cli 0.148.0", 0o700);
    std::fs::rename(&replacement, &binary).unwrap();
    assert_eq!(inventory.prepared_executable().unwrap().path(), binary);
    assert_eq!(
        inventory.selected_trusted_version().as_deref(),
        Some("0.148.0")
    );

    for (version, outside_window) in [
        ("0.166.0", true),
        ("0.167.0", false),
        ("0.169.1", false),
        ("0.171.0", false),
    ] {
        script(
            root.path(),
            "replacement",
            &format!("echo codex-cli {version}"),
            0o700,
        );
        std::fs::rename(&replacement, &binary).unwrap();
        assert_eq!(inventory.prepared_executable().unwrap().path(), binary);
        let snapshot = inventory.snapshot(false);
        assert_eq!(snapshot["installed"], version);
        assert_eq!(snapshot["outside_maintenance_window"], outside_window);
    }

    std::fs::remove_file(&binary).unwrap();
    assert!(
        inventory
            .prepared_executable()
            .map_or(true, |prepared| prepared.path() != binary)
    );
    let refreshed = inventory.snapshot(false);
    let configured = refreshed["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|runtime| runtime["source"] == "configured")
        .unwrap();
    assert_eq!(configured["available"], false);
}

#[test]
fn constructor_context_does_not_need_a_live_path_or_nvm_install() {
    let root = private_dir();
    let home = root.path().join("home");
    let user_home = root.path().join("user");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&user_home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let inventory =
        RuntimeInventory::with_discovery(home, user_home, None, Some(OsString::new()), Vec::new());
    let snapshot = inventory.snapshot(false);
    assert!(
        snapshot["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|runtime| runtime["source"] == "codex_app" || runtime["source"] == "running")
    );
}

#[test]
fn cached_version_does_not_wait_for_or_repeat_a_delayed_refresh() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let root = private_dir();
    let home = root.path().join("home");
    let user_home = root.path().join("user");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&user_home).unwrap();
    let calls = root.path().join("calls");
    let delay = root.path().join("delay");
    let entered = root.path().join("entered");
    let release = root.path().join("release");
    let body = format!(
        "echo called >> '{}'\nif [ -f '{}' ]; then\n: > '{}'\nwhile [ ! -f '{}' ]; do /bin/sleep 0.01; done\nfi\necho codex-cli 0.158.0",
        calls.display(),
        delay.display(),
        entered.display(),
        release.display()
    );
    let binary = script(root.path(), "codex", &body, 0o700);
    let inventory = RuntimeInventory::with_discovery(
        home,
        user_home,
        Some(binary),
        Some(OsString::new()),
        Vec::new(),
    );
    inventory.snapshot(false);
    assert_eq!(
        inventory.selected_trusted_version().as_deref(),
        Some("0.158.0")
    );
    std::fs::write(&delay, b"").unwrap();

    let inventory = &inventory;
    let read = std::thread::scope(|scope| {
        let refreshing = scope.spawn(|| inventory.snapshot(true));
        let deadline = Deadline::after(test_timeout(Duration::from_secs(1)));
        while !entered.exists() && !deadline.is_expired() {
            #[expect(
                clippy::disallowed_methods,
                reason = "探测替身通过文件报告进入状态，以固定间隔轮询"
            )]
            std::thread::sleep(Duration::from_millis(5));
        }
        let entered_in_time = deadline.accept(entered.exists(), "delayed probe started");
        let requested = Instant::now();
        let queued = scope.spawn(move || inventory.snapshot_requested(true, requested));
        let (sender, receiver) = mpsc::channel();
        scope.spawn(move || sender.send(inventory.selected_trusted_version()).unwrap());
        let read = receiver.recv_timeout(Duration::from_millis(500));
        std::fs::write(&release, b"").unwrap();
        assert!(entered_in_time.unwrap(), "delayed probe did not start");
        assert_eq!(refreshing.join().unwrap()["installed"], "0.158.0");
        assert_eq!(queued.join().unwrap().value["installed"], "0.158.0");
        read
    });
    assert_eq!(read.unwrap().as_deref(), Some("0.158.0"));
    assert_eq!(
        std::fs::read_to_string(calls).unwrap().lines().count(),
        2,
        "one initial probe and one coalesced refresh; cached reads never probe"
    );
}

#[test]
fn selected_trusted_version_never_probes_a_cold_stale_or_changed_cache() {
    let root = tempfile::Builder::new()
        .prefix("emp-inventory-version-")
        .tempdir_in(emp_test_support::fixture_root())
        .unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.path().join("codex-home");
    let user_home = root.path().join("user-home");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&user_home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&user_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let version_calls = root.path().join("version-calls");
    let app_server_calls = root.path().join("app-server-calls");
    let body = format!(
        "if [ \"$1\" = \"--version\" ]; then echo called >> '{}'; echo 'codex-cli 0.155.0-alpha.9.2'; else echo called >> '{}'; fi",
        version_calls.display(),
        app_server_calls.display()
    );
    let binary = script(root.path(), "codex", &body, 0o700);
    assert_eq!(
        super::trust::trusted_binary(&binary),
        Some(binary.canonicalize().unwrap()),
        "test runtime must satisfy the unchanged executable trust policy"
    );
    let inventory = RuntimeInventory::with_discovery(
        home,
        user_home,
        Some(binary.clone()),
        Some(OsString::new()),
        Vec::new(),
    );

    assert_eq!(inventory.selected_trusted_version(), None);
    assert!(!version_calls.exists(), "cold read must not launch a probe");
    inventory.snapshot(false);
    assert_eq!(
        inventory.selected_trusted_version().as_deref(),
        Some("0.155.0-alpha.9.2")
    );
    {
        let mut cache = inventory.cache.lock().unwrap();
        std::sync::Arc::get_mut(cache.as_mut().unwrap())
            .unwrap()
            .checked = std::time::Instant::now() - std::time::Duration::from_secs(120);
    }
    assert_eq!(
        inventory.selected_trusted_version().as_deref(),
        Some("0.155.0-alpha.9.2")
    );
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(inventory.selected_trusted_version(), None);
    assert_eq!(
        std::fs::read_to_string(&version_calls)
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!app_server_calls.exists());
}

#[test]
fn unavailable_configured_launcher_is_reported_without_probing_it() {
    use super::{discovery::Candidate, observe_candidate};

    let root = private_dir();
    let marker = root.path().join("launcher-was-executed");
    let launcher = script(
        root.path(),
        "codex.cmd",
        &format!("touch '{}'", marker.display()),
        0o700,
    );
    let observation = observe_candidate(&Candidate {
        source: "configured",
        name: "Configured Codex",
        path: launcher,
        host_version: None,
        fallbacks: Vec::new(),
        unavailable: true,
        running: false,
    });
    assert_eq!(observation["status"], "unavailable");
    assert_eq!(observation["installed"], serde_json::Value::Null);
    assert!(!marker.exists(), "unavailable launcher was probed");
}

#[test]
#[ignore = "manual installed Codex 0.158.0 inventory probe; set EMP_CODEX_0158_BINARY"]
fn installed_codex_0158_is_reported_available_by_runtime_inventory() {
    let binary = std::env::var_os("EMP_CODEX_0158_BINARY")
        .expect("EMP_CODEX_0158_BINARY must point to the installed 0.158.0 runtime");
    let home = PathBuf::from(std::env::var_os("CODEX_HOME").expect("disposable CODEX_HOME"));
    let user_home = PathBuf::from(std::env::var_os("HOME").expect("disposable HOME"));
    let inventory = RuntimeInventory::with_discovery(
        home,
        user_home,
        Some(PathBuf::from(binary)),
        Some(OsString::new()),
        Vec::new(),
    );

    let snapshot = inventory.snapshot(true);
    assert_eq!(snapshot["installed"], "0.158.0");
    assert_eq!(snapshot["status"], "available");
    assert_eq!(snapshot["helper_source"], "configured");
    assert_eq!(snapshot["runtimes"][0]["helper"], true);
}
#[test]
#[ignore = "requires a running trusted Codex app-server"]
fn installed_running_runtime_beats_an_idle_legacy_plugin() {
    let root = private_dir();
    let home = root.path().join("codex-home");
    let plugin = home.join("plugins/.plugin-appserver");
    std::fs::create_dir_all(&plugin).unwrap();
    script(&plugin, "codex", "echo codex-cli 0.163.0-alpha.11.1", 0o700);
    let inventory = RuntimeInventory::with_discovery(
        home,
        root.path().to_owned(),
        None,
        Some(OsString::new()),
        Vec::new(),
    );
    let observed = inventory.snapshot(true);
    assert!(
        observed["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|runtime| runtime["helper"] == true && runtime["running"] == true),
        "{observed}"
    );
    assert!(
        semver::Version::parse(observed["installed"].as_str().unwrap()).unwrap()
            >= semver::Version::new(0, 169, 1)
    );
    assert!(
        observed["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|runtime| runtime["source"] == "codex_app"
                && runtime["installed"] == "0.163.0-alpha.11.1"
                && runtime["helper"] == false)
    );
    assert_eq!(
        inventory.selected_trusted_version().as_deref(),
        observed["installed"].as_str()
    );
}
