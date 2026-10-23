use super::*;

#[test]
fn toggle_preserves_options_and_restores_the_original_feature() {
    for original in [
        "# Keep this header\nmodel = 'fixture'\n",
        "[features]\nshell_snapshot = true\nasync_compaction = false\n",
        "[features]\nasync_compaction = false # Keep this explanation\n",
        "[features.async_compaction]\nenabled = false\nmodel_async_compact_threshold_percent = 73\n",
        "features = {async_compaction = {enabled = true, model_async_compact_threshold_percent = 71}}\n",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(&config, original).unwrap();
        let manager = CodexPreferences::new(config.clone(), temp.path().join("lease.json"));
        let before = manager.snapshot().unwrap();
        manager.apply(Some(true), "", "").unwrap();
        assert_eq!(manager.snapshot().unwrap()["background_enabled"], true);
        assert_eq!(
            manager.snapshot().unwrap()["background_threshold_percent"],
            before["background_threshold_percent"]
        );
        manager.apply(Some(false), "", "").unwrap();
        assert_eq!(manager.snapshot().unwrap()["background_enabled"], false);
        manager.restore().unwrap();
        assert_eq!(manager.snapshot().unwrap(), before);
        assert_eq!(std::fs::read_to_string(config).unwrap(), original);
    }
}

#[test]
fn restore_preserves_external_edits_and_unrelated_settings() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(&config, "model = 'keep'\n").unwrap();
    let manager = CodexPreferences::new(config.clone(), temp.path().join("lease.json"));
    manager.apply(Some(true), "", "").unwrap();
    let mut document: DocumentMut = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    document["features"]["other_feature"] = toml_edit::value(true);
    std::fs::write(&config, document.to_string()).unwrap();
    manager.restore().unwrap();
    let document: DocumentMut = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    assert_eq!(document["features"]["other_feature"].as_bool(), Some(true));
    assert_eq!(document["model"].as_str(), Some("keep"));
    manager.apply(Some(true), "", "").unwrap();
    let changed = "model = 'keep'\n[features.async_compaction]\nenabled = true\nmodel_async_compact_threshold_percent = 72\n";
    std::fs::write(&config, changed).unwrap();
    assert!(manager.restore().is_err());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), changed);
}

#[test]
fn memory_model_overrides_preserve_permissions_and_restore_both_original_models() {
    for original in [
        "# User settings\nmodel = 'gpt-fixture'\n",
        "[memories]\ngenerate_memories = false\nuse_memories = false\nextract_model = 'original-extract' # Keep explanation\nconsolidation_model = 'original-consolidate'\n",
        "memories = {generate_memories = false, extract_model = 'original'}\n",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(&config, original).unwrap();
        let manager = CodexPreferences::new(config.clone(), temp.path().join("preferences.json"));
        let original_models = manager.snapshot().unwrap()["memory_default_models"].clone();
        manager.apply(None, "gpt-6-luna", "gpt-6-luna").unwrap();
        assert_eq!(
            manager.snapshot().unwrap()["memory_default_models"],
            original_models
        );
        let document: DocumentMut = std::fs::read_to_string(&config).unwrap().parse().unwrap();
        assert_eq!(
            document["memories"]["extract_model"].as_str(),
            Some("gpt-6-luna")
        );
        assert_eq!(
            document["memories"]["consolidation_model"].as_str(),
            Some("gpt-6-luna")
        );
        if original.contains("generate_memories") {
            assert_eq!(
                document["memories"]["generate_memories"].as_bool(),
                Some(false)
            );
        }
        assert!(document.get("features").is_none());
        manager
            .apply(Some(true), "gpt-6.1-sol", "gpt-6.1-sol")
            .unwrap();
        let saved_lease = std::fs::read(&manager.lease).unwrap();
        std::fs::write(&manager.lease, "invalid fixture journal").unwrap();
        assert!(
            manager
                .apply(Some(false), "gpt-6-luna", "gpt-6-luna")
                .is_err()
        );
        let document = manager.read().unwrap();
        assert_eq!(
            document["memories"]["extract_model"].as_str(),
            Some("gpt-6.1-sol")
        );
        assert!(
            manager
                .rollback(Some(false), "gpt-6-luna", "gpt-6-luna")
                .is_err()
        );
        let document = manager.read().unwrap();
        for key in MEMORY_KEYS {
            assert_eq!(document["memories"][key].as_str(), Some("gpt-6-luna"));
        }
        std::fs::write(&manager.lease, saved_lease).unwrap();
        manager.restore().unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    }
}

#[test]
fn a_prepared_toggle_can_restore_after_an_interrupted_write() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(&config, "model = 'keep'\n").unwrap();
    let manager = CodexPreferences::new(config.clone(), temp.path().join("preferences.json"));
    manager.apply(Some(true), "", "").unwrap();
    let mut interrupted = manager.read_lease().unwrap().unwrap();
    interrupted["status"] = json!("prepared");
    interrupted["previous"] = interrupted["applied"].clone();
    interrupted["applied"] = json!("value = false\n");
    manager.save_lease(&interrupted).unwrap();
    manager.restore().unwrap();
    assert_eq!(std::fs::read_to_string(config).unwrap(), "model = 'keep'\n");
}

#[test]
fn memory_stages_reset_independently_to_original_values_or_absence() {
    for original in [
        "# Keep header\nmodel = 'gpt-fixture'\n",
        "[memories]\ngenerate_memories = false\nextract_model = 'gpt-original-extract' # Keep explanation\nconsolidation_model = 'gpt-original-consolidate'\n",
        "memories = {use_memories = false, extract_model = 'gpt-original'}\n",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(&config, original).unwrap();
        let manager = CodexPreferences::new(config.clone(), temp.path().join("preferences.json"));
        let before = manager.read().unwrap();
        let defaults = manager.snapshot().unwrap();
        for (extraction, consolidation) in [
            ("gpt-6-luna", "gpt-6.1-sol"),
            ("", "gpt-6.1-sol"),
            ("gpt-6-luna", ""),
            ("", ""),
        ] {
            manager.apply(None, extraction, consolidation).unwrap();
            let document = manager.read().unwrap();
            for (key, selected) in MEMORY_KEYS.into_iter().zip([extraction, consolidation]) {
                let actual = document
                    .get("memories")
                    .and_then(|table| table.get(key))
                    .and_then(Item::as_str);
                let expected = if selected.is_empty() {
                    before
                        .get("memories")
                        .and_then(|table| table.get(key))
                        .and_then(Item::as_str)
                } else {
                    Some(selected)
                };
                assert_eq!(actual, expected, "{key}");
            }
            assert_eq!(manager.snapshot().unwrap(), defaults);
        }
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        manager.apply(None, "gpt-6-luna", "").unwrap();
        let memory = manager.memories();
        let lease = memory.read_lease().unwrap().unwrap();
        let mut malformed = lease.clone();
        malformed["original"] = json!("value = true\n");
        memory.save_lease(&malformed).unwrap();
        let before_error = std::fs::read_to_string(&config).unwrap();
        assert!(manager.apply(None, "", "gpt-6.1-sol").is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before_error);
        memory.save_lease(&lease).unwrap();
        let changed = std::fs::read_to_string(&config)
            .unwrap()
            .replace("gpt-6-luna", "gpt-user-edit");
        std::fs::write(&config, &changed).unwrap();
        assert!(manager.apply(None, "", "gpt-6.1-sol").is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), changed);
    }
}
