//! Pump tray-icon's native window on the same thread that created it.
pub(super) struct Events;

impl Events {
    pub(super) fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self)
    }

    pub(super) fn pump(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        // SAFETY: the message buffer is writable and all dispatch stays on the
        // owning thread; tray-icon owns the hidden window and its callbacks.
        unsafe {
            let mut message = std::mem::zeroed();
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
