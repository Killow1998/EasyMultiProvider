//! StatusNotifierItem + D-Bus menu; no GTK or terminal process is needed.
use super::{Action, Snapshot, icon};
use ksni::blocking::TrayMethods;
use std::sync::mpsc::SyncSender;

pub(super) struct Tray(ksni::blocking::Handle<LinuxTray>);

impl Tray {
    pub(super) fn new(
        snapshot: Snapshot,
        actions: SyncSender<Action>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let rgba = icon::rgba()?;
        let data = rgba
            .chunks_exact(4)
            .flat_map(|p| [p[3], p[0], p[1], p[2]])
            .collect();
        let item = LinuxTray {
            snapshot,
            actions,
            icon: ksni::Icon {
                width: icon::SIZE as i32,
                height: icon::SIZE as i32,
                data,
            },
        };
        Ok(Self(item.spawn()?))
    }

    pub(super) fn update(&mut self, snapshot: Snapshot) {
        self.0.update(move |tray| tray.snapshot = snapshot);
    }

    pub(super) fn pump(&mut self) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.0.shutdown().wait();
    }
}

struct LinuxTray {
    snapshot: Snapshot,
    actions: SyncSender<Action>,
    icon: ksni::Icon,
}

impl LinuxTray {
    fn item(&self, label: &str, action: Option<Action>) -> ksni::MenuItem<Self> {
        ksni::menu::StandardItem {
            label: label.to_owned(),
            enabled: action.is_some(),
            activate: Box::new(move |tray: &mut Self| {
                if let Some(action) = action {
                    let _ = tray.actions.try_send(action);
                }
            }),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for LinuxTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "easy-multi-provider".to_owned()
    }
    fn title(&self) -> String {
        "EMP".to_owned()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.snapshot.tooltip().to_owned(),
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let copy = self.snapshot.copy();
        vec![
            self.item(copy.open, Some(Action::Open)),
            self.item(self.snapshot.toggle_label(), self.snapshot.toggle()),
            self.item(
                self.snapshot.quit_label(),
                self.snapshot.busy.is_none().then_some(Action::Quit),
            ),
        ]
    }
}
