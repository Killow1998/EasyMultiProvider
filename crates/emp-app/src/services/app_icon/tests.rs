use super::*;

#[test]
fn saved_appearance_loads_without_touching_codex_or_provider_configuration() {
    let root = tempfile::tempdir().unwrap();
    let mut selection = Selection {
        preset: "custom".into(),
        png: Some(STANDARD.encode(DEFAULT_PNG)),
        ..Selection::default()
    };
    std::fs::write(
        root.path().join("appearance.json"),
        serde_json::to_vec(&selection).unwrap(),
    )
    .unwrap();
    let icon = AppIcon::new(root.path()).snapshot();
    assert_eq!(icon.metadata()["preset"], "custom");
    assert_eq!(icon.tray_rgba().len(), 48 * 48 * 4);
    let ico = pixels::ico(&icon).unwrap();
    assert_eq!(&ico[..6], &[0, 0, 1, 0, 1, 0]);
    assert_eq!(&ico[22..30], b"\x89PNG\r\n\x1a\n");
    assert!(!root.path().join("config.json").exists());
    selection.png = Some(STANDARD.encode(b"not an image"));
    assert!(Image::decode(selection).is_err());
}
