//! Keep the old application's process handle open throughout an update.
use crate::Result;
use crate::package::{KillOnDrop, http};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) struct Parent {
    process: KillOnDrop,
    session: String,
    port: u16,
    pub(super) identity: serde_json::Value,
}

pub(super) fn environment(command: &mut Command, root: &Path, config: &Path) {
    command
        .current_dir(root)
        .env("CODEX_HOME", root.join("codex-home"))
        .env("EASY_MULTI_PROVIDER_CONFIG", config)
        .env("EASY_MULTI_PROVIDER_MASTER_KEY", "")
        .env(
            "EASY_MULTI_PROVIDER_MASTER_KEY_FILE",
            root.join("master.key"),
        )
        .env_remove("EMP_UPDATE_READY")
        .env_remove("EMP_UPDATE_RESULT")
        .stdin(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(command, 0x0800_0000);
}

impl Parent {
    pub(super) fn start(binary: &Path, root: &Path, config: &Path, port: u16) -> Result<Self> {
        let log_path = root.join("old-emp-output.txt");
        let log = fs::File::create(&log_path).map_err(|error| error.to_string())?;
        let mut command = Command::new(binary);
        environment(&mut command, root, config);
        command
            .args(["serve", "--config"])
            .arg(config)
            .args(["--port", &port.to_string()])
            .stdout(log.try_clone().map_err(|error| error.to_string())?)
            .stderr(log);
        let mut process = KillOnDrop(command.spawn().map_err(|error| error.to_string())?);
        let pid = process.0.id();
        let created = emp_state::update::created(pid).ok_or("old EMP birth time unavailable")?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let bootstrap = loop {
            if process
                .0
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err("old EMP exited before the update".into());
            }
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            if let Some(token) = log
                .lines()
                .find_map(|line| line.strip_prefix("Open in browser: "))
                .and_then(|url| url.split_once("bootstrap=").map(|(_, token)| token.trim()))
            {
                break token.to_owned();
            }
            if Instant::now() >= deadline {
                return Err("old EMP did not start".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let (status, body) = http(
            port,
            "POST",
            "/api/session",
            &[("X-EMP-Bootstrap", &bootstrap)],
        )?;
        let session = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["session"].as_str().map(str::to_owned))
            .filter(|_| status == 200)
            .ok_or("old EMP session unavailable")?;
        Ok(Self {
            process,
            session,
            port,
            identity: serde_json::json!({"pid":pid,"created":created}),
        })
    }

    pub(super) fn quit(&mut self) -> Result {
        let (status, _) = http(
            self.port,
            "POST",
            "/api/quit",
            &[("X-EMP-Session", &self.session)],
        )?;
        if status != 200 {
            return Err(format!("old EMP quit returned {status}"));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self
                .process
                .0
                .try_wait()
                .map_err(|error| error.to_string())?
            {
                if !status.success() {
                    return Err(format!("old EMP exited with {status}"));
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err("old EMP did not exit".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Retain the Child handle; Windows can still open this process object.
        // The updater must recognize it as exited without waiting for us to drop it.
        Ok(())
    }
}
