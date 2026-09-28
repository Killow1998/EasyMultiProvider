//! Non-shell browser launcher for desktop starts.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
fn variable(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_owned())
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
/// Absolute rundll32 path. A bare name would be searched in the application
/// directory first, letting a planted `rundll32.exe` beside EMP run instead.
fn windows_rundll32() -> String {
    windows_system_directory()
        .join("rundll32.exe")
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn windows_system_directory() -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }
    let mut buffer = [0_u16; 1024];
    // SAFETY: the buffer is valid for `buffer.len()` UTF-16 units and the API
    // writes at most that many, returning the length without the terminator.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    let length = length as usize;
    if length > 0 && length < buffer.len() {
        return std::ffi::OsString::from_wide(&buffer[..length]).into();
    }
    system_directory_from_root(std::env::var_os("SystemRoot"))
}

#[cfg(not(windows))]
fn windows_system_directory() -> std::path::PathBuf {
    system_directory_from_root(std::env::var_os("SystemRoot"))
}

fn system_directory_from_root(root: Option<std::ffi::OsString>) -> std::path::PathBuf {
    let root = root
        .map(std::path::PathBuf::from)
        .filter(|root| {
            let value = root.as_os_str().to_string_lossy();
            let bytes = value.as_bytes();
            root.is_absolute()
                || (bytes.len() >= 3
                    && bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                    && matches!(bytes[2], b'\\' | b'/'))
                || value.starts_with(r"\\")
        })
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    root.join("System32")
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
            windows_rundll32(),
            "url.dll,FileProtocolHandler".to_owned(),
            url.to_owned(),
        ]
    } else if cfg!(target_os = "macos") {
        vec!["/usr/bin/open".to_owned(), url.to_owned()]
    } else {
        vec![xdg_open_path().to_owned(), url.to_owned()]
    };
    launch(&arguments)
}

fn xdg_open_path() -> &'static str {
    for path in ["/usr/bin/xdg-open", "/usr/local/bin/xdg-open"] {
        if std::path::Path::new(path).is_file() {
            return path;
        }
    }
    "/usr/bin/xdg-open"
}

#[cfg(test)]
mod tests {
    use super::system_directory_from_root;

    #[test]
    fn system_directory_falls_back_to_the_default_windows_root() {
        let fallback = system_directory_from_root(None);
        assert!(fallback.starts_with(r"C:\Windows"));
        assert!(fallback.ends_with("System32"));
        let relative = system_directory_from_root(Some("Windows".into()));
        assert!(relative.starts_with(r"C:\Windows"));
        let drive_relative = system_directory_from_root(Some(r"D:relative".into()));
        assert!(drive_relative.starts_with(r"C:\Windows"));
        let configured = system_directory_from_root(Some(r"D:\Win".into()));
        assert!(configured.starts_with(r"D:\Win"));
    }
}
