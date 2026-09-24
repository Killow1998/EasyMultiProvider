use super::*;

#[test]
fn startup_migrates_active_v2_native_lease_and_restores_user_sideband() {
    for has_catalog in [false, true] {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = canonical_root(&directory);
        let app_config = root.join("config.json");
        let codex_home = root.join("codex");
        let native_auth = codex_home.join("auth.json");
        std::fs::create_dir_all(&codex_home).expect("create Codex home");
        std::fs::write(
            &native_auth,
            br#"{"tokens":{"access_token":"fixture-native","account_id":"fixture-account"}}"#,
        )
        .expect("write native auth");
        std::fs::write(
            codex_home.join("config.toml"),
            "openai_base_url = \"native\"\nexperimental_realtime_ws_base_url = \"https://voice.example/v1\"\n",
        )
        .expect("write initial Codex config");
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &app_config,
            "missing-test-codex",
            native_auth,
        )
        .expect("start server");
        let manager = &server.state.backend.integration.manager;
        let catalog = crate::services::catalog::generated_catalog_path(&server.state);
        let catalog_value = catalog.to_string_lossy().into_owned();
        let enabled = manager
            .enable_with_sideband(
                &server.state.base_url,
                has_catalog.then_some(catalog_value.as_str()),
                true,
                None,
            )
            .expect("create legacy-compatible active lease");
        assert!(enabled.ok());

        let lease_path = manager.lease_path();
        let mut lease: Value =
            serde_json::from_slice(&std::fs::read(lease_path).expect("read lease"))
                .expect("lease JSON");
        lease["version"] = json!(2);
        lease["fields"]
            .as_object_mut()
            .expect("lease fields")
            .remove(emp_integration::REALTIME_SIDEBAND_FIELD);
        std::fs::write(
            lease_path,
            serde_json::to_vec(&lease).expect("legacy lease JSON"),
        )
        .expect("write legacy lease");

        crate::services::startup::reconcile(&server.state)
            .expect("re-adopt and migrate startup lease");
        let migrated: Value =
            serde_json::from_slice(&std::fs::read(lease_path).expect("read migrated lease"))
                .expect("migrated lease JSON");
        assert_eq!(migrated["version"], 3);
        assert_eq!(
            migrated["fields"][emp_integration::REALTIME_SIDEBAND_FIELD]["original"],
            json!({"present":true,"value":"https://voice.example/v1"})
        );
        assert_eq!(
            migrated["fields"][emp_integration::REALTIME_SIDEBAND_FIELD]["applied"],
            json!({"present":true,"value":server.state.base_url.as_str()})
        );
        let active = std::fs::read_to_string(manager.config_path()).expect("active config");
        assert!(active.contains(&format!(
            "experimental_realtime_ws_base_url = \"{}\"",
            server.state.base_url
        )));
        server.shutdown().expect("shutdown and restore integration");
        let restored =
            std::fs::read_to_string(codex_home.join("config.toml")).expect("restored Codex config");
        assert!(
            restored.contains("experimental_realtime_ws_base_url = \"https://voice.example/v1\"")
        );
        assert!(restored.contains("openai_base_url = \"native\""));
    }
}
