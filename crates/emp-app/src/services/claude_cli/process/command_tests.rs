use super::*;

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
