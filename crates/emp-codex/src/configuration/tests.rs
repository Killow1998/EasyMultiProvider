use super::*;
use std::fs;

#[test]
fn enable_preserves_unmanaged_toml_style_and_restore_is_exact_for_fields() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let lease = directory.path().join("state/lease.json");
    fs::write(&config,"# keep\nopenai_base_url   = \"native\"  # inline\ntitle = \"keep\"\n[nested]\nopenai_base_url = \"nested\"\n").unwrap();
    let manager = IntegrationManager::new(&config, &lease, Some("instance".to_owned())).unwrap();
    assert_eq!(
        manager
            .enable("http://127.0.0.1:123/v1", Some("catalog.json"), true)
            .unwrap()
            .state,
        "active"
    );
    let applied = fs::read_to_string(&config).unwrap();
    assert!(applied.contains("openai_base_url   = \"http://127.0.0.1:123/v1\"  # inline"));
    assert!(applied.contains("[nested]\nopenai_base_url = \"nested\""));
    manager.restore().unwrap();
    let restored = fs::read_to_string(&config).unwrap();
    assert!(restored.contains("openai_base_url   = \"native\"  # inline"));
    assert!(restored.contains("title = \"keep\""));
}
