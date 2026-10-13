//! Official CLI invocation in a disposable workspace. Never import CLI tools,
//! skills, hooks or MCP servers into a client-owned tool execution flow.
use super::{Error, profile, tools};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tempfile::TempDir;

pub(super) struct Invocation {
    temp: TempDir,
    home: PathBuf,
    executable: PathBuf,
    agent: String,
}

impl Invocation {
    pub(super) fn new(instructions: &str) -> Result<Self, Error> {
        let executable =
            emp_codex::installed_agy::resolve_agy_cli().ok_or(Error("agy_cli_unavailable"))?;
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_dir())
            .ok_or(Error("agy_cli_home_unavailable"))?;
        profile::validate_home(&home)?;
        let temp = tempfile::tempdir().map_err(|_| Error("agy_cli_temp_unavailable"))?;
        let agent = format!(
            "emp-{}",
            crate::util::random_hex(12).map_err(|_| Error("agy_cli_temp_unavailable"))?
        );
        let agents = temp.path().join("workspace/.agents/agents");
        std::fs::create_dir_all(&agents).map_err(|_| Error("agy_cli_temp_unavailable"))?;
        let content = format!(
            "---\nname: {agent}\ndescription: EMP client-owned tool projection.\ntools: [finish]\nmainAgent: true\nsubagent: false\nmodel: inherit\ncommandExecutionPolicy: off\nskills: []\nplugins: []\nmcpServers: []\n---\n{instructions}\n"
        );
        std::fs::write(agents.join(format!("{agent}.md")), content)
            .and_then(|()| {
                std::fs::write(temp.path().join("schema.json"), tools::schema().to_string())
            })
            .map_err(|_| Error("agy_cli_temp_unavailable"))?;
        Ok(Self {
            temp,
            home,
            executable,
            agent,
        })
    }

    fn base(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", self.temp.path())
            .env("TMP", self.temp.path())
            .env("TEMP", self.temp.path())
            .env("LANG", "C.UTF-8")
            .env("TERM", "dumb")
            .current_dir(self.temp.path().join("workspace"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Authentication belongs to the official profile; no API key, account
        // override, remote-control session or parent agy conversation is inherited.
        for name in [
            "SystemRoot",
            "WINDIR",
            "APPDATA",
            "LOCALAPPDATA",
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
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        command
    }

    pub(super) fn inference(&self, model: &str, effort: Option<&str>) -> Command {
        let mut command = self.base();
        command
            .args([
                "--agent",
                &self.agent,
                "--model",
                model,
                "--project",
                "default-cli-project",
                "--disable-slash-commands",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--print-timeout",
                "300s",
                "--json-schema",
            ])
            .arg(self.temp.path().join("schema.json"))
            .arg("--log-file")
            .arg(self.temp.path().join("cli.log"));
        if let Some(effort) = effort {
            command.args(["--effort", effort]);
        }
        command
    }

    pub(super) fn models(&self) -> Command {
        let mut command = self.base();
        command.arg("models");
        command
    }
}
