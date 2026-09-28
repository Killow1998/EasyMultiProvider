//! Codex config.toml ownership: enabling EMP writes the fields Codex reads,
//! restore puts the user's original config back exactly, and a lease makes
//! crash recovery possible.

use emp_integration::IntegrationManager;
use std::fs;

fn manager(directory: &std::path::Path) -> IntegrationManager {
    IntegrationManager::new(
        directory.join("config.toml"),
        directory.join("state/lease.json"),
        Some("fixture-instance".to_owned()),
    )
    .expect("manager")
}

#[test]
fn enable_writes_managed_fields_and_restore_returns_the_original_config() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    fs::write(
        &config,
        "# header\ntitle = \"keep\"\nopenai_base_url = \"native\"\n[nested]\nkey = \"value\"\n",
    )
    .unwrap();

    let manager = manager(directory.path());
    assert_eq!(manager.status().unwrap().state, "native");

    let enabled = manager
        .enable("http://127.0.0.1:123/v1", Some("catalog.json"), true)
        .unwrap();
    assert_eq!(enabled.action, "enabled");
    assert_eq!(enabled.state, "active");
    let applied = fs::read_to_string(&config).unwrap();
    assert!(applied.contains("openai_base_url = \"http://127.0.0.1:123/v1\""));
    assert!(applied.contains("model_catalog_json"));
    assert!(
        applied.contains("title = \"keep\""),
        "unmanaged lines survive"
    );
    assert!(manager.status().unwrap().same_instance);

    let restored = manager.restore().unwrap();
    assert_eq!(restored.action, "restored");
    let config_text = fs::read_to_string(&config).unwrap();
    assert!(config_text.contains("openai_base_url = \"native\""));
    assert!(!config_text.contains("model_catalog_json"));
    assert!(config_text.contains("title = \"keep\""));
    assert!(config_text.contains("key = \"value\""));
}

#[test]
fn restore_without_a_lease_and_crash_recovery_are_noops_that_keep_config() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    fs::write(&config, "title = \"keep\"\n").unwrap();
    let manager = manager(directory.path());
    let restored = manager.restore().unwrap();
    assert_eq!(restored.action, "noop");
    assert_eq!(fs::read_to_string(&config).unwrap(), "title = \"keep\"\n");

    // A crash mid-enable leaves the lease "prepared"; recovery restores the
    // original values so Codex is never left pointing at a dead EMP.
    let _ = manager
        .enable("http://127.0.0.1:123/v1", Some("catalog.json"), true)
        .unwrap();
    // Simulate the crash by marking the lease prepared again.
    let lease_path = directory.path().join("state/lease.json");
    let mut lease: serde_json::Value =
        serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();
    lease["status"] = "prepared".into();
    fs::write(&lease_path, serde_json::to_vec(&lease).unwrap()).unwrap();

    let recovered = manager.recover(false, false).unwrap();
    assert_eq!(recovered.action, "restored");
    // The user's original config (no EMP fields) is put back byte-for-byte.
    let restored_text = fs::read_to_string(&config).unwrap();
    assert_eq!(restored_text, "title = \"keep\"\n");
    assert!(!restored_text.contains("openai_base_url"));
}

#[test]
fn enabling_refuses_when_the_service_is_not_listening() {
    let directory = tempfile::tempdir().unwrap();
    let error = manager(directory.path())
        .enable("http://127.0.0.1:123/v1", None, false)
        .expect_err("service not ready");
    assert!(error.to_string().contains("listening"));
}

#[test]
fn search_feature_toggle_and_restore_round_trips_both_field_shapes() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    fs::write(&config, "model = \"work/gpt-5\"\n").unwrap();
    let search = emp_integration::search::SearchFeatureManager::new(
        config.clone(),
        directory.path().join("state/search-lease.json"),
    );

    search.apply(true).expect("enable search");
    let applied = fs::read_to_string(&config).unwrap();
    assert!(applied.contains("web_search"));
    assert!(applied.contains("standalone_web_search = true"));
    // Idempotent re-apply does not corrupt the config.
    search.apply(true).expect("re-apply");

    search.restore().expect("restore search");
    let restored = fs::read_to_string(&config).unwrap();
    assert!(!restored.contains("web_search"));
    assert!(restored.contains("model = \"work/gpt-5\""));
    // Restore without an active lease is a no-op.
    search.restore().expect("second restore");
}

#[test]
fn runtime_records_round_trip_and_offline_snapshots_stay_truthful() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        emp_integration::runtime::RuntimeStore::new(directory.path().join("state/runtime.json"));
    assert!(store.load().unwrap().is_none());

    let record = store
        .save(
            "reload_required",
            "emp",
            "applied",
            &[
                "work/gpt-5".to_owned(),
                "work/gpt-5".to_owned(),
                String::new(),
            ],
            true,
            "waiting for user",
        )
        .expect("save");
    assert_eq!(record.expected_models, ["work/gpt-5"]);
    assert_eq!(
        store.load().unwrap().expect("record").state,
        "reload_required"
    );

    // Offline status never claims a live verification happened.
    let snapshot = emp_integration::runtime::offline_snapshot(Some(&record), "stale");
    assert_eq!(snapshot["state"], "reload_required");
    assert_eq!(snapshot["verified"], false);
    assert_eq!(snapshot["last_known"]["state"], "reload_required");
    let empty = emp_integration::runtime::offline_snapshot(None, "stale");
    assert_eq!(empty["state"], "not_checked");
    assert_eq!(empty["last_known"], serde_json::Value::Null);
}
