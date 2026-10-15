use super::*;
use crate::lifecycle::ServerHandle;
use std::net::{IpAddr, Ipv4Addr};

#[test]
fn loopback_upgrade_preserves_native_settings_and_static_catalogs() {
    for dynamic in [false, true] {
        for owned_sideband in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let app_config = root.join("config.json");
            std::fs::write(&app_config, br#"{"auto_enable_on_start":false}"#).unwrap();
            let codex = root.join("codex");
            std::fs::create_dir(&codex).unwrap();
            let original = "# user settings\nopenai_base_url = \"https://native.example/v1\"\nexperimental_realtime_ws_base_url = \"https://voice.example/v1\"\nmodel = \"kept\"\n";
            std::fs::write(codex.join("config.toml"), original).unwrap();
            let auth = codex.join("auth.json");
            if dynamic {
                std::fs::write(&auth, br#"{"tokens":{"access_token":"fixture-native","account_id":"fixture-account"}}"#).unwrap();
            }
            let server = ServerHandle::start_with_config_options(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                0,
                &app_config,
                "missing-test-codex",
                auth,
            )
            .unwrap();
            let manager = &server.state.backend.integration.manager;
            let previous = format!("http://127.0.0.1:{}/v1", server.local_addr().port());
            let desired = format!("http://localhost:{}/v1", server.local_addr().port());
            let catalog = generated_catalog_path(&server.state)
                .to_string_lossy()
                .into_owned();
            assert!(
                manager
                    .enable_with_sideband(
                        &previous,
                        Some(&catalog),
                        true,
                        owned_sideband.then_some(previous.as_str())
                    )
                    .unwrap()
                    .ok()
            );

            reconcile_with_base_url(&server.state, &desired).unwrap();
            let status = manager.status().unwrap();
            assert_eq!(status.state, "active");
            assert!(
                server
                    .state
                    .backend
                    .integration
                    .startup_conflicts
                    .lock()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                status.fields["openai_base_url"].value.as_deref(),
                Some(desired.as_str())
            );
            assert_eq!(
                status.fields["model_catalog_json"].value.as_deref(),
                (!dynamic).then_some(catalog.as_str())
            );
            assert_eq!(
                status.fields[emp_integration::REALTIME_SIDEBAND_FIELD]
                    .value
                    .as_deref(),
                Some(if dynamic || owned_sideband {
                    desired.as_str()
                } else {
                    "https://voice.example/v1"
                })
            );
            let lease_before = std::fs::read(manager.lease_path()).unwrap();
            reconcile_with_base_url(&server.state, &desired).unwrap();
            let lease_after: serde_json::Value =
                serde_json::from_slice(&std::fs::read(manager.lease_path()).unwrap()).unwrap();
            let lease_before: serde_json::Value = serde_json::from_slice(&lease_before).unwrap();
            assert_eq!(
                lease_after["fields"], lease_before["fields"],
                "repeated startup keeps the original restoration baseline"
            );
            server.shutdown().unwrap();
            assert_eq!(
                std::fs::read_to_string(codex.join("config.toml")).unwrap(),
                original
            );
        }
    }
}

#[test]
fn loopback_upgrade_preserves_foreign_listener_and_external_edits() {
    for external_edit in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config = root.join("config.json");
        std::fs::write(&config, br#"{"auto_enable_on_start":false}"#).unwrap();
        let codex = root.join("codex");
        std::fs::create_dir(&codex).unwrap();
        std::fs::write(codex.join("config.toml"), "model = \"kept\"\n").unwrap();
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config,
            "missing-test-codex",
            codex.join("auth.json"),
        )
        .unwrap();
        let manager = &server.state.backend.integration.manager;
        let port = server.local_addr().port();
        let previous = format!(
            "http://127.0.0.1:{}/v1",
            if external_edit { port } else { port - 1 }
        );
        let mut previous_manager = manager.clone();
        previous_manager.instance_id = "previous-instance".into();
        assert!(previous_manager.enable(&previous, None, true).unwrap().ok());
        if external_edit {
            std::fs::write(
                manager.config_path(),
                "openai_base_url = \"https://changed.example/v1\"\nmodel = \"kept\"\n",
            )
            .unwrap();
        }
        let before = std::fs::read(manager.config_path()).unwrap();
        let lease_before = std::fs::read(manager.lease_path()).unwrap();
        reconcile_with_base_url(&server.state, &format!("http://localhost:{port}/v1")).unwrap();
        assert_eq!(std::fs::read(manager.config_path()).unwrap(), before);
        assert_eq!(std::fs::read(manager.lease_path()).unwrap(), lease_before);
        if !external_edit {
            assert_eq!(
                *server
                    .state
                    .backend
                    .integration
                    .startup_conflicts
                    .lock()
                    .unwrap(),
                vec!["listener_mismatch"]
            );
        } else {
            assert_eq!(manager.status().unwrap().state, "conflict");
        }
        server.shutdown().unwrap();
        assert_eq!(std::fs::read(codex.join("config.toml")).unwrap(), before);
    }
}
