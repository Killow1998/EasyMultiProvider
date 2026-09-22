//! Desktop configuration paths and a non-shell browser launcher.
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
fn variable(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_owned())
}
pub(super) fn config_path() -> PathBuf {
    let home = if cfg!(windows) {
        variable("USERPROFILE").or_else(|| variable("HOME"))
    } else {
        variable("HOME").or_else(|| variable("USERPROFILE"))
    }
    .map(PathBuf::from)
    .unwrap_or_default();
    if cfg!(windows) {
        return variable("LOCALAPPDATA")
            .or_else(|| variable("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"))
            .join("EasyMultiProvider/config.json");
    }
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/EasyMultiProvider/config.json");
    }
    variable("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("easy-multi-provider/config.json")
}
fn launch(arguments: &[String]) -> bool {
    let Some((program, arguments)) = arguments.split_first() else {
        return false;
    };
    let Ok(mut child) = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => return false,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            // The browser belongs to the desktop, not EMP's service lifetime.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
pub(crate) fn open_browser(url: &str) -> bool {
    if let Some(configured) = variable("BROWSER") {
        for command in configured.split(if cfg!(windows) { ';' } else { ':' }) {
            let Some(mut arguments) = shlex::split(command) else {
                continue;
            };
            if arguments.iter().any(|arg| arg.contains("%s")) {
                for arg in &mut arguments {
                    *arg = arg.replace("%s", url);
                }
            } else {
                arguments.push(url.to_owned());
            }
            if launch(&arguments) {
                return true;
            }
        }
    }
    let arguments = if cfg!(windows) {
        vec![
            "rundll32".to_owned(),
            "url.dll,FileProtocolHandler".to_owned(),
            url.to_owned(),
        ]
    } else if cfg!(target_os = "macos") {
        vec!["open".to_owned(), url.to_owned()]
    } else {
        vec!["xdg-open".to_owned(), url.to_owned()]
    };
    launch(&arguments)
}
