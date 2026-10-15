use super::*;
use std::os::windows::process::CommandExt;
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole, GetConsoleWindow};
use windows_sys::Win32::System::Threading::{CREATE_NEW_CONSOLE, CREATE_NO_WINDOW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// The tests attach only their own isolated console child, never the desktop
// session. Serialize AttachConsole, which is process-wide in the test runner.
static CONSOLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn start(workspace: &Workspace, flags: u32) -> Emp {
    let mut command = workspace.command();
    command
        .args(workspace.serve_arguments())
        .creation_flags(flags);
    Emp::spawn(command)
}

fn session_window(pid: i32) -> HWND {
    struct Search {
        pid: u32,
        window: HWND,
    }
    unsafe extern "system" fn find(window: HWND, data: LPARAM) -> i32 {
        // SAFETY: EnumWindows is synchronous and data points to our Search.
        unsafe {
            let search = &mut *(data as *mut Search);
            let mut pid = 0;
            GetWindowThreadProcessId(window, &mut pid);
            let mut name = [0u16; 64];
            let len = GetClassNameW(window, name.as_mut_ptr(), name.len() as i32);
            if pid == search.pid
                && String::from_utf16_lossy(&name[..len.max(0) as usize]) == "EMP.SessionEvents"
            {
                search.window = window;
                return 0;
            }
        }
        1
    }
    let mut search = Search {
        pid: pid as u32,
        window: std::ptr::null_mut(),
    };
    // SAFETY: callbacks cannot outlive this stack value.
    unsafe {
        EnumWindows(Some(find), &mut search as *mut Search as LPARAM);
    }
    assert!(
        !search.window.is_null(),
        "EMP must register session notifications before readiness"
    );
    search.window
}

#[test]
fn closing_the_console_restores_before_windows_terminates_emp() {
    let _serial = CONSOLE.lock().unwrap();
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let mut emp = start(&workspace, CREATE_NEW_CONSOLE);
    enable(&emp, &workspace);
    // SAFETY: attach only to this test's child and detach before closing its
    // console, so the test runner does not receive CTRL_CLOSE_EVENT itself.
    let window = unsafe {
        FreeConsole();
        assert_ne!(AttachConsole(emp.pid() as u32), 0);
        let window = GetConsoleWindow();
        assert_ne!(FreeConsole(), 0);
        window
    };
    assert!(!window.is_null());
    // Actual console close, not an invocation of EMP's cleanup function.
    assert_ne!(unsafe { PostMessageW(window, WM_CLOSE, 0, 0) }, 0);
    await_native(&workspace);
    let _ = emp.wait_exit();
    assert_native(&workspace);
}

#[test]
fn canceled_session_end_keeps_running_and_confirmed_end_restores_synchronously() {
    let _serial = CONSOLE.lock().unwrap();
    for (reason, flags) in [
        (0, CREATE_NEW_CONSOLE),
        (ENDSESSION_LOGOFF as isize, CREATE_NO_WINDOW),
    ] {
        let upstream = Upstream::start();
        let workspace = Workspace::with_routes(&upstream);
        let mut emp = start(&workspace, flags);
        enable(&emp, &workspace);
        let window = session_window(emp.pid());
        let applied = workspace.read_codex_config();
        // Send only to our fixture window. No real logoff/shutdown is requested.
        unsafe {
            assert_eq!(SendMessageW(window, WM_QUERYENDSESSION, 0, reason), 1);
            SendMessageW(window, WM_ENDSESSION, 0, reason);
        }
        assert_eq!(workspace.read_codex_config(), applied);
        assert_eq!(emp.anonymous("GET", "/healthz").status, 200);
        unsafe {
            SendMessageW(window, WM_ENDSESSION, 1, reason);
        }
        // Restoration must finish before the session-end callback returns.
        assert_native(&workspace);
        assert_eq!(emp.wait_exit(), Some(0));
    }
}

#[test]
fn close_window_request_restores_native() {
    let _serial = CONSOLE.lock().unwrap();
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let mut emp = start(&workspace, CREATE_NEW_CONSOLE);
    enable(&emp, &workspace);
    let window = session_window(emp.pid());
    unsafe {
        SendMessageW(window, WM_CLOSE, 0, 0);
    }
    assert_native(&workspace);
    assert_eq!(emp.wait_exit(), Some(0));
}
