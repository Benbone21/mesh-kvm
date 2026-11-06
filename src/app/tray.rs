//! The tray icon. It holds no state of its own: the tooltip comes from one `Client`, and the
//! menu either opens the app window or sends the same actions the window does.

use super::client::Client;
use super::model::Snapshot;

const REFRESH_EVERY_MS: u64 = 500;

/// What the tray's tooltip says, in plain words.
fn tooltip(snapshot: &Snapshot) -> String {
    match (&snapshot.local, snapshot.active()) {
        (Some(_), Some(device)) => format!("mesh-kvm: {} has keyboard and mouse", device.title()),
        _ => "mesh-kvm: not connected".to_string(),
    }
}

/// Starts `mesh-kvm --app`. A second window asks the first to come forward and exits.
fn open_app() {
    let _ = super::launch::spawn_self("--app");
}

#[derive(Clone, Copy)]
enum Action {
    Open,
    SwitchDevice,
    ReleaseAll,
}

fn perform(action: Action, client: &Client) {
    match action {
        Action::Open => open_app(),
        Action::SwitchDevice => client.master_action("toggle_focus"),
        Action::ReleaseAll => client.master_action("panic"),
    }
}

/// Runs the tray until Quit. Quit ends only this process; the service keeps running.
pub fn run_tray() -> Result<(), String> {
    let _held = match super::single::claim(super::single::TRAY_PORT) {
        super::single::Instance::First(listener) => listener,
        super::single::Instance::AlreadyRunning => return Ok(()),
    };
    platform::run()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use ksni::blocking::TrayMethods;
    use ksni::menu::{MenuItem, StandardItem};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct MeshTray {
        text: String,
        client: Arc<Client>,
        quit: Arc<AtomicBool>,
    }

    impl MeshTray {
        fn item(&self, label: &str, action: Action) -> MenuItem<Self> {
            StandardItem {
                label: label.into(),
                activate: Box::new(move |this: &mut Self| perform(action, &this.client)),
                ..Default::default()
            }
            .into()
        }
    }

    impl ksni::Tray for MeshTray {
        fn id(&self) -> String {
            "mesh-kvm".into()
        }

        fn title(&self) -> String {
            "mesh-kvm".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![ksni::Icon {
                width: crate::app::icon::SIZE as i32,
                height: crate::app::icon::SIZE as i32,
                data: crate::app::icon::argb(),
            }]
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: self.text.clone(), ..Default::default() }
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                self.item("Open", Action::Open),
                self.item("Switch device", Action::SwitchDevice),
                self.item("Release all input", Action::ReleaseAll),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|this: &mut Self| this.quit.store(true, Ordering::SeqCst)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub fn run() -> Result<(), String> {
        let client = Arc::new(Client::start(Box::new(|| {})));
        let quit = Arc::new(AtomicBool::new(false));
        let tray = MeshTray {
            text: tooltip(&client.snapshot()),
            client: client.clone(),
            quit: quit.clone(),
        };
        let handle = tray.spawn().map_err(|e| format!("The tray icon couldn't start: {e}"))?;
        while !quit.load(Ordering::SeqCst) && !handle.is_closed() {
            std::thread::sleep(Duration::from_millis(REFRESH_EVERY_MS));
            let text = tooltip(&client.snapshot());
            handle.update(|tray| {
                if tray.text != text {
                    tray.text = text;
                }
            });
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, SetTimer, TranslateMessage, MSG,
    };

    pub fn run() -> Result<(), String> {
        let client = Client::start(Box::new(|| {}));
        let open = MenuItem::new("Open", true, None);
        let switch = MenuItem::new("Switch device", true, None);
        let release = MenuItem::new("Release all input", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &switch, &release, &PredefinedMenuItem::separator(), &quit])
            .map_err(|e| format!("The tray menu couldn't be built: {e}"))?;
        let actions: [(MenuId, Action); 3] = [
            (open.id().clone(), Action::Open),
            (switch.id().clone(), Action::SwitchDevice),
            (release.id().clone(), Action::ReleaseAll),
        ];
        let icon = Icon::from_rgba(
            crate::app::icon::rgba(),
            crate::app::icon::SIZE,
            crate::app::icon::SIZE,
        )
        .map_err(|e| format!("The tray icon picture is unusable: {e}"))?;
        let mut shown = tooltip(&client.snapshot());
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip(&shown)
            .build()
            .map_err(|e| format!("The tray icon couldn't start: {e}"))?;

        // A timer wakes the message loop, so the tooltip and menu clicks are handled promptly.
        unsafe { SetTimer(std::ptr::null_mut(), 0, REFRESH_EVERY_MS as u32, None) };
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == *quit.id() {
                    return Ok(());
                }
                if let Some((_, action)) = actions.iter().find(|(id, _)| *id == event.id) {
                    perform(*action, &client);
                }
            }
            let text = tooltip(&client.snapshot());
            if text != shown {
                let _ = tray.set_tooltip(Some(&text));
                shown = text;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model::{DeviceStatus, Snapshot};

    #[test]
    fn unreadable_service_shows_not_connected() {
        assert_eq!(tooltip(&Snapshot::default()), "mesh-kvm: not connected");
    }

    fn snapshot_with_active(active: &str) -> Snapshot {
        let local = DeviceStatus {
            node: "debian".into(),
            active_node: active.into(),
            ..Default::default()
        };
        Snapshot { local: Some(local), ..Default::default() }
    }

    #[test]
    fn tooltip_names_the_device_with_the_keyboard_and_mouse() {
        assert_eq!(
            tooltip(&snapshot_with_active("Debian")),
            "mesh-kvm: Debian desktop has keyboard and mouse"
        );
        assert_eq!(
            tooltip(&snapshot_with_active("Windows")),
            "mesh-kvm: Windows laptop has keyboard and mouse"
        );
    }

    #[test]
    fn tooltip_without_a_known_active_device_shows_not_connected() {
        assert_eq!(tooltip(&snapshot_with_active("")), "mesh-kvm: not connected");
    }
}
