//! Starting mesh-kvm's own pieces: the tray and the window run as separate processes of this exe.

use super::icon::ICON_SIZES;
use super::model::ServiceStart;
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "windows"))]
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How often the service is looked for while it starts, and for how long: the first start under
/// Windows' scanning or a busy login can take a while. A look at a port that accepts and never
/// answers takes over a second by itself, so the tries alone don't set the time.
const START_POLL: Duration = Duration::from_millis(250);
const START_POLLS: u32 = 60;
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a service that has answered must keep answering. Under systemd, one that can't reach
/// its input devices answers for a moment before it stops.
const START_SETTLE: Duration = Duration::from_millis(600);
#[cfg(target_os = "linux")]
const UNIT: &str = "mesh-kvm.service";

/// Quotes `exe` for a VBScript string that is itself inside a `WshShell.Run` command line.
fn vbs_run(exe: &str, args: &str) -> String {
    format!("WshShell.Run \"\"\"{exe}\"\" {args}\", 0, False\r\n")
}

const VBS_HEAD: &str = "Set WshShell = CreateObject(\"WScript.Shell\")\r\n";

/// The script in the Startup folder: starts the service and the tray at login, both windowless.
/// The service reads the saved address, so that a change in the app is never undone at login.
/// `peer` is for an address that can't be saved yet.
///
/// If an update hasn't proved itself, as with a reboot right after the swap, the installed exe may
/// be missing or broken. The old exe then does the starting, as a watcher that can put things back.
/// It runs as a copy, as `Run` goes through the shell, which won't start a file called `.old`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn windows_vbs(exe: &str, peer: Option<&str>) -> String {
    let service = match peer {
        Some(peer) => format!("--role windows --peer {peer}"),
        None => "--role windows".to_string(),
    };
    let folder = exe.rsplit_once('\\').map_or("", |(folder, _)| folder);
    let marker = crate::update::swap::MARKER_NAME;
    let aside = format!("{exe}.old");
    let copy = format!("{folder}\\{}", crate::update::swap::RECOVERY_EXE);
    let watcher = vbs_run(&copy, &format!("{} --tray {service}", crate::update::swap::WATCH_FLAG));
    format!(
        "{VBS_HEAD}Set Fso = CreateObject(\"Scripting.FileSystemObject\")\r\n\
         If Fso.FileExists(\"{folder}\\{marker}\") And Fso.FileExists(\"{aside}\") Then\r\n\
         Fso.CopyFile \"{aside}\", \"{copy}\", True\r\n\
         {watcher}WScript.Quit\r\nEnd If\r\n{}{}",
        vbs_run(exe, &service),
        vbs_run(exe, "--tray")
    )
}

/// The address in a login script an older version wrote, if it holds a usable one.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn peer_in_login_script(script: &str) -> Option<String> {
    let after = script.split_once("--peer ")?.1;
    let word = after.split(|c: char| c.is_whitespace() || c == '"').next()?;
    crate::settings::validate_peer(word).ok()
}

/// The Start Menu script: opens the app window.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn windows_app_vbs(exe: &str) -> String {
    format!("{VBS_HEAD}{}", vbs_run(exe, "--app"))
}

const OLD_START_MENU_SCRIPT: &str = "mesh-kvm Studio.vbs";
const START_MENU_SCRIPT: &str = "mesh-kvm.vbs";
const START_MENU_SHORTCUT: &str = "mesh-kvm.lnk";
const LOGIN_SCRIPT: &str = "mesh-kvm-autostart.vbs";

/// Creates `lnk`, which opens the app with the exe's own icon, through the shell's own shortcut
/// object. True when the shortcut exists after.
#[cfg(target_os = "windows")]
pub fn make_shortcut(lnk: &Path, exe: &str) -> bool {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    let dir = Path::new(exe).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let made = unsafe {
        // Already set up on this thread, one way or the other, is fine too.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(exe))?;
            link.SetArguments(&HSTRING::from("--app"))?;
            link.SetWorkingDirectory(&HSTRING::from(dir.as_str()))?;
            link.SetIconLocation(&HSTRING::from(exe), 0)?;
            link.SetDescription(&HSTRING::from("Share one mouse and keyboard between your computers"))?;
            link.cast::<IPersistFile>()?.Save(&HSTRING::from(lnk.as_os_str()), true)
        })()
    };
    made.is_ok() && lnk.exists()
}

/// Writes the Start Menu entry (a shortcut, or the script if the shortcut can't be made), removes
/// the one older installs left, and (when `autostart` is on and the Startup folder exists) writes
/// the login script, or removes it when `autostart` is off. Returns the login script's path if it
/// was written.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn install_windows_launchers(
    programs_dir: &Path,
    exe: &str,
    peer: Option<&str>,
    autostart: bool,
    shortcut: &dyn Fn(&Path, &str) -> bool,
) -> Option<PathBuf> {
    let _ = std::fs::remove_file(programs_dir.join(OLD_START_MENU_SCRIPT));
    // The login script goes first: the shortcut can take a moment, and the app reads this file to
    // show the start-at-boot switch.
    let startup = programs_dir.join("Startup");
    if !autostart {
        let _ = std::fs::remove_file(startup.join(LOGIN_SCRIPT));
    }
    let login = (autostart && startup.exists())
        .then(|| startup.join(LOGIN_SCRIPT))
        .and_then(|login| std::fs::write(&login, windows_vbs(exe, peer)).ok().map(|_| login));
    let script = programs_dir.join(START_MENU_SCRIPT);
    let lnk = programs_dir.join(START_MENU_SHORTCUT);
    if (lnk.exists() && shortcut_points_at(&lnk, exe)) || shortcut(&lnk, exe) {
        let _ = std::fs::remove_file(&script);
    } else {
        let _ = std::fs::write(&script, windows_app_vbs(exe));
    }
    login
}

/// True when the shortcut file holds `exe`, as text or as the wide characters Windows stores.
fn shortcut_points_at(lnk: &Path, exe: &str) -> bool {
    let Ok(bytes) = std::fs::read(lnk) else { return false };
    let wide: Vec<u8> = exe.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let has = |needle: &[u8]| !needle.is_empty() && bytes.windows(needle.len()).any(|w| w == needle);
    has(exe.as_bytes()) || has(&wide)
}

/// Quotes `exe` as the Desktop Entry spec wants, when it needs it.
fn exec_path(exe: &str) -> String {
    let exe = exe.replace('%', "%%");
    if exe.chars().any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c)) {
        let escaped: String = exe
            .chars()
            .flat_map(|c| if "\"`$\\".contains(c) { vec!['\\', c] } else { vec![c] })
            .collect();
        format!("\"{escaped}\"")
    } else {
        exe
    }
}

/// The window's app id on Wayland, and so the name of the menu entry the panel matches it to.
pub const APP_ID: &str = "mesh-kvm";

/// A freedesktop launcher that runs `exe args`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn linux_desktop_entry(exe: &str, args: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=mesh-kvm\n\
         Comment=Share one mouse and keyboard between your computers\n\
         Exec={} {args}\n\
         Icon=mesh-kvm\n\
         Terminal=false\n\
         Categories=Utility;\n\
         StartupWMClass={APP_ID}\n",
        exec_path(exe)
    )
}

/// Writes the menu entry, the tray's autostart entry, and the icon in each of `ICON_SIZES` under
/// `home`. Returns the files written.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn write_linux_launchers(home: &Path, exe: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut files = vec![
        (
            home.join(format!(".local/share/applications/{APP_ID}.desktop")),
            linux_desktop_entry(exe, "--app").into_bytes(),
        ),
        (
            home.join(".config/autostart/mesh-kvm-tray.desktop"),
            linux_desktop_entry(exe, "--tray").into_bytes(),
        ),
    ];
    for size in ICON_SIZES {
        files.push((
            home.join(format!(".local/share/icons/hicolor/{size}x{size}/apps/mesh-kvm.png")),
            super::icon::png_scaled(size),
        ));
    }
    let mut written = Vec::new();
    for (path, bytes) in files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, bytes)?;
        written.push(path);
    }
    Ok(written)
}

/// Writes only the tray's login entry, for this exe, when starting at boot is turned on.
#[cfg(target_os = "linux")]
pub fn write_tray_autostart(home: &Path) -> std::io::Result<()> {
    let path = home.join(".config/autostart/mesh-kvm-tray.desktop");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, linux_desktop_entry(&running_exe()?.to_string_lossy(), "--tray"))
}

/// `mesh-kvm --install-launchers`: writes the launchers for this exe and says what it wrote.
#[cfg(target_os = "linux")]
pub fn install_linux_launchers() -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME isn't set, so there is nowhere to write the launchers.".to_string())?;
    let exe = running_exe().map_err(|e| format!("Couldn't find where mesh-kvm is installed: {e}"))?;
    let written = write_linux_launchers(Path::new(&home), &exe.to_string_lossy())
        .map_err(|e| format!("Couldn't write the launchers: {e}"))?;
    for path in written {
        println!("Wrote {}", path.display());
    }
    match refresh_menu(|tool| run_quietly(tool)) {
        Some(tool) => println!("Refreshed the app menu with {tool}"),
        None => println!("Log out and back in if mesh-kvm doesn't appear in your app menu."),
    }
    Ok(())
}

/// KDE reads its app menu from a cache, so a new entry stays hidden until the cache is rebuilt.
#[cfg(any(target_os = "linux", test))]
fn refresh_menu(run: impl Fn(&str) -> bool) -> Option<&'static str> {
    ["kbuildsycoca6", "kbuildsycoca5"].into_iter().find(|tool| run(tool))
}

#[cfg(target_os = "linux")]
fn run_quietly(tool: &str) -> bool {
    std::process::Command::new(tool)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Linux reports `current_exe` with " (deleted)" on the end once a rebuild has replaced the file.
fn without_deleted_suffix(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_suffix(" (deleted)")) {
        Some(clean) => PathBuf::from(clean),
        None => path,
    }
}

pub fn running_exe() -> std::io::Result<PathBuf> {
    std::env::current_exe().map(without_deleted_suffix)
}

/// Starts this exe again with `arg` (such as `--app` or `--tray`) and does not wait for it.
pub fn spawn_self(arg: &str) -> std::io::Result<()> {
    spawn_with(&[arg], &[])
}

/// Set on a window or tray started by the one it replaces, which still holds the port.
pub const RELAUNCHED_ENV: &str = "MESH_KVM_RELAUNCHED";

/// A running exe that an update moved aside still goes by the name it has now.
fn without_old_suffix(path: PathBuf) -> PathBuf {
    let text = path.to_str();
    match text.and_then(|p| p.strip_suffix(".old").or_else(|| p.strip_suffix(".failed"))) {
        Some(clean) => PathBuf::from(clean),
        None => path,
    }
}

/// Starts the new version of a window or tray, which waits for this old one to end.
pub fn relaunch(arg: &str) -> std::io::Result<()> {
    spawn_exe(without_old_suffix(running_exe()?), &[arg], &[(RELAUNCHED_ENV, "1")])
}

/// Starts this exe again with `args` and does not wait for it. It outlives this process.
fn spawn_with(args: &[&str], envs: &[(&str, &str)]) -> std::io::Result<()> {
    spawn_exe(running_exe()?, args, envs)
}

/// On Windows the child gets none of this process's handles: see `platform::detach`.
#[cfg(target_os = "windows")]
pub fn spawn_exe(exe: PathBuf, args: &[&str], envs: &[(&str, &str)]) -> std::io::Result<()> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    crate::platform::detach::spawn(&exe, &args, envs).map(drop)
}

#[cfg(not(target_os = "windows"))]
pub fn spawn_exe(exe: PathBuf, args: &[&str], envs: &[(&str, &str)]) -> std::io::Result<()> {
    let mut command = Command::new(exe);
    command.args(args).envs(envs.iter().copied()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    // Collect the child's exit status when it ends, so it doesn't linger as a zombie.
    let _ = std::thread::Builder::new().name("mesh-kvm-reap".into()).spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum StartPlan {
    /// It is already running.
    Nothing,
    /// A systemd unit runs it here, so that it restarts and stops with the session.
    Unit,
    /// Nothing else will run it, so this exe starts one that outlives it.
    Detached,
}

/// How to get a service running here. Doing nothing when one answers is what keeps a second one
/// from starting, and the API port stops one that slips past this in a race.
pub fn start_plan(service_answers: bool, unit_exists: bool) -> StartPlan {
    match (service_answers, unit_exists) {
        (true, _) => StartPlan::Nothing,
        (false, true) => StartPlan::Unit,
        (false, false) => StartPlan::Detached,
    }
}

/// What `ensure_service` does to the outside world.
trait ServiceHost {
    fn answers(&self) -> bool;
    fn unit_exists(&self) -> bool;
    fn start_unit(&self) -> Result<(), String>;
    fn start_detached(&self) -> Result<(), String>;
    fn wait(&self, time: Duration);
}

/// Starts this device's service if none answers, and waits to see it run.
fn ensure_service(host: &dyn ServiceHost, log: &Path) -> Result<(), String> {
    let failed = |why: String| format!("mesh-kvm couldn't start on this device: {why}");
    match start_plan(host.answers(), host.unit_exists()) {
        StartPlan::Nothing => return Ok(()),
        StartPlan::Unit => {
            // A unit that won't start shouldn't leave the person with nothing.
            if host.start_unit().is_err() {
                host.start_detached().map_err(failed)?;
            }
        }
        StartPlan::Detached => host.start_detached().map_err(failed)?,
    }
    let started = Instant::now();
    for _ in 0..START_POLLS {
        host.wait(START_POLL);
        if host.answers() {
            host.wait(START_SETTLE);
            if host.answers() {
                return Ok(());
            }
        }
        if started.elapsed() >= START_TIMEOUT {
            break;
        }
    }
    Err(format!("mesh-kvm didn't start on this device. The reason may be in the log: {}", log.display()))
}

struct ThisDevice;

impl ServiceHost for ThisDevice {
    fn answers(&self) -> bool {
        matches!(crate::webui::http_call("127.0.0.1", "GET", "/api/local", ""), Ok((200, _)))
    }

    #[cfg(target_os = "linux")]
    fn unit_exists(&self) -> bool {
        Command::new("systemctl")
            .args(["--user", "cat", UNIT])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(not(target_os = "linux"))]
    fn unit_exists(&self) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    fn start_unit(&self) -> Result<(), String> {
        // `--no-block`, as the unit's start is watched through the API anyway.
        let output = Command::new("systemctl")
            .args(["--user", "--no-block", "start", UNIT])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }

    #[cfg(not(target_os = "linux"))]
    fn start_unit(&self) -> Result<(), String> {
        Err("this device has no units".to_string())
    }

    fn start_detached(&self) -> Result<(), String> {
        // Never with no arguments: on Windows that is a double-click, which opens the window too.
        spawn_with(&["--role", crate::platform::ROLE], &[]).map_err(|e| e.to_string())
    }

    fn wait(&self, time: Duration) {
        std::thread::sleep(time);
    }
}

/// Gets this device's service running in the background, for the app and the tray, which must
/// not wait for it. Starting it can take seconds.
#[derive(Clone, Default)]
pub struct ServiceStarter(Arc<Mutex<ServiceStart>>);

impl ServiceStarter {
    pub fn begin() -> Self {
        let starter = Self(Arc::new(Mutex::new(ServiceStart::Starting)));
        let shared = starter.clone();
        let spawned = std::thread::Builder::new().name("mesh-kvm-service-start".into()).spawn(move || {
            let log = crate::settings::log_path();
            let result = ensure_service(&ThisDevice, &log);
            shared.finish(result);
        });
        if let Err(e) = spawned {
            starter.finish(Err(format!("mesh-kvm couldn't start on this device: {e}")));
        }
        starter
    }

    fn finish(&self, result: Result<(), String>) {
        if let Err(reason) = &result {
            crate::settings::log_note(reason);
        }
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = match result {
            Ok(()) => ServiceStart::Done,
            Err(reason) => ServiceStart::Failed(reason),
        };
    }

    pub fn state(&self) -> ServiceStart {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    #[cfg(test)]
    pub fn begin_for_test(state: ServiceStart) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    #[cfg(test)]
    pub fn finish_for_test(&self, reason: &str) {
        *self.0.lock().unwrap() = ServiceStart::Failed(reason.to_string());
    }
}

/// Set on a Windows service started by the one it replaces, which still holds the ports.
pub const RESTARTING_ENV: &str = "MESH_KVM_RESTARTING";

/// The arguments a restarted service runs with. No arguments on Windows would open the window.
fn restart_args(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return vec!["--role".to_string(), crate::platform::ROLE.to_string()];
    }
    args
}

/// Names the service a watcher has to see end before it starts the new one.
pub const OLD_PID_FLAG: &str = "--old-pid";

/// What a watcher is started with: everything the service it replaces was, and that service's
/// process id.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn args_for_watcher(args: Vec<String>, old_pid: u32) -> Vec<String> {
    let mut all = vec![crate::update::swap::WATCH_FLAG.to_string(), OLD_PID_FLAG.to_string(), old_pid.to_string()];
    all.extend(restart_args(args));
    all
}

/// What a watcher was told.
#[derive(Debug, PartialEq, Eq)]
pub struct WatcherArgs {
    /// What to start the service with.
    pub service: Vec<String>,
    /// Start the tray too, as at login.
    pub tray: bool,
    /// The old service, when it's the one that started this watcher. The login script has none.
    pub old_pid: Option<u32>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn args_from_watcher(args: Vec<String>) -> WatcherArgs {
    let mut service = Vec::new();
    let (mut tray, mut old_pid) = (false, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tray" => tray = true,
            OLD_PID_FLAG => old_pid = args.next().and_then(|pid| pid.parse().ok()),
            _ if arg == crate::update::swap::WATCH_FLAG => {}
            _ => service.push(arg),
        }
    }
    WatcherArgs { service: restart_args(service), tray, old_pid }
}

/// How long a service started by the one it replaces waits for that one's ports to come free.
pub const PORTS_WAIT: Duration = Duration::from_secs(20);
const PORTS_LOOK: Duration = Duration::from_millis(250);

/// True for a service started by the one it replaces, or by the update's watcher.
pub fn restarting() -> bool {
    std::env::var_os(RESTARTING_ENV).is_some()
}

/// Binds with `bind`. A service that replaces another keeps trying while the port is still taken,
/// for up to `PORTS_WAIT`, as the old one may take a while to end. Any other start fails at once,
/// as a taken port then means another service is running.
pub fn bind_patiently<T>(bind: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let patience = if restarting() { PORTS_WAIT } else { Duration::ZERO };
    retry_while_taken(patience, bind, &std::thread::sleep)
}

fn retry_while_taken<T>(patience: Duration, mut bind: impl FnMut() -> std::io::Result<T>, wait: &dyn Fn(Duration)) -> std::io::Result<T> {
    let mut waited = Duration::ZERO;
    loop {
        match bind() {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && waited < patience => {
                wait(PORTS_LOOK);
                waited += PORTS_LOOK;
            }
            other => return other,
        }
    }
}

/// Replaces this service with a new one, which reads the settings again. Only returns on failure.
pub fn restart_service() -> String {
    // Not a `stop`: starting again can fail, and the failure is the stop.
    crate::linklog::record_and_wait("restarting", crate::linklog::Details::new());
    let args = restart_args(std::env::args().skip(1).collect());
    #[cfg(unix)]
    {
        // This keeps the process id, so systemd sees nothing happen, and a detached service stays
        // running with nobody to start it again. Every socket here closes on exec.
        use std::os::unix::process::CommandExt;
        match running_exe() {
            Ok(exe) => Command::new(exe).args(&args).exec().to_string(),
            Err(e) => e.to_string(),
        }
    }
    #[cfg(not(unix))]
    {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match spawn_with(&args, &[(RESTARTING_ENV, "1")]) {
            Ok(()) => std::process::exit(0),
            Err(e) => e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    /// A service that starts answering once `after` looks have passed since it was started.
    #[derive(Default)]
    struct FakeHost {
        running: Cell<bool>,
        unit: bool,
        unit_fails: bool,
        detached_fails: bool,
        /// Looks until a started service answers. `None` means it never does.
        answers_after: Option<u32>,
        /// Looks after it first answers that it dies.
        dies_after: Option<u32>,
        looks: Cell<u32>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl FakeHost {
        fn started(&self) -> bool {
            self.calls.borrow().iter().any(|c| matches!(*c, "unit" | "detached"))
        }
    }

    impl super::ServiceHost for FakeHost {
        fn answers(&self) -> bool {
            if self.running.get() {
                return true;
            }
            if !self.started() {
                return false;
            }
            let looks = self.looks.get();
            self.looks.set(looks + 1);
            let Some(after) = self.answers_after else { return false };
            looks >= after && self.dies_after.is_none_or(|dies| looks < after + dies)
        }

        fn unit_exists(&self) -> bool {
            self.unit
        }

        fn start_unit(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("unit");
            if self.unit_fails {
                self.calls.borrow_mut().clear();
                self.calls.borrow_mut().push("unit failed");
                return Err("no such bus".into());
            }
            Ok(())
        }

        fn start_detached(&self) -> Result<(), String> {
            if self.detached_fails {
                return Err("permission denied".into());
            }
            self.calls.borrow_mut().push("detached");
            Ok(())
        }

        fn wait(&self, _time: Duration) {}
    }

    fn ensure(host: &FakeHost) -> Result<(), String> {
        super::ensure_service(host, Path::new("/tmp/mesh-kvm.log"))
    }

    #[test]
    fn the_plan_starts_nothing_when_a_service_answers() {
        use super::{start_plan, StartPlan};
        assert_eq!(start_plan(true, true), StartPlan::Nothing);
        assert_eq!(start_plan(true, false), StartPlan::Nothing);
        assert_eq!(start_plan(false, true), StartPlan::Unit);
        assert_eq!(start_plan(false, false), StartPlan::Detached);
    }

    #[test]
    fn a_running_service_is_left_alone() {
        let host = FakeHost { unit: true, ..Default::default() };
        host.running.set(true);
        assert_eq!(ensure(&host), Ok(()));
        assert!(host.calls.borrow().is_empty());
    }

    #[test]
    fn the_unit_is_used_when_there_is_one_and_only_once() {
        let host = FakeHost { unit: true, answers_after: Some(3), ..Default::default() };
        assert_eq!(ensure(&host), Ok(()));
        assert_eq!(*host.calls.borrow(), vec!["unit"]);
    }

    #[test]
    fn with_no_unit_a_detached_service_is_started() {
        let host = FakeHost { answers_after: Some(0), ..Default::default() };
        assert_eq!(ensure(&host), Ok(()));
        assert_eq!(*host.calls.borrow(), vec!["detached"]);
    }

    #[test]
    fn a_unit_that_will_not_start_falls_back_to_a_detached_service() {
        let host = FakeHost { unit: true, unit_fails: true, answers_after: Some(0), ..Default::default() };
        assert_eq!(ensure(&host), Ok(()));
        assert_eq!(*host.calls.borrow(), vec!["unit failed", "detached"]);
    }

    #[test]
    fn a_service_that_cannot_be_started_says_why() {
        let host = FakeHost { detached_fails: true, ..Default::default() };
        let error = ensure(&host).unwrap_err();
        assert!(error.contains("couldn't start") && error.contains("permission denied"), "{error}");
    }

    #[test]
    fn a_service_that_never_answers_points_at_the_log() {
        let host = FakeHost::default();
        let error = ensure(&host).unwrap_err();
        assert!(error.contains("didn't start") && error.contains("/tmp/mesh-kvm.log"), "{error}");
    }

    /// A port that accepts and never answers: every look takes a long time.
    struct SilentPort(FakeHost);

    impl super::ServiceHost for SilentPort {
        fn answers(&self) -> bool {
            std::thread::sleep(Duration::from_millis(300));
            false
        }
        fn unit_exists(&self) -> bool {
            false
        }
        fn start_unit(&self) -> Result<(), String> {
            self.0.start_unit()
        }
        fn start_detached(&self) -> Result<(), String> {
            self.0.start_detached()
        }
        fn wait(&self, _time: Duration) {}
    }

    #[test]
    fn slow_looks_at_the_service_still_end_on_time() {
        let started = std::time::Instant::now();
        let host = SilentPort(FakeHost::default());
        assert!(super::ensure_service(&host, Path::new("/tmp/mesh-kvm.log")).is_err());
        // 60 looks at 300 ms would be 18 seconds.
        assert!(started.elapsed() < super::START_TIMEOUT + Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[test]
    fn a_service_that_answers_once_and_dies_has_not_started() {
        let host = FakeHost { answers_after: Some(2), dies_after: Some(1), ..Default::default() };
        assert!(ensure(&host).is_err());
    }

    #[test]
    fn a_restart_keeps_the_arguments_and_never_turns_into_a_bare_launch() {
        let kept = vec!["--role".to_string(), "windows".to_string(), "--port".to_string(), "9".to_string()];
        assert_eq!(restart_args(kept.clone()), kept);
        assert_eq!(restart_args(vec![]), vec!["--role".to_string(), crate::platform::ROLE.to_string()]);
    }

    #[test]
    fn menu_refresh_prefers_the_current_kde_tool() {
        assert_eq!(refresh_menu(|_| true), Some("kbuildsycoca6"));
    }

    #[test]
    fn menu_refresh_falls_back_to_the_older_tool() {
        assert_eq!(refresh_menu(|tool| tool == "kbuildsycoca5"), Some("kbuildsycoca5"));
    }

    #[test]
    fn menu_refresh_is_optional_off_kde() {
        assert_eq!(refresh_menu(|_| false), None);
    }

    use super::*;
    use std::path::Path;

    const EXE: &str = r"C:\Users\me\AppData\Local\mesh-kvm\mesh-kvm.exe";

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-launch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn login_script_starts_service_and_tray() {
        let vbs = windows_vbs(EXE, None);
        let normal = vbs.split_once("End If").unwrap().1;
        let runs: Vec<&str> = normal.lines().filter(|l| l.contains("WshShell.Run")).collect();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().any(|l| l.contains("--role windows")));
        assert!(!vbs.contains("--peer"), "the saved address is used, so that an app change isn't undone");
        assert_eq!(runs.iter().filter(|l| l.contains("--tray")).count(), 1);
        assert!(runs.iter().all(|l| l.contains(EXE) && l.ends_with(", 0, False")));
    }

    #[test]
    fn login_script_lines_are_exact_for_a_path_with_spaces() {
        let exe = r"C:\Users\Mary Jane\AppData\Local\mesh-kvm\mesh-kvm.exe";
        let vbs = windows_vbs(exe, Some("192.168.8.210"));
        let want_service = "WshShell.Run \"\"\"C:\\Users\\Mary Jane\\AppData\\Local\\mesh-kvm\\mesh-kvm.exe\"\" --role windows --peer 192.168.8.210\", 0, False";
        let want_tray = "WshShell.Run \"\"\"C:\\Users\\Mary Jane\\AppData\\Local\\mesh-kvm\\mesh-kvm.exe\"\" --tray\", 0, False";
        let lines: Vec<&str> = vbs.lines().collect();
        assert_eq!(lines[0], "Set WshShell = CreateObject(\"WScript.Shell\")");
        assert_eq!(lines[lines.len() - 2], want_service);
        assert_eq!(lines[lines.len() - 1], want_tray);
    }

    #[test]
    fn the_address_comes_out_of_a_login_script_from_an_older_version() {
        let old = "Set WshShell = CreateObject(\"WScript.Shell\")\r\n\
                   WshShell.Run \"\"\"C:\\mesh-kvm.exe\"\" --role windows --peer 192.168.8.241 --switch-edge left\", 0, False\r\n";
        assert_eq!(peer_in_login_script(old).as_deref(), Some("192.168.8.241"));
        let at_end = windows_vbs(EXE, Some("192.168.8.241"));
        assert_eq!(peer_in_login_script(&at_end).as_deref(), Some("192.168.8.241"));
        assert_eq!(peer_in_login_script(&windows_vbs(EXE, None)), None);
        assert_eq!(peer_in_login_script("--peer nonsense"), None);
    }

    #[test]
    fn percent_signs_in_the_exe_path_are_doubled_in_a_desktop_entry() {
        let entry = linux_desktop_entry("/opt/100%/mesh-kvm", "--app");
        assert!(entry.contains("Exec=/opt/100%%/mesh-kvm --app\n"));
    }

    #[test]
    fn start_menu_opens_the_app() {
        let vbs = windows_app_vbs(EXE);
        assert!(vbs.contains("--app"));
        assert!(!vbs.contains("--studio"));
        assert!(vbs.contains(EXE));
    }

    #[test]
    fn desktop_entry_is_valid() {
        let entry = linux_desktop_entry("/home/me/bin/mesh-kvm", "--app");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("\nType=Application\n"));
        assert!(entry.contains("\nExec=/home/me/bin/mesh-kvm --app\n"));
        assert!(entry.contains("\nIcon=mesh-kvm\n"));
    }

    #[test]
    fn desktop_entry_quotes_a_path_with_spaces() {
        let entry = linux_desktop_entry("/home/me/my apps/mesh-kvm", "--tray");
        assert!(entry.contains("\nExec=\"/home/me/my apps/mesh-kvm\" --tray\n"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_tray_login_entry_can_be_written_by_itself() {
        let home = temp("tray-entry");
        write_tray_autostart(&home).unwrap();
        let entry = std::fs::read_to_string(home.join(".config/autostart/mesh-kvm-tray.desktop")).unwrap();
        assert!(entry.contains("--tray"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn linux_launchers_are_written_under_home() {
        let home = temp("linux");
        let written = write_linux_launchers(&home, "/opt/mesh-kvm").unwrap();
        assert_eq!(written.len(), 2 + ICON_SIZES.len());
        let menu = std::fs::read_to_string(home.join(".local/share/applications/mesh-kvm.desktop")).unwrap();
        assert!(menu.contains("Exec=/opt/mesh-kvm --app"));
        let tray = std::fs::read_to_string(home.join(".config/autostart/mesh-kvm-tray.desktop")).unwrap();
        assert!(tray.contains("Exec=/opt/mesh-kvm --tray"));
        let png = std::fs::read(home.join(".local/share/icons/hicolor/32x32/apps/mesh-kvm.png")).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_menu_entry_matches_the_windows_app_id() {
        let home = temp("app-id");
        write_linux_launchers(&home, "/opt/mesh-kvm").unwrap();
        let entry = std::fs::read_to_string(home.join(format!(".local/share/applications/{APP_ID}.desktop"))).unwrap();
        assert!(entry.contains(&format!("\nStartupWMClass={APP_ID}\n")));
        assert!(entry.contains("\nIcon=mesh-kvm\n"));
        for size in ICON_SIZES {
            let png = std::fs::read(home.join(format!(".local/share/icons/hicolor/{size}x{size}/apps/mesh-kvm.png"))).unwrap();
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        }
        let _ = std::fs::remove_dir_all(home);
    }

    fn programs(dir: &Path) -> std::path::PathBuf {
        let programs = dir.join("Programs");
        std::fs::create_dir_all(programs.join("Startup")).unwrap();
        programs
    }

    #[test]
    fn windows_launchers_replace_the_studio_script() {
        let dir = temp("win");
        let programs = programs(&dir);
        std::fs::write(programs.join("mesh-kvm Studio.vbs"), "old").unwrap();
        install_windows_launchers(&programs, EXE, None, true, &|_, _| false);
        assert!(!programs.join("mesh-kvm Studio.vbs").exists());
        assert!(std::fs::read_to_string(programs.join("mesh-kvm.vbs")).unwrap().contains("--app"));
        let login = std::fs::read_to_string(programs.join("Startup/mesh-kvm-autostart.vbs")).unwrap();
        assert!(login.contains("--tray"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_working_shortcut_replaces_the_script() {
        let dir = temp("win-lnk");
        let programs = programs(&dir);
        std::fs::write(programs.join("mesh-kvm.vbs"), "stale").unwrap();
        install_windows_launchers(&programs, EXE, None, true, &|lnk, exe| {
            assert_eq!(lnk, programs.join("mesh-kvm.lnk"));
            assert_eq!(exe, EXE);
            true
        });
        assert!(!programs.join("mesh-kvm.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_shortcut_falls_back_to_the_script() {
        let dir = temp("win-fallback");
        let programs = programs(&dir);
        install_windows_launchers(&programs, EXE, None, true, &|_, _| false);
        assert!(programs.join("mesh-kvm.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_replaced_exe_loses_the_deleted_suffix() {
        assert_eq!(
            without_deleted_suffix(PathBuf::from("/opt/mesh-kvm (deleted)")),
            PathBuf::from("/opt/mesh-kvm")
        );
        assert_eq!(without_deleted_suffix(PathBuf::from("/opt/mesh-kvm")), PathBuf::from("/opt/mesh-kvm"));
    }

    #[test]
    fn a_copy_moved_aside_as_failed_also_starts_from_the_installed_name() {
        assert_eq!(without_old_suffix(PathBuf::from(r"C:\a\mesh-kvm.exe.failed")), PathBuf::from(r"C:\a\mesh-kvm.exe"));
    }

    #[test]
    fn a_login_during_an_unproven_update_goes_through_the_old_exe() {
        let vbs = windows_vbs(EXE, None);
        assert!(vbs.contains(r#"Fso.FileExists("C:\Users\me\AppData\Local\mesh-kvm\update-pending.json")"#), "{vbs}");
        assert!(vbs.contains(&format!(r#"Fso.FileExists("{EXE}.old")"#)), "{vbs}");
        let copy = r"C:\Users\me\AppData\Local\mesh-kvm\mesh-kvm-recover.exe";
        assert!(vbs.contains(&format!(r#"Fso.CopyFile "{EXE}.old", "{copy}", True"#)), "{vbs}");
        let watcher = format!("WshShell.Run \"\"\"{copy}\"\" --update-watch --tray --role windows\", 0, False");
        assert!(vbs.contains(&watcher), "{vbs}");
        assert!(vbs.split_once("End If").unwrap().0.lines().filter(|l| l.contains("WshShell.Run")).all(|l| l.contains(".exe\"\"")), "the shell only runs an exe");
        let (recovery, normal) = vbs.split_once("End If").unwrap();
        assert!(recovery.contains("WScript.Quit"), "the normal start doesn't run as well");
        assert!(normal.contains("--role windows") && normal.contains("--tray"), "{vbs}");
    }

    #[test]
    fn a_login_during_an_update_keeps_a_pinned_address() {
        let vbs = windows_vbs(EXE, Some("192.168.8.210"));
        assert!(vbs.contains("--update-watch --tray --role windows --peer 192.168.8.210"), "{vbs}");
        assert_eq!(peer_in_login_script(&vbs).as_deref(), Some("192.168.8.210"));
    }

    #[test]
    fn the_watcher_is_given_the_service_arguments_and_starts_the_service_with_them() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(args_for_watcher(args(&["--role", "windows", "--port", "9"]), 4242), args(&["--update-watch", "--old-pid", "4242", "--role", "windows", "--port", "9"]));
        assert_eq!(args_for_watcher(vec![], 7), args(&["--update-watch", "--old-pid", "7", "--role", crate::platform::ROLE]));
        let told = args_from_watcher(args(&["--update-watch", "--old-pid", "4242", "--role", "windows", "--port", "9"]));
        assert_eq!(told, WatcherArgs { service: args(&["--role", "windows", "--port", "9"]), tray: false, old_pid: Some(4242) });
        let told = args_from_watcher(args(&["--update-watch"]));
        assert_eq!(told, WatcherArgs { service: args(&["--role", crate::platform::ROLE]), tray: false, old_pid: None });
    }

    #[test]
    fn a_watcher_from_the_login_script_has_no_old_service_to_wait_for() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let told = args_from_watcher(args(&["--update-watch", "--tray", "--role", "windows", "--peer", "192.168.8.210"]));
        assert_eq!(told, WatcherArgs { service: args(&["--role", "windows", "--peer", "192.168.8.210"]), tray: true, old_pid: None });
    }

    fn taken() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::AddrInUse)
    }

    #[test]
    fn a_replacing_service_waits_for_its_ports_to_come_free() {
        let tries = Cell::new(0);
        let waited = Cell::new(Duration::ZERO);
        let bound = retry_while_taken(
            PORTS_WAIT,
            || {
                tries.set(tries.get() + 1);
                if tries.get() < 20 { Err(taken()) } else { Ok("bound") }
            },
            &|time| waited.set(waited.get() + time),
        );
        assert_eq!(bound.unwrap(), "bound");
        assert!(waited.get() < PORTS_WAIT, "{:?}", waited.get());
    }

    #[test]
    fn a_replacing_service_gives_up_on_a_port_that_stays_taken() {
        let waited = Cell::new(Duration::ZERO);
        let bound: std::io::Result<()> = retry_while_taken(PORTS_WAIT, || Err(taken()), &|time| waited.set(waited.get() + time));
        assert_eq!(bound.unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
        assert_eq!(waited.get(), PORTS_WAIT);
    }

    #[test]
    fn a_normal_start_finds_a_taken_port_at_once() {
        let tries = Cell::new(0);
        let bound: std::io::Result<()> = retry_while_taken(Duration::ZERO, || {
            tries.set(tries.get() + 1);
            Err(taken())
        }, &|_| panic!("it waited"));
        assert!(bound.is_err());
        assert_eq!(tries.get(), 1);
    }

    #[test]
    fn only_a_taken_port_is_tried_again() {
        let tries = Cell::new(0);
        let bound: std::io::Result<()> = retry_while_taken(PORTS_WAIT, || {
            tries.set(tries.get() + 1);
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }, &|_| {});
        assert_eq!(bound.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(tries.get(), 1);
    }

    #[test]
    fn a_new_copy_starts_from_the_installed_name_and_not_the_one_moved_aside() {
        assert_eq!(without_old_suffix(PathBuf::from(r"C:\a\mesh-kvm.exe.old")), PathBuf::from(r"C:\a\mesh-kvm.exe"));
        assert_eq!(without_old_suffix(PathBuf::from("/usr/bin/mesh-kvm")), PathBuf::from("/usr/bin/mesh-kvm"));
    }

    #[test]
    fn the_login_script_is_written_before_the_shortcut_is_made() {
        let dir = temp("win-order");
        let programs = programs(&dir);
        install_windows_launchers(&programs, EXE, None, true, &|_, _| {
            assert!(programs.join("Startup/mesh-kvm-autostart.vbs").exists());
            true
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    fn fake_lnk(path: &Path, exe: &str) {
        let wide: Vec<u8> = exe.encode_utf16().flat_map(u16::to_le_bytes).collect();
        std::fs::write(path, [b"L\0\0\0 junk ".as_slice(), &wide, b" more"].concat()).unwrap();
    }

    #[test]
    fn a_shortcut_that_already_points_at_the_exe_is_left_alone() {
        let dir = temp("win-skip");
        let programs = programs(&dir);
        fake_lnk(&programs.join("mesh-kvm.lnk"), EXE);
        std::fs::write(programs.join("mesh-kvm.vbs"), "stale").unwrap();
        install_windows_launchers(&programs, EXE, None, true, &|_, _| panic!("PowerShell ran"));
        assert!(!programs.join("mesh-kvm.vbs").exists());
        assert!(programs.join("mesh-kvm.lnk").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_shortcut_to_another_path_is_made_again() {
        let dir = temp("win-moved");
        let programs = programs(&dir);
        fake_lnk(&programs.join("mesh-kvm.lnk"), r"C:\old\mesh-kvm.exe");
        let made = std::cell::Cell::new(false);
        install_windows_launchers(&programs, EXE, None, true, &|_, _| {
            made.set(true);
            true
        });
        assert!(made.get());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_autostart_writes_no_startup_script() {
        let dir = temp("win-off");
        let programs = programs(&dir);
        install_windows_launchers(&programs, EXE, None, false, &|_, _| false);
        assert!(programs.join("mesh-kvm.vbs").exists());
        assert!(!programs.join("Startup/mesh-kvm-autostart.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn turning_autostart_off_removes_a_login_script_already_there() {
        let dir = temp("win-off-again");
        let programs = programs(&dir);
        install_windows_launchers(&programs, EXE, None, true, &|_, _| true);
        assert!(programs.join("Startup/mesh-kvm-autostart.vbs").exists());
        install_windows_launchers(&programs, EXE, None, false, &|_, _| true);
        assert!(!programs.join("Startup/mesh-kvm-autostart.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
