//! Desktop-only entry point; the service and WebUI share the same lifecycle commands.
mod actions;
mod copy;
mod icon;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(any(windows, target_os = "macos"))]
#[path = "native.rs"]
mod platform;

use crate::app::ServerState;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Open,
    Enable,
    Restore,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Emp,
    Native,
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    english: bool,
    mode: Mode,
    busy: bool,
}

impl Snapshot {
    fn copy(self) -> &'static copy::Copy {
        copy::select(self.english)
    }

    fn toggle(self) -> Option<Action> {
        if self.busy {
            return None;
        }
        match self.mode {
            Mode::Emp => Some(Action::Restore),
            Mode::Native => Some(Action::Enable),
            Mode::Unresolved => None,
        }
    }

    fn toggle_label(self) -> &'static str {
        if self.mode == Mode::Emp {
            self.copy().restore
        } else {
            self.copy().enable
        }
    }

    fn tooltip(self) -> &'static str {
        let copy = self.copy();
        if self.busy {
            return copy.busy;
        }
        match self.mode {
            Mode::Emp => copy.active,
            Mode::Native => copy.native,
            Mode::Unresolved => copy.unresolved,
        }
    }
}

pub(crate) fn available() -> bool {
    cfg!(any(windows, target_os = "macos"))
        || (std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
            && (std::env::var_os("DISPLAY").is_some()
                || std::env::var_os("WAYLAND_DISPLAY").is_some()))
}

pub(crate) fn startup_error(error: &str, english: bool) {
    crate::cli::desktop::show_error(copy::select(english).startup_failed, error);
}

pub(crate) struct Recovery {
    tray: platform::Tray,
    actions: mpsc::Receiver<Action>,
    url: String,
    english: bool,
}

impl Recovery {
    pub(crate) fn new(url: String, english: bool) -> Option<Self> {
        if !available() {
            return None;
        }
        let (sender, actions) = mpsc::sync_channel(4);
        let snapshot = Snapshot {
            english,
            mode: Mode::Unresolved,
            busy: false,
        };
        let tray = match platform::Tray::new(snapshot, sender) {
            Ok(tray) => tray,
            Err(_) => {
                let copy = snapshot.copy();
                crate::cli::desktop::notify_error(copy.failure_title, copy.tray_unavailable);
                return None;
            }
        };
        #[cfg(windows)]
        crate::cli::desktop::hide_owned_console();
        Some(Self {
            tray,
            actions,
            url,
            english,
        })
    }

    pub(crate) fn poll(&mut self) -> bool {
        self.tray.pump();
        while let Ok(action) = self.actions.try_recv() {
            match action {
                Action::Open => {
                    if !crate::cli::desktop::open_browser(&self.url) {
                        let copy = copy::select(self.english);
                        crate::cli::desktop::notify_error(copy.failure_title, copy.browser_failed);
                    }
                }
                Action::Quit => return false,
                Action::Enable | Action::Restore => {}
            }
        }
        true
    }
}

/// Run the native event loop on the launch thread, as required by AppKit.
/// Headless `serve` never enters this function. A failed tray retains the console.
pub(crate) fn wait(state: Arc<ServerState>) -> bool {
    if !available() {
        return false;
    }
    let initial = actions::snapshot(&state, false);
    let (send, receive) = mpsc::sync_channel(4);
    let mut tray = match platform::Tray::new(initial, send) {
        Ok(tray) => tray,
        Err(error) => {
            eprintln!("EMP system tray is unavailable: {error}");
            crate::cli::desktop::notify_error(
                initial.copy().failure_title,
                initial.copy().tray_unavailable,
            );
            state.backend.diagnostics.journal.event(
                "warning",
                "desktop_tray_unavailable",
                &serde_json::json!({}),
            );
            return false;
        }
    };
    let (updates, received) = mpsc::sync_channel(8);
    let worker_state = Arc::clone(&state);
    let worker = match std::thread::Builder::new()
        .name("emp-tray-actions".to_owned())
        .spawn(move || actions::run(worker_state, receive, updates))
    {
        Ok(worker) => worker,
        Err(error) => {
            eprintln!("EMP system tray worker could not start: {error}");
            crate::cli::desktop::notify_error(
                initial.copy().failure_title,
                initial.copy().tray_unavailable,
            );
            return false;
        }
    };
    state
        .backend
        .diagnostics
        .journal
        .event("info", "desktop_tray_started", &serde_json::json!({}));
    #[cfg(windows)]
    crate::cli::desktop::hide_owned_console();
    let mut current = initial;
    while !state.shutdown.load(Ordering::Acquire) {
        tray.pump();
        let mut next = current;
        while let Ok(update) = received.try_recv() {
            next = update;
        }
        if current != next {
            tray.update(next);
            current = next;
        }
    }
    drop(tray);
    let _ = worker.join();
    true
}
