//! Pump tray-icon's native window on the same thread that created it.
pub(super) struct Events;

pub(super) fn notify_budget(
    window: windows_sys::Win32::Foundation::HWND,
    guid: u128,
    title: &str,
    message: &str,
) {
    use windows_sys::Win32::UI::Shell::*;
    let mut notification = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uFlags: NIF_GUID | NIF_INFO,
        dwInfoFlags: NIIF_INFO | NIIF_NOSOUND,
        guidItem: windows_sys::core::GUID::from_u128(guid),
        ..Default::default()
    };
    for (target, source) in notification.szInfoTitle[..63]
        .iter_mut()
        .zip(title.encode_utf16())
    {
        *target = source;
    }
    for (target, source) in notification.szInfo[..255]
        .iter_mut()
        .zip(message.encode_utf16())
    {
        *target = source;
    }
    // SAFETY: notification is initialized with its ABI size and NUL-terminated text and lives through the call.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &notification) };
}

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
