#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg_attr(feature = "hotpath", hotpath::main)]
fn main() -> std::process::ExitCode {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt takes integer options and runs before application threads start.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 1024 * 1024);
    }
    #[cfg(windows)]
    if std::env::args_os().nth(1).is_some() {
        // SAFETY: ATTACH_PARENT_PROCESS is a valid console selector and the call takes no pointers.
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(
                windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
    }
    match emp_app::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
