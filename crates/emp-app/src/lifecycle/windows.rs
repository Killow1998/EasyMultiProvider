//! In-process Windows session notifications. No helper process or later repair.
//!
//! Loading user32 means a console handler alone cannot receive logoff/shutdown.
//! A hidden top-level window receives WM_ENDSESSION; a message-only window
//! would miss the broadcast. Cleanup finishes before returning that message.
use crate::app::ServerState;
use std::io;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const STOP_EVENTS: u32 = WM_APP + 1;
const CLASS: *const u16 = w!("EMP.SessionEvents");

struct ExitState {
    state: Arc<ServerState>,
    external: Arc<AtomicBool>,
}

pub(super) struct SessionEvents {
    window: usize,
    worker: Option<JoinHandle<()>>,
}

impl SessionEvents {
    pub(super) fn new(state: Arc<ServerState>, external: Arc<AtomicBool>) -> io::Result<Self> {
        let state = Arc::new(ExitState { state, external });
        let (ready, received) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("emp-session-events".to_owned())
            .spawn(move || {
                // SAFETY: this thread owns the class/window and keeps state
                // alive until DestroyWindow has finished all callbacks.
                unsafe {
                    let instance = GetModuleHandleW(null_mut());
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(window_proc),
                        hInstance: instance,
                        lpszClassName: CLASS,
                        ..std::mem::zeroed()
                    };
                    if RegisterClassW(&class) == 0 {
                        let _ = ready.send(Err(io::Error::last_os_error()));
                        return;
                    }
                    let window = CreateWindowExW(
                        0,
                        CLASS,
                        w!("EMP shutdown notifications"),
                        0,
                        0,
                        0,
                        0,
                        0,
                        null_mut(),
                        null_mut(),
                        instance,
                        Arc::as_ptr(&state).cast(),
                    );
                    if window.is_null() {
                        let _ = ready.send(Err(io::Error::last_os_error()));
                    } else if ready.send(Ok(window as usize)).is_ok() {
                        let mut message = std::mem::zeroed();
                        loop {
                            let result = GetMessageW(&mut message, null_mut(), 0, 0);
                            if result <= 0 {
                                if result < 0 {
                                    restore_before_exit(&state);
                                }
                                break;
                            }
                            TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                    if !window.is_null() {
                        DestroyWindow(window);
                    }
                    UnregisterClassW(CLASS, instance);
                }
            })?;
        let window = received
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| io::Error::other("Windows exit notifications did not become ready"))??;
        Ok(Self {
            window,
            worker: Some(worker),
        })
    }
}

impl Drop for SessionEvents {
    fn drop(&mut self) {
        // SAFETY: the message targets our window; destruction stays on its
        // owning thread. A canceled shutdown never posts this message.
        unsafe {
            PostMessageW(self.window as HWND, STOP_EVENTS, 0, 0);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn restore_before_exit(exit: &ExitState) {
    // No Rust unwind may cross a Win32 callback boundary.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        exit.external.store(true, Ordering::Release);
        let state = &exit.state;
        let result = super::shutdown::prepare_exit(state, false);
        state.backend.diagnostics.journal.event(
            if result.is_ok() { "info" } else { "error" },
            "session_exit_restore",
            &serde_json::json!({"success": result.is_ok()}),
        );
    }));
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: Windows supplies the CREATESTRUCT during WM_NCCREATE. All later
    // callbacks run on this thread while the owning Arc in new() remains live.
    unsafe {
        if message == WM_NCCREATE {
            let creation = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, creation.lpCreateParams as isize);
        }
        match message {
            // Query is not a commitment: another app or the user can cancel.
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION if wparam == 0 => 0,
            WM_ENDSESSION | WM_CLOSE => {
                let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *const ExitState;
                if let Some(state) = state.as_ref() {
                    restore_before_exit(state);
                }
                0
            }
            STOP_EVENTS => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}
