use super::*;

impl Context {
    pub(in crate::services::claude_cli::tui) fn prepare(
        state: &ServerState,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
        executable: &emp_codex::installed_cli::InstalledClaudeCli,
        cancelled: Arc<Cancellation>,
    ) -> Result<Self, ClaudeCliError> {
        let root = tempfile::Builder::new()
            .prefix("emp-claude-turn-")
            .tempdir()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
        let config = root.path().join("config");
        let project = root.path().join("project");
        let home = root.path().join("home");
        for path in [&config, &project, &home] {
            std::fs::create_dir_all(path)
                .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
        }
        #[cfg(unix)]
        let project = std::fs::canonicalize(project)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
        std::fs::write(config.join(".claude.json"),json!({"hasCompletedOnboarding":true,"theme":"dark","autoUpdates":false,
            "projects":{project.to_string_lossy().as_ref():{"hasTrustDialogAccepted":true,"enableAllProjectMcpServers":true,"hasCompletedProjectOnboarding":true}}}).to_string())
            .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
        let prepared = media::prepare(body).map_err(ClaudeCliError::Failure)?;
        let prompt = root.path().join("system.txt");
        std::fs::write(&prompt, &prepared.system_prompt)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
        let attachments = root.path().join("attachments.jsonl");
        let local = route.provider.value()["auth_mode"] == "claude_login";
        let context = super::super::attachments::prepare(
            prepared.expected_user_content,
            local,
            &attachments,
        )?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?;
        let port = listener
            .local_addr()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?
            .port();
        let token = crate::util::random_hex(32)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?;
        let mcp = root.path().join("mcp.json");
        std::fs::write(&mcp,json!({"mcpServers":{"emp_codex":{"type":"http","alwaysLoad":true,"url":format!("http://127.0.0.1:{port}/mcp"),"headers":{"Authorization":format!("Bearer {token}")}}}}).to_string())
            .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
        let raw = crate::util::random_hex(16)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_id_generation_failed"))?;
        let session = format!(
            "{}-{}-4{}-8{}-{}",
            &raw[..8],
            &raw[8..12],
            &raw[13..16],
            &raw[17..20],
            &raw[20..]
        );
        let done = root.path().join("complete");
        let emp = std::env::current_exe()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_spawn_failed"))?;
        let quote = |path: &std::path::Path| {
            if cfg!(windows) {
                format!("\"{}\"", path.display())
            } else {
                format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
            }
        };
        let hook = format!("{} --emp-claude-complete {}", quote(&emp), quote(&done));
        let settings =
            json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":hook}]}]}}).to_string();
        let mut command = CommandBuilder::new(&executable.executable);
        command.env_clear();
        command.cwd(&project);
        for (key, value) in [
            ("PATH", executable.child_path.clone()),
            ("HOME", home.clone().into_os_string()),
            ("USERPROFILE", home.into_os_string()),
            ("CLAUDE_CONFIG_DIR", config.clone().into_os_string()),
            ("XDG_CONFIG_HOME", config.clone().into_os_string()),
            ("XDG_CACHE_HOME", root.path().join("cache").into_os_string()),
            ("TMPDIR", root.path().as_os_str().to_owned()),
            ("TMP", root.path().as_os_str().to_owned()),
            ("TEMP", root.path().as_os_str().to_owned()),
        ] {
            command.env(key, value);
        }
        for (key, value) in [
            ("TERM", "xterm-256color"),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("DISABLE_AUTOUPDATER", "1"),
            ("DISABLE_COMPACT", "1"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("CLAUDE_CODE_DISABLE_TELEMETRY", "1"),
            ("CLAUDE_CODE_DISABLE_ERROR_REPORTING", "1"),
            ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
        ] {
            command.env(key, value);
        }
        #[cfg(windows)]
        for key in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if local {
            let home = super::super::super::process::host_cli_home()
                .ok_or(ClaudeCliError::Failure("claude_cli_local_home_unavailable"))?;
            let mut monitor = None;
            let owner = super::super::super::verify_local_subscription(
                state,
                executable,
                &home,
                &config,
                &root.path().join("cache"),
                root.path(),
                &project,
                &cancelled,
                &mut monitor,
            )?;
            if let Some(owner) = owner {
                state.backend.claude_quota.identify(owner);
            } else {
                state.backend.claude_quota.begin(None);
            }
            let original = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude"));
            let credentials = original.join(".credentials.json");
            if credentials.is_file() {
                let bytes = emp_state::read_file_limited(&credentials, 1024 * 1024)
                    .map_err(|_| ClaudeCliError::Failure("claude_cli_login_required"))?;
                emp_state::write_new_private_file(&config.join(".credentials.json"), &bytes)
                    .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
            }
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "NO_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
                "no_proxy",
                "SSL_CERT_FILE",
                "SSL_CERT_DIR",
                "NODE_EXTRA_CA_CERTS",
            ] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
        } else {
            command.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"));
            command.env("ANTHROPIC_AUTH_TOKEN", &token);
        }
        command.args(["--tools", "", "--strict-mcp-config", "--mcp-config"]);
        command.arg(mcp);
        command.args([
            "--allowedTools",
            "mcp__emp_codex__codex",
            "--permission-mode",
            "default",
            "--setting-sources",
            "project",
            "--settings",
            &settings,
            "--no-chrome",
            "--session-id",
            &session,
            "--model",
            &route.upstream_model,
            "--system-prompt-file",
        ]);
        command.arg(prompt);
        if let Some(effort) = projection::effort_for_model(body, route.model.value())
            .map_err(ClaudeCliError::Failure)?
        {
            command.args(["--effort", effort]);
        }
        if let Some(limit) = body["max_output_tokens"]
            .as_u64()
            .or_else(|| super::super::super::configured_limit(route, "output_limit"))
        {
            command.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", limit.to_string());
        }
        Ok(Self {
            _root: root,
            command,
            listener,
            token,
            context: Some(context),
            attachments,
            config,
            session,
            done,
            local,
            route: route.clone(),
            incoming: incoming.clone(),
            client: Arc::clone(&state.backend.transport.client),
            runtime: state.backend.transport.runtime.handle().clone(),
            shutdown: Arc::clone(&state.shutdown),
            cancelled,
        })
    }
}
