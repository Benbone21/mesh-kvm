//! The tray icon. It holds no state of its own: the tooltip comes from one `Client`, and the
//! menu either opens the app window or sends the same actions the window does.

use super::client::Client;
use super::launch::ServiceStarter;
use super::model::{ServiceStart, Snapshot};
use super::updates::VersionWatch;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH_EVERY_MS: u64 = 500;
/// The tray only needs the tooltip, so it polls the service less often than the window does.
const POLL_EVERY: Duration = Duration::from_secs(2);
/// How long a failed action shows in the tooltip.
const NOTICE_FOR: Duration = Duration::from_secs(5);
/// How long the tray has to close by itself once it's told to.
const LEAVE_WITHIN: Duration = Duration::from_secs(3);
/// The menu item that stops mesh-kvm on this device.
const QUIT_LABEL: &str = "Quit mesh-kvm";

/// What the tray's tooltip says, in plain words.
fn tooltip(snapshot: &Snapshot) -> String {
    match (&snapshot.local, snapshot.active()) {
        (Some(_), Some(device)) => format!("mesh-kvm: {} has keyboard and mouse", snapshot.title(device)),
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
fn current_text(client: &Client, shared: &SharedNotices, start: &ServiceStart) -> String {
    let now = Instant::now();
    if let Some(problem) = client.last_error() {
        client.dismiss_error();
        notices(shared).show(&problem, now);
    }
    let snapshot = client.snapshot();
    let base = super::model::start_notice(start, &snapshot).map_or_else(|| tooltip(&snapshot), |text| format!("mesh-kvm: {text}"));
    notices(shared).text(base, now)
}

/// Starts the new version's tray and ends this one, once the service has moved on to it.
fn follow_update(client: &Client, watch: &mut VersionWatch) {
    let own = crate::update::trust::running_version().to_string();
    let snapshot = client.snapshot();
    if watch.moved_on(&own, snapshot.local.as_ref().map(|l| l.version.as_str())) {
        match super::launch::relaunch("--tray") {
            Ok(()) => std::process::exit(0),
            Err(e) => crate::settings::log_note(&format!("Couldn't reopen the tray after updating: {e}")),
        }
    }
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
        Action::ReleaseAll => client.release_all_input(),
    }
}

/// Set when this tray is to close: told so by `--quit` or the installer, or after its own Quit.
static LEAVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn leaving() -> bool {
    LEAVE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Quit mesh-kvm: stops the window, the service and this tray. The other device then only sees
/// this one as offline. It starts again from the menu, or at sign-in if that's on.
fn quit_everything() {
    LEAVE.store(true, std::sync::atomic::Ordering::SeqCst);
    let quit = crate::quit::everything_here();
    if quit == crate::quit::Quit::StillRunning {
        crate::settings::log_note("Quit was chosen in the tray, but the service didn't stop.");
    }
}

/// Runs the tray until Quit, which stops mesh-kvm on this device.
pub fn run_tray() -> Result<(), String> {
    let held = match super::single::claim_after_handover(super::single::TRAY_PORT) {
        super::single::Instance::First(listener) => listener,
        super::single::Instance::AlreadyRunning => return Ok(()),
        super::single::Instance::Failed(e) => return Err(e),
    };
    super::single::listen(held, |word| {
        if word == super::single::Word::Quit {
            LEAVE.store(true, std::sync::atomic::Ordering::SeqCst);
            // The loop notices within a moment. A tray still waiting for the taskbar doesn't.
            std::thread::sleep(LEAVE_WITHIN);
            std::process::exit(0);
        }
    });
    crate::mute::start_service_watch();
    platform::run(starter_for_login(crate::settings::autostart_disabled_marker().exists()))
}

/// The tray runs at every login, so it only starts the service if that hasn't been turned off.
/// Opening the app still starts it.
fn starter_for_login(turned_off: bool) -> ServiceStarter {
    if turned_off {
        ServiceStarter::default()
    } else {
        ServiceStarter::begin()
    }
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

        /// A left-click opens the window; the menu stays on the right-click.
        fn activate(&mut self, _x: i32, _y: i32) {
            perform(Action::Open, &self.client, &self.notices);
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
                    label: QUIT_LABEL.into(),
                    activate: Box::new(|this: &mut Self| this.quit.store(true, Ordering::SeqCst)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub fn run(starter: ServiceStarter) -> Result<(), String> {
        let client = Arc::new(Client::start(POLL_EVERY, Box::new(|| {})));
        let shared = SharedNotices::default();
        let quit = Arc::new(AtomicBool::new(false));
        let tray = MeshTray {
            text: current_text(&client, &shared, &starter.state()),
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
        let mut watch = VersionWatch::default();
        while !quit.load(Ordering::SeqCst) && !leaving() && !handle.is_closed() {
            std::thread::sleep(Duration::from_millis(REFRESH_EVERY_MS));
            follow_update(&client, &mut watch);
            let text = current_text(&client, &shared, &starter.state());
            handle.update(|tray| {
                if tray.text != text {
                    tray.text = text;
                }
            });
        }
        if quit.load(Ordering::SeqCst) {
            quit_everything();
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    static OPEN_REQUESTED: AtomicBool = AtomicBool::new(false);
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

    pub fn run(starter: ServiceStarter) -> Result<(), String> {
        let client = Client::start(POLL_EVERY, Box::new(|| {}));
        let shared = SharedNotices::default();
        let open = MenuItem::new("Open", true, None);
        let switch = MenuItem::new("Switch device", true, None);
        let release = MenuItem::new("Release all input", true, None);
        let quit = MenuItem::new(QUIT_LABEL, true, None);
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
        // A left-click opens the window. The handler only notes it; the loop below does the work.
        TrayIconEvent::set_event_handler(Some(|event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                OPEN_REQUESTED.store(true, Ordering::SeqCst);
            }
        }));
        let mut shown = current_text(&client, &shared, &starter.state());
        let tray = build_with_retry(&menu, &icon, &shown)?;

        // A timer wakes the message loop, so the tooltip and menu clicks are handled promptly.
        unsafe { SetTimer(std::ptr::null_mut(), 0, REFRESH_EVERY_MS as u32, None) };
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let mut watch = VersionWatch::default();
        while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            follow_update(&client, &mut watch);
            if leaving() {
                return Ok(());
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == *quit.id() {
                    drop(tray);
                    quit_everything();
                    return Ok(());
                }
                if let Some((_, action)) = actions.iter().find(|(id, _)| *id == event.id) {
                    perform(*action, &client, &shared);
                }
            }
            if OPEN_REQUESTED.swap(false, Ordering::SeqCst) {
                perform(Action::Open, &client, &shared);
            }
            let text = current_text(&client, &shared, &starter.state());
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
    fn a_tray_with_starting_at_boot_off_starts_nothing() {
        assert_eq!(starter_for_login(true).state(), ServiceStart::Done);
    }

    #[test]
    fn unreadable_service_shows_not_connected() {
        assert_eq!(tooltip(&Snapshot::default()), "mesh-kvm: not connected");
    }

    fn snapshot_with_active(active: &str) -> Snapshot {
        let local = DeviceStatus {
            node: "debian".into(),
            device_name: "Desk tower".into(),
            active_node: active.into(),
            ..Default::default()
        };
        Snapshot { local: Some(local), ..Default::default() }
    }

    #[test]
    fn tooltip_names_the_device_with_the_keyboard_and_mouse() {
        assert_eq!(
            tooltip(&snapshot_with_active("Debian")),
            "mesh-kvm: Desk tower has keyboard and mouse"
        );
        assert_eq!(
            tooltip(&snapshot_with_active("Windows")),
            "mesh-kvm: The other device has keyboard and mouse"
        );
    }

    #[test]
    fn tooltip_without_a_known_active_device_shows_not_connected() {
        assert_eq!(tooltip(&snapshot_with_active("")), "mesh-kvm: not connected");
    }
}
