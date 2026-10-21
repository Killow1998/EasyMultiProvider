#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg_attr(feature = "hotpath", hotpath::main)]
fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    if std::env::args_os().nth(1).is_some() {
        // Desktop starts must not initialize a console before main. Explicit
        // CLI commands can reuse their parent's console; inherited redirected
        // standard handles remain available even when no console is attached.
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
