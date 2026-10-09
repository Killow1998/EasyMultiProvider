use super::*;

#[test]
fn effort_is_optional_and_model_is_configured() {
    let dir = tempfile::tempdir().expect("temp dir");
    let working_directory = dir.path().join("workspace");
    std::fs::create_dir(&working_directory).expect("private working directory");
    let prompt_file = dir.path().join("prompt");
    let command = command(CommandConfig {
        executable: Path::new("/bin/true"),
        child_path: OsStr::new("/bin"),
        model: "upstream-model",
        effort: Some("high"),
        output_limit: Some(4096),
        prompt_file: &prompt_file,
        schema: "{}",
        input_format: InputFormat::StreamJson,
        mode: InvocationMode::CpaRelay {
            port: 1234,
            token: "local-only-token",
            home: dir.path(),
        },
        config_home: dir.path(),
        cache_home: dir.path(),
        temp: dir.path(),
        working_directory: &working_directory,
    });
    let args = command
        .get_args()
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>();
    assert_eq!(command.get_current_dir(), Some(working_directory.as_path()));
    let envs = command
        .get_envs()
        .filter_map(|(key, value)| Some((key.to_string_lossy(), value?.to_string_lossy())));
    let envs = envs.collect::<Vec<_>>();
    assert!(
        envs.iter()
            .any(|(key, value)| key == "DISABLE_AUTOUPDATER" && value == "1")
    );
    assert!(
        envs.iter()
            .any(|(key, value)| key == "DISABLE_UPDATES" && value == "1")
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--model", "upstream-model"])
    );
    assert!(args.windows(2).any(|pair| pair == ["--effort", "high"]));
    assert!(
        envs.iter()
            .any(|(key, value)| key == "CLAUDE_CODE_MAX_OUTPUT_TOKENS" && value == "4096")
    );
    assert!(
        envs.iter()
            .any(|(key, value)| key == "DISABLE_COMPACT" && value == "1")
    );
    assert!(
        !envs
            .iter()
            .any(|(key, _)| key == "CLAUDE_CODE_MAX_CONTEXT_TOKENS")
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--input-format", "stream-json"])
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--output-format", "stream-json"])
    );
    assert!(args.iter().any(|arg| arg == "--verbose"));
    assert!(!args.iter().any(|arg| arg == "opus" || arg == "low"));
}

#[test]
fn local_login_command_uses_host_auth_without_provider_credentials_or_user_hooks() {
    let dir = tempfile::tempdir().expect("temp dir");
    let working_directory = dir.path().join("empty-workspace");
    std::fs::create_dir(&working_directory).expect("private working directory");
    let prompt_file = dir.path().join("system-prompt.txt");
    let command = command(CommandConfig {
        executable: Path::new("/bin/true"),
        child_path: OsStr::new("/usr/bin:/bin"),
        model: "configured-alias",
        effort: None,
        output_limit: None,
        prompt_file: &prompt_file,
        schema: "{}",
        input_format: InputFormat::StreamJson,
        mode: InvocationMode::LocalLogin { home: dir.path() },
        config_home: &dir.path().join("xdg-config"),
        cache_home: &dir.path().join("xdg-cache"),
        temp: dir.path(),
        working_directory: &working_directory,
    });
    let args = command
        .get_args()
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>();
    let envs = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect::<Vec<_>>();
    let env_names = envs.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>();

    assert_eq!(command.get_current_dir(), Some(working_directory.as_path()));
    assert!(!args.iter().any(|arg| arg == "--bare"));
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--model", "configured-alias"])
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--setting-sources", "project"])
    );
    assert!(
        args.windows(2)
            .any(|pair| { pair == ["--settings", r#"{"disableAllHooks":true}"#] })
    );
    assert!(args.windows(2).any(|pair| pair == ["--tools", ""]));
    assert!(!env_names.contains(&"CLAUDE_CODE_MAX_CONTEXT_TOKENS"));
    assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
    assert!(args.iter().any(|arg| arg == "--disable-slash-commands"));
    assert!(args.iter().any(|arg| arg == "--no-session-persistence"));
    assert!(args.windows(2).any(|pair| pair == ["--max-turns", "1"]));
    assert!(args.iter().any(|arg| arg == "--include-partial-messages"));
    assert!(envs.iter().any(|(key, value)| {
        key == "CLAUDE_CODE_DISABLE_CLAUDE_MDS" && value.as_deref() == Some("1")
    }));
    assert!(LOCAL_NETWORK_ENV.contains(&"CLAUDE_CONFIG_DIR"));
    for forbidden in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "NODE_OPTIONS",
    ] {
        assert!(
            !env_names.contains(&forbidden),
            "inherited {forbidden} blocked"
        );
    }
}

#[test]
fn auth_status_command_is_a_bounded_supported_status_invocation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let working_directory = dir.path().join("empty-workspace");
    std::fs::create_dir(&working_directory).expect("private working directory");
    let command = auth_status_command(LocalCommandConfig {
        executable: Path::new("/bin/true"),
        child_path: OsStr::new("/usr/bin:/bin"),
        home: dir.path(),
        config_home: dir.path(),
        cache_home: dir.path(),
        temp: dir.path(),
        working_directory: &working_directory,
    });
    let args = command
        .get_args()
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>();
    assert_eq!(
        args,
        [
            "--setting-sources",
            "project",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "auth",
            "status",
            "--json"
        ]
    );
    assert_eq!(command.get_current_dir(), Some(working_directory.as_path()));
    let env_names = command
        .get_envs()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for forbidden in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "NODE_OPTIONS",
    ] {
        assert!(!env_names.iter().any(|name| name == forbidden));
    }
    assert!(command.get_envs().any(|(key, value)| {
        key == OsStr::new("CLAUDE_CODE_DISABLE_CLAUDE_MDS") && value == Some(OsStr::new("1"))
    }));
}
