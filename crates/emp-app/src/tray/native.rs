//! Shared native menu for Windows and macOS; each keeps its OS event loop.
use super::{Action, Snapshot, icon};
use std::sync::mpsc::SyncSender;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[cfg(windows)]
#[path = "windows.rs"]
mod events;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod events;

pub(super) struct Tray {
    icon: TrayIcon,
    open: MenuItem,
    toggle: MenuItem,
    quit: MenuItem,
    snapshot: Snapshot,
    actions: SyncSender<Action>,
    events: events::Events,
}

impl Tray {
    pub(super) fn new(
        snapshot: Snapshot,
        actions: SyncSender<Action>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        #[cfg(target_os = "macos")]
        {
            objc2::rc::autoreleasepool(|_| Self::build(snapshot, actions))
        }
        #[cfg(windows)]
        {
            Self::build(snapshot, actions)
        }
    }

    fn build(
        snapshot: Snapshot,
        actions: SyncSender<Action>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut events = events::Events::new()?;
        events.pump();
        let copy = snapshot.copy();
        let open = MenuItem::with_id("emp-open", copy.open, true, None);
        let toggle = MenuItem::with_id(
            "emp-toggle",
            snapshot.toggle_label(),
            snapshot.toggle().is_some(),
            None,
        );
        let quit = MenuItem::with_id("emp-quit", copy.quit, true, None);
        let menu = Menu::with_items(&[&open, &toggle, &quit])?;
        let image = Icon::from_rgba(icon::rgba()?, icon::SIZE, icon::SIZE)?;
        let builder = TrayIconBuilder::new().with_id("easy-multi-provider");
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(image);
        #[cfg(windows)]
        let builder = builder.with_icon(image);
        let icon = builder
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .with_menu_on_right_click(true)
            .with_tooltip(snapshot.tooltip())
            .build()?;
        Ok(Self {
            icon,
            open,
            toggle,
            quit,
            snapshot,
            actions,
            events,
        })
    }

    pub(super) fn update(&mut self, snapshot: Snapshot) {
        let copy = snapshot.copy();
        self.open.set_text(copy.open);
        self.toggle.set_text(snapshot.toggle_label());
        self.toggle.set_enabled(snapshot.toggle().is_some());
        self.quit.set_text(copy.quit);
        self.quit.set_enabled(!snapshot.busy);
        let _ = self.icon.set_tooltip(Some(snapshot.tooltip()));
        self.snapshot = snapshot;
    }

    pub(super) fn pump(&mut self) {
        self.events.pump();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let action = match event.id.as_ref() {
                "emp-open" => Some(Action::Open),
                "emp-toggle" => self.snapshot.toggle(),
                "emp-quit" if !self.snapshot.busy => Some(Action::Quit),
                _ => None,
            };
            if let Some(action) = action {
                let _ = self.actions.try_send(action);
            }
        }
    }
}
