use crate::app::ServerState;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::Duration,
};

#[derive(Default)]
pub(crate) struct AeroRuntime(Mutex<Option<Running>>);

struct Running {
    process: Child,
    url: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl AeroRuntime {
    pub(crate) fn endpoint(&self, state: &ServerState, config: &Value) -> Result<String, String> {
        let mut running = self.0.lock().map_err(|_| "aero.runtime_unavailable")?;
        let root = state
            .backend
            .configuration
            .config_path
            .parent()
            .expect("configuration directory")
            .join("aero");
        let profiles_path = root.join("profiles.json");
        let mut profiles = BTreeMap::new();
        for profile in config["aero_profiles"].as_array().into_iter().flatten() {
            let roles = json!({"planner":profile["planner"],"executor":profile["executor"]});
            profiles.insert(super::route_name(profile), roles.clone());
            for alias in profile["route_names"].as_array().into_iter().flatten() {
                profiles.insert(
                    alias.as_str().expect("normalized route").to_owned(),
                    roles.clone(),
                );
            }
        }
        emp_state::atomic_write_private_state(&profiles_path, &serde_json::to_vec(&json!({
            "upstream":format!("http://127.0.0.1:{}/v1",state.port),"review_interval":30,"models":profiles,
        })).expect("aero configuration JSON")).map_err(|_| "aero.runtime_unavailable")?;
        if let Some(child) = running.as_mut()
            && child
                .process
                .try_wait()
                .map_err(|_| "aero.runtime_unavailable")?
                .is_none()
        {
            return Ok(child.url.clone());
        }
        running.take();
        let executable = std::env::current_exe().map_err(|_| "aero.runtime_unavailable")?;
        let bundled = executable
            .parent()
            .expect("executable directory")
            .join("aero")
            .join(if cfg!(windows) {
                "aero-server.exe"
            } else {
                "aero-server"
            });
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../experiments/aero/gateway.py");
        let mut command = if bundled.is_file() {
            Command::new(bundled)
        } else if script.is_file() {
            let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
            command.arg("-B").arg(script);
            command
        } else {
            return Err("aero.runtime_missing".into());
        };
        command
            .args(["--config"])
            .arg(&profiles_path)
            .arg("--runtime")
            .arg(&root)
            .args(["--port", "0", "--managed"])
            .env("EMP_GATEWAY_KEY", &state.backend.gateway_key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = Running {
            process: command.spawn().map_err(|_| "aero.runtime_missing")?,
            url: String::new(),
        };
        let stdout = child.process.stdout.take().expect("piped aero startup");
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = sender.send(result);
        });
        let startup = receiver.recv_timeout(Duration::from_secs(20));
        let url = match startup {
            Ok(Ok(line)) => serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|value| value["url"].as_str().map(str::to_owned)),
            _ => None,
        };
        let Some(url) = url else {
            drop(child);
            let _ = reader.join();
            return Err("aero.runtime_unavailable".into());
        };
        let _ = reader.join();
        child.url = url.clone();
        *running = Some(child);
        Ok(url)
    }

    pub(crate) fn stop(&self) {
        if let Ok(mut running) = self.0.lock() {
            running.take();
        }
    }
}
