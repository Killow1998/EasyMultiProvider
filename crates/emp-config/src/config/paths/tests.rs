use super::{ConfigPathEnvironment, ConfigPlatform, config_path_from_environment};
use std::path::PathBuf;

#[test]
fn explicit_config_precedes_environment_and_expands_home() {
    let actual = config_path_from_environment(
        ConfigPathEnvironment {
            configured: Some("  ~/custom/config.json  "),
            home: Some("/home/example"),
            user_profile: None,
            xdg_config_home: Some("/xdg"),
            local_app_data: Some("C:/Local"),
            app_data: Some("C:/Roaming"),
        },
        ConfigPlatform::Unix,
    );
    assert_eq!(actual, PathBuf::from("/home/example/custom/config.json"));
}

#[test]
fn linux_uses_nonempty_xdg_then_home_config() {
    let configured = config_path_from_environment(
        ConfigPathEnvironment {
            configured: None,
            home: Some("/home/example"),
            user_profile: None,
            xdg_config_home: Some(" /settings "),
            local_app_data: None,
            app_data: None,
        },
        ConfigPlatform::Unix,
    );
    assert_eq!(
        configured,
        PathBuf::from("/settings/easy-multi-provider/config.json")
    );

    let fallback = config_path_from_environment(
        ConfigPathEnvironment {
            configured: Some("  "),
            home: Some("/home/example"),
            user_profile: None,
            xdg_config_home: Some(" "),
            local_app_data: None,
            app_data: None,
        },
        ConfigPlatform::Unix,
    );
    assert_eq!(
        fallback,
        PathBuf::from("/home/example/.config/easy-multi-provider/config.json")
    );
}

#[test]
fn macos_uses_application_support_under_home() {
    let actual = config_path_from_environment(
        ConfigPathEnvironment {
            configured: None,
            home: Some("/Users/example"),
            user_profile: None,
            xdg_config_home: Some("/ignored"),
            local_app_data: None,
            app_data: None,
        },
        ConfigPlatform::MacOs,
    );
    assert_eq!(
        actual,
        PathBuf::from("/Users/example/Library/Application Support/EasyMultiProvider/config.json")
    );
}

#[test]
fn windows_prefers_local_app_data_then_app_data_then_user_profile() {
    let local = config_path_from_environment(
        ConfigPathEnvironment {
            configured: None,
            home: Some("C:/Home"),
            user_profile: Some("C:/Users/example"),
            xdg_config_home: None,
            local_app_data: Some("C:/Local"),
            app_data: Some("C:/Roaming"),
        },
        ConfigPlatform::Windows,
    );
    assert_eq!(
        local,
        PathBuf::from("C:/Local/EasyMultiProvider/config.json")
    );

    let roaming = config_path_from_environment(
        ConfigPathEnvironment {
            configured: None,
            home: None,
            user_profile: Some("C:/Users/example"),
            xdg_config_home: None,
            local_app_data: Some(" "),
            app_data: Some("C:/Roaming"),
        },
        ConfigPlatform::Windows,
    );
    assert_eq!(
        roaming,
        PathBuf::from("C:/Roaming/EasyMultiProvider/config.json")
    );

    let fallback = config_path_from_environment(
        ConfigPathEnvironment {
            configured: None,
            home: None,
            user_profile: Some("C:/Users/example"),
            xdg_config_home: None,
            local_app_data: None,
            app_data: None,
        },
        ConfigPlatform::Windows,
    );
    assert_eq!(
        fallback,
        PathBuf::from("C:/Users/example/AppData/Local/EasyMultiProvider/config.json")
    );
}
