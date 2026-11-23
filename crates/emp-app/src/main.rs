#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg_attr(feature = "hotpath", hotpath::main)]
fn main() -> std::process::ExitCode {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // Set before starting threads so large request buffers can be unmapped on free.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 1024 * 1024);
    }
    #[cfg(windows)]
    if std::env::args_os().nth(1).is_some() {
        // Keep desktop starts console-free before main; explicit CLI commands may
        // reuse the parent's console, preserving inherited redirected standard handles.
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
