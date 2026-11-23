use super::{
    COMPATIBILITY, CREDENTIAL_STATES, ConfigPlatform, RUNTIME_SOURCES, choice,
    configuration_path_mask, desktop_config_path, host_version_value, quota_status, valid_version,
};
use serde_json::json;

#[test]
fn support_report_allowlists_reject_unknown_values() {
    assert_eq!(choice(Some("available"), COMPATIBILITY), "available");
    assert_eq!(choice(Some("secret-path"), COMPATIBILITY), "unknown");
    assert_eq!(choice(Some("invalid"), CREDENTIAL_STATES), "invalid");
    assert_eq!(choice(None, CREDENTIAL_STATES), "unknown");
    assert_eq!(choice(Some("nvm"), RUNTIME_SOURCES), "nvm");
    assert_eq!(choice(Some("path_cli"), RUNTIME_SOURCES), "path_cli");
    assert_eq!(
        choice(Some("app_server_daemon"), RUNTIME_SOURCES),
        "app_server_daemon"
    );
}

#[test]
fn support_report_version_validation_matches_python_shape() {
    for valid in ["0.11.10", "1234.9.123456-beta.1+local"] {
        assert!(valid_version(valid), "{valid}");
    }
    for invalid in ["", "0.11", "12345.1.1", "1.2.1234567", "1.2.3+", "1.2.3 x"] {
        assert!(!valid_version(invalid), "{invalid}");
    }
}

#[test]
fn host_metadata_allows_msix_versions_without_changing_engine_validation() {
    assert_eq!(
        host_version_value(Some("26.915.4065.0")),
        json!("26.915.4065.0")
    );
    assert_eq!(
        host_version_value(Some("26.924.22138")),
        json!("26.924.22138")
    );
    assert!(!valid_version("26.915.4065.0"));
    assert_eq!(
        host_version_value(Some("file:///private/package")),
        json!(null)
    );
}

#[test]
fn support_report_quota_status_matches_python_precedence() {
    assert_eq!(
        quota_status(Some("quota_auth_required"), "valid", true, Some(&json!({}))),
        "auth_required"
    );
    assert_eq!(
        quota_status(
            Some("quota_transport_error"),
            "valid",
            true,
            Some(&json!({}))
        ),
        "transport_error"
    );
    assert_eq!(
        quota_status(Some("quota_rate_limited"), "valid", true, Some(&json!({}))),
        "rate_limited"
    );
    assert_eq!(
        quota_status(Some("other"), "valid", true, Some(&json!({}))),
        "unclassified_error"
    );
    assert_eq!(
        quota_status(None, "invalid", true, Some(&json!({}))),
        "auth_required"
    );
    assert_eq!(
        quota_status(None, "unknown", false, Some(&json!({}))),
        "credential_missing"
    );
    assert_eq!(
        quota_status(None, "valid", true, Some(&json!({}))),
        "success"
    );
    assert_eq!(quota_status(None, "valid", true, None), "not_checked");
}

#[test]
fn support_report_configuration_masks_desktop_working_and_custom_paths() {
    use std::path::Path;
    let home = Path::new("/home/alice");
    let cwd = Path::new("/work/project");
    let cases = [
        (
            Path::new("/home/alice/.config/easy-multi-provider/config.json"),
            ConfigPlatform::Other,
            None,
            "~/.config/easy-multi-provider/config.json",
            "desktop_default",
        ),
        (
            Path::new("/private/xdg/easy-multi-provider/config.json"),
            ConfigPlatform::Other,
            Some("/private/xdg"),
            "<xdg-config>/easy-multi-provider/config.json",
            "desktop_default",
        ),
        (
            Path::new("/home/alice/Library/Application Support/EasyMultiProvider/config.json"),
            ConfigPlatform::MacOs,
            None,
            "~/Library/Application Support/EasyMultiProvider/config.json",
            "desktop_default",
        ),
        (
            Path::new("/home/alice/AppData/Local/EasyMultiProvider/config.json"),
            ConfigPlatform::Windows,
            Some("/home/alice/AppData/Local"),
            "<user-config>/EasyMultiProvider/config.json",
            "desktop_default",
        ),
    ];
    for (path, platform, xdg, expected_path, expected_location) in cases {
        let (display, location) = configuration_path_mask(
            path,
            home,
            cwd,
            platform,
            if platform == ConfigPlatform::Windows {
                xdg
            } else {
                None
            },
            None,
            if platform == ConfigPlatform::Other {
                xdg
            } else {
                None
            },
        );
        assert_eq!(display, expected_path);
        assert_eq!(location, expected_location);
        assert!(!display.contains("alice") && !display.contains("private"));
    }
    assert_eq!(
        configuration_path_mask(
            Path::new("./config.json"),
            home,
            cwd,
            ConfigPlatform::Other,
            None,
            None,
            None,
        ),
        ("./config.json".to_owned(), "working_directory"),
    );
    assert_eq!(
        configuration_path_mask(
            Path::new("/tmp/private-config/config.json"),
            home,
            cwd,
            ConfigPlatform::Other,
            None,
            None,
            None,
        ),
        ("<custom>/config.json".to_owned(), "custom"),
    );
}

#[test]
fn desktop_config_path_uses_python_environment_precedence() {
    use std::path::{Path, PathBuf};
    let home = Path::new("/home/alice");
    assert_eq!(
        desktop_config_path(
            ConfigPlatform::Windows,
            home,
            Some("/local"),
            Some("/roaming"),
            None
        ),
        PathBuf::from("/local/EasyMultiProvider/config.json"),
    );
    assert_eq!(
        desktop_config_path(ConfigPlatform::Windows, home, None, Some("/roaming"), None),
        PathBuf::from("/roaming/EasyMultiProvider/config.json"),
    );
    assert_eq!(
        desktop_config_path(ConfigPlatform::Other, home, None, None, None),
        PathBuf::from("/home/alice/.config/easy-multi-provider/config.json"),
    );
}
