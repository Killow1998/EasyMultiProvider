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
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let Ok(mut child) = command
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

/// Keep failures visible when desktop launchers do not allocate a terminal.
pub(crate) fn show_error(title: &str, message: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        // SAFETY: both strings are terminated and live through the modal call.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        launch(&[
            "/usr/bin/osascript".to_owned(), "-e".to_owned(),
            "on run argv\ndisplay alert (item 1 of argv) message (item 2 of argv) as critical\nend run".to_owned(),
            title.to_owned(), message.to_owned(),
        ]);
    }
    #[cfg(target_os = "linux")]
    {
        if std::path::Path::new("/usr/bin/zenity").is_file() {
            launch(&[
                "/usr/bin/zenity".to_owned(),
                "--error".to_owned(),
                "--no-markup".to_owned(),
                "--title".to_owned(),
                title.to_owned(),
                "--text".to_owned(),
                message.to_owned(),
            ]);
        } else if std::path::Path::new("/usr/bin/notify-send").is_file() {
            launch(&[
                "/usr/bin/notify-send".to_owned(),
                "--".to_owned(),
                title.to_owned(),
                message.to_owned(),
            ]);
        } else {
            eprintln!("{title}: {message}");
        }
    }
}

/// Nonfatal tray feedback must not keep the action worker or shutdown waiting.
pub(crate) fn notify_error(title: &str, message: &str) {
    let (title, message) = (title.to_owned(), message.to_owned());
    let _ = std::thread::Builder::new()
        .name("emp-desktop-notice".to_owned())
        .spawn(move || show_error(&title, &message));
}

#[cfg(target_os = "linux")]
pub(crate) fn detach_service() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["serve", "--open-browser"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe. Only the new child loses the
    // launcher's terminal; an existing shell and other EMP processes stay put.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn hide_owned_console() {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
    let mut processes = [0_u32; 2];
    // SAFETY: only hide a console owned solely by this EMP process. An
    // existing PowerShell/CMD window belongs to its user and stays visible.
    unsafe {
        if GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) == 1 {
            let window = GetConsoleWindow();
            if !window.is_null() {
                ShowWindow(window, SW_HIDE);
            }
        }
    }
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

    #[cfg(windows)]
    #[test]
    fn configured_browser_helpers_never_allocate_a_console() {
        let root = tempfile::tempdir().unwrap();
        let receipt = root.path().join("console.txt");
        let script = format!(
            "Add-Type 'using System; using System.Runtime.InteropServices; public class Probe {{ [DllImport(\"kernel32.dll\")] public static extern IntPtr GetConsoleWindow(); }}'; [IO.File]::WriteAllText('{}', ([Probe]::GetConsoleWindow() -eq [IntPtr]::Zero).ToString())",
            receipt.display().to_string().replace('\'', "''")
        );
        assert!(super::launch(&[
            super::windows_system_directory()
                .join("WindowsPowerShell/v1.0/powershell.exe")
                .to_string_lossy()
                .into_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script,
        ]));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !receipt.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(receipt).unwrap(), "True");
    }
}
