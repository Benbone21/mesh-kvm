//! The tray icon. It holds no state of its own: the tooltip comes from one `Client`, and the
//! menu either opens the app window or sends the same actions the window does.

use super::client::Client;
use super::model::Snapshot;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH_EVERY_MS: u64 = 500;
/// How long a failed action shows in the tooltip.
const NOTICE_FOR: Duration = Duration::from_secs(5);

/// What the tray's tooltip says, in plain words.
fn tooltip(snapshot: &Snapshot) -> String {
    match (&snapshot.local, snapshot.active()) {
        (Some(_), Some(device)) => format!("mesh-kvm: {} has keyboard and mouse", device.title()),
        _ => "mesh-kvm: not connected".to_string(),
    }
}

/// A problem to show in the tooltip for a few seconds, since the tray has nowhere else to say it.
#[derive(Default)]
struct Notices {
    current: Option<(String, Instant)>,
}

impl Notices {
    fn show(&mut self, message: &str, now: Instant) {
        #[cfg(target_os = "linux")]
        eprintln!("[Tray] {message}");
        self.current = Some((message.to_string(), now + NOTICE_FOR));
    }

    /// The tooltip to show: the notice while it lasts, else `base`.
    fn text(&mut self, base: String, now: Instant) -> String {
        match &self.current {
            Some((message, until)) if now < *until => format!("mesh-kvm: {message}"),
            _ => {
                self.current = None;
                base
            }
        }
    }
}

type SharedNotices = Arc<Mutex<Notices>>;

fn notices(shared: &SharedNotices) -> std::sync::MutexGuard<'_, Notices> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// The tooltip now: a failed action's message, or what the service says.
fn current_text(client: &Client, shared: &SharedNotices) -> String {
    let now = Instant::now();
    if let Some(problem) = client.last_error() {
        client.dismiss_error();
        notices(shared).show(&problem, now);
    }
    notices(shared).text(tooltip(&client.snapshot()), now)
}

#[derive(Clone, Copy)]
enum Action {
    Open,
    SwitchDevice,
    ReleaseAll,
}

fn perform(action: Action, client: &Client, shared: &SharedNotices) {
    match action {
        // `--app` starts a second window process; it asks the first to come forward and exits.
        Action::Open => {
            if let Err(e) = super::launch::spawn_self("--app") {
                notices(shared).show(&format!("Couldn't open the window: {e}"), Instant::now());
            }
        }
        // These go to the service in the background; a refusal comes back through `last_error`.
        Action::SwitchDevice => client.master_action("toggle_focus"),
        Action::ReleaseAll => client.master_action("panic"),
    }
}

/// Runs the tray until Quit. Quit ends only this process; the service keeps running.
pub fn run_tray() -> Result<(), String> {
    let _held = match super::single::claim(super::single::TRAY_PORT) {
        super::single::Instance::First(listener) => listener,
        super::single::Instance::AlreadyRunning => return Ok(()),
        super::single::Instance::Failed(e) => return Err(e),
    };
    platform::run()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use ksni::blocking::TrayMethods;
    use ksni::menu::{MenuItem, StandardItem};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct MeshTray {
        text: String,
        client: Arc<Client>,
        notices: SharedNotices,
        quit: Arc<AtomicBool>,
    }

    impl MeshTray {
        fn item(&self, label: &str, action: Action) -> MenuItem<Self> {
            StandardItem {
                label: label.into(),
                activate: Box::new(move |this: &mut Self| perform(action, &this.client, &this.notices)),
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
        let shared = SharedNotices::default();
        let quit = Arc::new(AtomicBool::new(false));
        let tray = MeshTray {
            text: current_text(&client, &shared),
            client: client.clone(),
            notices: shared.clone(),
            quit: quit.clone(),
        };
        // At login the tray can start before the desktop's tray host does. Assuming one is
        // available lets the icon appear when the host does.
        let handle = tray
            .assume_sni_available(true)
            .spawn()
            .map_err(|e| format!("The tray icon couldn't start: {e}"))?;
        while !quit.load(Ordering::SeqCst) && !handle.is_closed() {
            std::thread::sleep(Duration::from_millis(REFRESH_EVERY_MS));
            let text = current_text(&client, &shared);
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
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, SetTimer, TranslateMessage, MSG,
    };

    /// At login the taskbar may not exist yet, so building the icon is retried for this long.
    const BUILD_FOR: Duration = Duration::from_secs(60);
    const BUILD_RETRY_EVERY: Duration = Duration::from_secs(2);

    fn build_with_retry(menu: &Menu, icon: &Icon, tooltip: &str) -> Result<TrayIcon, String> {
        let started = Instant::now();
        loop {
            let built = TrayIconBuilder::new()
                .with_menu(Box::new(menu.clone()))
                .with_icon(icon.clone())
                .with_tooltip(tooltip)
                .build();
            match built {
                Ok(tray) => return Ok(tray),
                Err(e) if started.elapsed() >= BUILD_FOR => {
                    return Err(format!("The tray icon couldn't start: {e}"));
                }
                Err(_) => std::thread::sleep(BUILD_RETRY_EVERY),
            }
        }
    }

    pub fn run() -> Result<(), String> {
        let client = Client::start(Box::new(|| {}));
        let shared = SharedNotices::default();
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
        // Nothing reads icon hover and click events, so don't let them pile up in the channel.
        TrayIconEvent::set_event_handler(Some(|_| {}));
        let mut shown = current_text(&client, &shared);
        let tray = build_with_retry(&menu, &icon, &shown)?;

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
                    perform(*action, &client, &shared);
                }
            }
            let text = current_text(&client, &shared);
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
    use std::time::{Duration, Instant};

    #[test]
    fn a_notice_replaces_the_tooltip_for_five_seconds() {
        let t0 = Instant::now();
        let mut notices = Notices::default();
        assert_eq!(notices.text("base".into(), t0), "base");
        notices.show("Couldn't open the window", t0);
        assert_eq!(notices.text("base".into(), t0 + Duration::from_secs(4)), "mesh-kvm: Couldn't open the window");
        assert_eq!(notices.text("base".into(), t0 + Duration::from_secs(6)), "base");
    }

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
