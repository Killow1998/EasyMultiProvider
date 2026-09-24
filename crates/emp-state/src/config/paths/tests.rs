
    use super::{
        CONFIG_PATH_ENV, ConfigPathEnvironment, ConfigPlatform, config_path_from_environment,
    };
    use serde_json::json;
    use std::path::PathBuf;
    use std::process::Command;

    fn run_python_case(
        python: &str,
        python_root: &std::path::Path,
        script: &str,
        case: &serde_json::Value,
    ) -> serde_json::Value {
        let output = Command::new(python)
            .arg("-c")
            .arg(script)
            .arg(case.to_string())
            .current_dir(python_root)
            .env("PYTHONPATH", python_root)
            .output()
            .expect("run live Python config path oracle");
        assert!(
            output.status.success(),
            "Python config path oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("Python config path JSON");
        assert_eq!(actual["version"], "0.11.10");
        assert_eq!(
            PathBuf::from(actual["module"].as_str().expect("Python module path")),
            python_root.join("easy_multi_provider/__init__.py")
        );
        actual
    }

    fn oracle_environment(environment: &ConfigPathEnvironment<'_>) -> serde_json::Value {
        let mut values = serde_json::Map::<String, serde_json::Value>::new();
        for (key, value) in [
            (CONFIG_PATH_ENV, environment.configured),
            ("HOME", environment.home),
            ("USERPROFILE", environment.user_profile),
            ("XDG_CONFIG_HOME", environment.xdg_config_home),
            ("LOCALAPPDATA", environment.local_app_data),
            ("APPDATA", environment.app_data),
        ] {
            if let Some(value) = value {
                values.insert(key.to_owned(), json!(value));
            }
        }
        serde_json::Value::Object(values)
    }

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
            PathBuf::from(
                "/Users/example/Library/Application Support/EasyMultiProvider/config.json"
            )
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

    #[test]
    fn desktop_config_path_matches_live_python_resolver_for_each_platform() {
        let (Ok(python), Ok(python_root)) = (
            std::env::var("EMP_PYTHON_INTEROP"),
            std::env::var("EMP_PYTHON_ORACLE_ROOT"),
        ) else {
            return;
        };
        let script = r#"
import json, sys
from pathlib import Path
import easy_multi_provider
from easy_multi_provider.config import resolve_desktop_config_path
case = json.loads(sys.argv[1])
path = resolve_desktop_config_path(
    environ=case["environment"],
    user_home=Path(case["user_home"]),
    platform_name=case["platform"],
)
result = {"path": str(path), "version": easy_multi_provider.__version__, "module": easy_multi_provider.__file__}
print(json.dumps(result, separators=(",", ":")))
"#;
        let cases = [
            (
                ConfigPathEnvironment {
                    configured: None,
                    home: Some("home/example"),
                    user_profile: Some("home/example"),
                    xdg_config_home: Some("xdg"),
                    local_app_data: Some("local"),
                    app_data: Some("roaming"),
                },
                ConfigPlatform::Unix,
                "linux",
                "home/example",
            ),
            (
                ConfigPathEnvironment {
                    configured: Some(" "),
                    home: Some("home/example"),
                    user_profile: None,
                    xdg_config_home: Some("xdg/custom"),
                    local_app_data: None,
                    app_data: None,
                },
                ConfigPlatform::Unix,
                "linux",
                "home/example",
            ),
            (
                ConfigPathEnvironment {
                    configured: None,
                    home: Some("home/example"),
                    user_profile: None,
                    xdg_config_home: None,
                    local_app_data: None,
                    app_data: None,
                },
                ConfigPlatform::MacOs,
                "darwin",
                "home/example",
            ),
            (
                ConfigPathEnvironment {
                    configured: None,
                    home: None,
                    user_profile: Some("home/example"),
                    xdg_config_home: None,
                    local_app_data: Some("local/appdata"),
                    app_data: Some("roaming/appdata"),
                },
                ConfigPlatform::Windows,
                "win32",
                "home/example",
            ),
            (
                ConfigPathEnvironment {
                    configured: None,
                    home: None,
                    user_profile: Some("home/example"),
                    xdg_config_home: None,
                    local_app_data: Some(" "),
                    app_data: Some("roaming/appdata"),
                },
                ConfigPlatform::Windows,
                "win32",
                "home/example",
            ),
            (
                ConfigPathEnvironment {
                    configured: None,
                    home: None,
                    user_profile: Some("home/example"),
                    xdg_config_home: None,
                    local_app_data: None,
                    app_data: None,
                },
                ConfigPlatform::Windows,
                "win32",
                "home/example",
            ),
        ];
        let python_root = PathBuf::from(python_root);
        for (environment, platform, python_platform, user_home) in cases {
            let case = json!({
                "platform": python_platform,
                "environment": oracle_environment(&environment),
                "user_home": user_home,
            });
            let actual = run_python_case(&python, &python_root, script, &case);
            let python_path = PathBuf::from(actual["path"].as_str().expect("Python config path"));
            assert_eq!(
                python_path,
                config_path_from_environment(environment, platform)
            );
        }
    }

    #[test]
    fn config_path_matches_live_python_environment_precedence() {
        let (Ok(python), Ok(python_root)) = (
            std::env::var("EMP_PYTHON_INTEROP"),
            std::env::var("EMP_PYTHON_ORACLE_ROOT"),
        ) else {
            return;
        };
        let platform = if cfg!(windows) {
            ConfigPlatform::Windows
        } else if cfg!(target_os = "macos") {
            ConfigPlatform::MacOs
        } else {
            ConfigPlatform::Unix
        };
        let python_platform = match platform {
            ConfigPlatform::Windows => "win32",
            ConfigPlatform::MacOs => "darwin",
            ConfigPlatform::Unix => "linux",
        };
        let script = r#"
import json, os, sys
from unittest.mock import patch
import easy_multi_provider
from easy_multi_provider.config import config_path
case = json.loads(sys.argv[1])
with patch.dict(os.environ, case["environment"], clear=True):
    result = {"path": str(config_path()), "version": easy_multi_provider.__version__, "module": easy_multi_provider.__file__}
print(json.dumps(result, separators=(",", ":")))
"#;
        let cases = [
            ConfigPathEnvironment {
                configured: Some(" ~/override/config.json "),
                home: Some("home/example"),
                user_profile: Some("home/example"),
                xdg_config_home: Some("xdg/settings"),
                local_app_data: Some("local/settings"),
                app_data: Some("roaming/settings"),
            },
            ConfigPathEnvironment {
                configured: Some("  "),
                home: Some("home/example"),
                user_profile: Some("home/example"),
                xdg_config_home: Some("xdg/settings"),
                local_app_data: Some("local/settings"),
                app_data: Some("roaming/settings"),
            },
        ];
        let python_root = PathBuf::from(python_root);
        for environment in cases {
            let case = json!({
                "platform": python_platform,
                "environment": oracle_environment(&environment),
            });
            let actual = run_python_case(&python, &python_root, script, &case);
            let python_path = PathBuf::from(actual["path"].as_str().expect("Python config path"));
            assert_eq!(
                python_path,
                config_path_from_environment(environment, platform)
            );
        }
    }
