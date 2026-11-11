#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]

mod app;
mod audio;
mod clipboard;
mod displays;
mod keymap;
mod input;
mod licenses;
mod linkdetails;
mod linklog;
mod lowlat;
mod mute;
mod platform;
mod protocol;
mod quit;
mod router;
mod secure;
mod settings;
mod spatial;
mod update;
mod webui;

use clap::Parser;
use protocol::{
    NodeId, SwitchEdge, DEFAULT_AUDIO_PORT, DEFAULT_CLIPBOARD_PORT, DEFAULT_INPUT_PORT,
};
use std::net::SocketAddr;

/// The exit status of a service that found another one running. The packaged unit doesn't restart
/// on this one, or it would keep trying until systemd gave up on it.
const ALREADY_RUNNING_STATUS: i32 = 75;

#[derive(Parser, Debug)]
#[command(
    name = "mesh-kvm",
    version,
    about = "Zero-latency P2P Mesh Input, Clipboard, and Audio daemon bridging Debian 13 and Windows 11"
)]
struct Cli {
    /// Node role: 'debian' or 'windows'
    #[arg(long, default_value = platform::ROLE)]
    role: NodeId,

    /// Peer IP address (or IP:PORT). Saved, and used in place of the saved one while given
    #[arg(long)]
    peer: Option<String>,

    /// Side of this device the peer sits on, for a new layout ('left', 'right', 'top', 'bottom')
    #[arg(long)]
    switch_edge: Option<SwitchEdge>,

    /// UDP port for asymmetric input mesh (default: 24850)
    #[arg(long, default_value_t = DEFAULT_INPUT_PORT)]
    port: u16,

    /// Optional screen width override
    #[arg(long)]
    width: Option<i32>,

    /// Optional screen height override
    #[arg(long)]
    height: Option<i32>,

    /// Optional explicit Linux mouse evdev path (e.g. /dev/input/event10)
    #[arg(long)]
    mouse_device: Option<String>,

    /// Phase 2: Stream system audio to peer IP over UDP 24851
    #[arg(long)]
    audio_send_to: Option<String>,

    /// Phase 2: Listen for incoming UDP audio stream on port 24851 (enabled by default)
    #[arg(long, default_value_t = true)]
    audio_listen: bool,

    /// Phase 2: UDP port for audio bus (default: 24851)
    #[arg(long, default_value_t = DEFAULT_AUDIO_PORT)]
    audio_port: u16,

    /// Phase 3: Enable unified clipboard sync over TCP 24852
    #[arg(long, default_value_t = true)]
    clipboard: bool,

    /// Phase 3: TCP port for clipboard sync (default: 24852)
    #[arg(long, default_value_t = DEFAULT_CLIPBOARD_PORT)]
    clipboard_port: u16,

    /// Ask the running service to show a code for pairing, print it, and wait for the result
    #[arg(long)]
    pair_show: bool,

    /// Enter the code the other device shows, through the running service
    #[arg(long, value_name = "CODE")]
    pair: Option<String>,

    /// Print the link log of this device, then the other device's, through the running service
    #[arg(long)]
    link_log: bool,

    /// Open the app window for the service on this device, and exit when it closes
    #[arg(long, alias = "studio")]
    app: bool,

    /// Show the tray icon for the service on this device. Its Quit stops mesh-kvm on this device
    #[arg(long)]
    tray: bool,

    /// Stop mesh-kvm on this device: the window, the tray and the service, then exit
    #[arg(long)]
    quit: bool,

    /// Show the licences of what's built into this program, then exit
    #[arg(long, alias = "licences")]
    licenses: bool,

    /// Print what's on the clipboard, for the service to read when an app has stopped answering
    #[cfg(target_os = "linux")]
    #[arg(long, hide = true)]
    read_clipboard: bool,

    /// See a new exe through its first minute, and put the old one back if it fails
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    update_watch: bool,

    /// The service a watcher waits for before it starts the new one
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    old_pid: Option<u32>,

    /// Write the menu and login launchers, then exit. On Windows, mesh-kvm-setup.exe runs this
    #[arg(long)]
    install_launchers: bool,

    /// With --install-launchers on Windows: start mesh-kvm at sign-in, 'on' or 'off'
    #[cfg(target_os = "windows")]
    #[arg(long, value_parser = ["on", "off"])]
    autostart: Option<String>,
}

fn parse_peer_socket_addr(peer: &str, default_port: u16) -> Result<(String, SocketAddr), String> {
    let trimmed = peer.trim();
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Ok((addr.ip().to_string(), addr));
    }
    let with_port = format!("{}:{}", trimmed, default_port);
    let addr = with_port
        .parse::<SocketAddr>()
        .map_err(|e| format!("Invalid peer address '{}': {}", peer, e))?;
    Ok((addr.ip().to_string(), addr))
}

/// The address to use, and whether `--peer` set it: the flag wins, then what was saved. With
/// neither, the service has no peer yet.
fn choose_peer(
    flag: Option<&str>,
    saved: Option<&str>,
    port: u16,
) -> Result<Option<(String, SocketAddr, bool)>, String> {
    let Some(text) = flag.or(saved) else {
        return Ok(None);
    };
    let (ip, addr) = parse_peer_socket_addr(text, port)?;
    Ok(Some((ip, addr, flag.is_some())))
}

/// True when the program was started with no arguments.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn is_bare_launch(arg_count: usize) -> bool {
    arg_count <= 1
}

/// What a bare Windows launch does.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum BarePlan {
    /// A copy run from somewhere else, such as Downloads, while mesh-kvm is installed: it opens the
    /// installed copy's window and ends. It never installs itself over that copy.
    OpenInstalled,
    /// Opens the tray and the window, and this process becomes the service unless one is running.
    /// A copy with nothing installed runs like this too, but installs nothing.
    Run { become_service: bool },
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn bare_launch_plan(is_installed_copy: bool, installed_exists: bool, service_running: bool) -> BarePlan {
    if !is_installed_copy && installed_exists {
        return BarePlan::OpenInstalled;
    }
    BarePlan::Run { become_service: !service_running }
}

/// What to tell the person when the window can't open, such as when there is no OpenGL.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn window_failed_text(log: &std::path::Path) -> String {
    format!(
        "The mesh-kvm window couldn't start. The sharing itself isn't affected.\n\nThe reason is in the log:\n{}",
        log.display()
    )
}

#[cfg(target_os = "windows")]
fn show_message_box(text: &str) {
    show_box(text, windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR);
}

#[cfg(target_os = "windows")]
fn show_box(text: &str, icon: windows_sys::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("mesh-kvm").as_ptr(), MB_OK | icon) };
}

/// Runs the app window, or raises the one that is already open. Returns when the window closes.
/// It starts the tray if none is running, so there is a way back once the window is closed.
fn run_app_window() {
    match app::single::claim_after_handover(app::single::WINDOW_PORT) {
        app::single::Instance::AlreadyRunning => {
            // Windows lets only the foreground process hand the foreground on, and that is this one.
            #[cfg(target_os = "windows")]
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
                    windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY,
                )
            };
            app::single::signal_show(app::single::WINDOW_PORT);
            return;
        }
        app::single::Instance::First(listener) => app::single::keep_for_window(listener),
        app::single::Instance::Failed(e) => {
            eprintln!("Error: {}", e);
            settings::log_fatal(&e);
            std::process::exit(1);
        }
    }
    if !app::single::is_held(app::single::TRAY_PORT) {
        if let Err(e) = app::launch::spawn_self("--tray") {
            settings::log_fatal(&format!("Couldn't start mesh-kvm --tray: {e}"));
        }
    }
    if let Err(e) = app::ui::run_window(app::launch::ServiceStarter::begin()) {
        eprintln!("Error: {}", e);
        settings::log_fatal(&e);
        // A window started from a menu has nowhere to print, so say it in a box.
        #[cfg(target_os = "windows")]
        show_message_box(&window_failed_text(&settings::log_path()));
        std::process::exit(1);
    }
}

/// Runs the tray icon until Quit. It never touches the service or input.
fn run_tray_icon() {
    if let Err(e) = app::tray::run_tray() {
        eprintln!("Error: {}", e);
        settings::log_fatal(&e);
        std::process::exit(1);
    }
}

fn main() {
    let cli = Cli::parse();

    #[cfg(target_os = "linux")]
    if cli.read_clipboard {
        clipboard::print_snapshot();
        return;
    }

    #[cfg(target_os = "windows")]
    if cli.update_watch {
        update::windows::run_watcher();
        return;
    }

    if cli.licenses {
        println!("{}", licenses::TEXT);
        // A Windows program with a window has nowhere to print, unless its output is redirected.
        #[cfg(target_os = "windows")]
        show_box(licenses::TEXT, windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONINFORMATION);
        return;
    }

    if cli.quit {
        let quit = quit::everything_here();
        println!("{quit:?}");
        std::process::exit(if quit == quit::Quit::StillRunning { 1 } else { 0 });
    }

    // Double-clicking the Windows exe passes no arguments. It opens the tray and the window, and
    // becomes the service unless one is already running; the login launcher always passes arguments.
    #[cfg(target_os = "windows")]
    if is_bare_launch(std::env::args_os().len()) {
        let service_running = webui::http_call("127.0.0.1", "GET", "/api/local", "").is_ok();
        let installed = app::install::installed_exe().filter(|exe| exe.exists());
        let plan = bare_launch_plan(app::install::runs_from_installed(), installed.is_some(), service_running);
        if let (BarePlan::OpenInstalled, Some(installed)) = (&plan, installed) {
            if let Err(e) = app::launch::spawn_exe(installed.clone(), &["--app"], &[]) {
                settings::log_fatal(&format!("Couldn't open the installed mesh-kvm at {}: {e}", installed.display()));
            }
            return;
        }
        for flag in ["--tray", "--app"] {
            if let Err(e) = app::launch::spawn_self(flag) {
                settings::log_fatal(&format!("Couldn't start mesh-kvm {flag}: {e}"));
            }
        }
        if plan == (BarePlan::Run { become_service: false }) {
            return;
        }
    }

    if cli.pair_show || cli.pair.is_some() {
        std::process::exit(run_pair_command(&cli));
    }
    if cli.link_log {
        std::process::exit(run_link_log());
    }

    if cli.app {
        run_app_window();
        return;
    }
    if cli.tray {
        run_tray_icon();
        return;
    }
    if cli.install_launchers {
        #[cfg(target_os = "linux")]
        let installed = app::launch::install_linux_launchers();
        #[cfg(target_os = "windows")]
        let installed = platform::windows::install_for_setup(cli.autostart.as_deref().map(|a| a == "on"));
        if let Err(e) = installed {
            eprintln!("Error: {}", e);
            settings::log_fatal(&e);
            std::process::exit(1);
        }
        return;
    }

    run_service(cli);
}

/// Stops and leaves a note where the app can point to.
fn fail(message: &str) -> ! {
    // A new copy that fails hasn't proved itself, however long stopping takes.
    update::note_service_failing();
    linklog::stop("failed", Some(message));
    eprintln!("Error: {}", message);
    settings::log_fatal(message);
    mute::shutdown();
    std::process::exit(1);
}

/// Replaces this process with a fresh service, as the peer changed.
fn restart() -> ! {
    let problem = app::launch::restart_service();
    fail(&format!("mesh-kvm couldn't start itself again: {problem}"));
}

/// A Windows service that a restart started has to wait for the one before it to end.
fn wait_for_old_service() {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    if std::env::var_os(app::launch::RESTARTING_ENV).is_none() {
        return;
    }
    let started = std::time::Instant::now();
    while started.elapsed() < WAIT && webui::http_call("127.0.0.1", "GET", "/api/local", "").is_ok() {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Pairing as the running service does it. It needs no peer address, and no channels.
fn pairing_service(secure: &secure::Services, pinned: bool) -> secure::pairing::PairingService {
    use secure::pairing::{Hooks, PairingService, PAIRING_PORT, PEER_UNPAIRED_BODY};
    let hooks = Hooks {
        name: Box::new(settings::device_name),
        peer_ip: Box::new(|| settings::peer_ip().and_then(|ip| ip.parse().ok())),
        pinned: Box::new(move || pinned),
        learn_address: Box::new(|ip| settings::learn_peer(&ip.to_string())),
        tell_peer: Box::new(|| {
            // Tried even with the input link down: the other device should hear it if it can.
            secure::control::peer_call_any_link("POST", "/api/local/action", PEER_UNPAIRED_BODY).map(|_| ())
        }),
        restart: Box::new(settings::request_restart),
        security: Box::new(|| secure::services().input.security()),
    };
    PairingService::new(secure::keys::pairing_path(), secure.pairing.as_deref().cloned(), secure.problem.clone(), PAIRING_PORT, hooks)
}

/// How the pairing flags talk to the running service: one request, and what came back.
type Call<'a> = &'a mut dyn FnMut(&str, &str, &str) -> Result<(u16, String), String>;

/// How long to wait for the result, in polls. A code lasts 120 seconds.
const PAIR_POLLS: u32 = 800;
const PAIR_POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(200);

fn run_pair_command(cli: &Cli) -> i32 {
    let body = match &cli.pair {
        Some(code) => serde_json::json!({ "action": "pair_enter", "code": code }),
        None => serde_json::json!({ "action": "pair_show" }),
    };
    let mut call = |method: &str, path: &str, body: &str| webui::http_call("127.0.0.1", method, path, body);
    #[cfg(target_os = "windows")]
    let mut say = windows_output();
    #[cfg(not(target_os = "windows"))]
    let mut say = |line: &str| println!("{line}");
    pair_through(&mut call, &body.to_string(), cli.pair.is_none(), &mut say, &|| std::thread::sleep(PAIR_POLL_EVERY))
}

/// Where a Windows build prints: redirected output is left alone, then the calling terminal is
/// attached. It has no console of its own, so there may be neither.
#[cfg(target_os = "windows")]
fn windows_console() -> Option<Box<dyn FnMut(&str)>> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
    let out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if !out.is_null() && out != INVALID_HANDLE_VALUE {
        return Some(Box::new(|line| println!("{line}")));
    }
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0 {
        use std::io::Write;
        if let Ok(mut console) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            return Some(Box::new(move |line| {
                let _ = writeln!(console, "{line}");
            }));
        }
    }
    None
}

/// Where a Windows build says things: the console, and with none the lines go in message boxes.
#[cfg(target_os = "windows")]
fn windows_output() -> Box<dyn FnMut(&str)> {
    if let Some(console) = windows_console() {
        return console;
    }
    let icon = windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONINFORMATION;
    // A box waits to be closed. The code's stays up on its own thread while pairing goes on, and
    // the result is waited on so that it is read before the program ends.
    Box::new(move |line| {
        let waiting_for_result = !line.starts_with("Code:") && !line.starts_with("Enter it");
        let line = line.to_string();
        let shown = std::thread::spawn(move || show_box(&line, icon));
        if waiting_for_result {
            let _ = shown.join();
        }
    })
}

/// For this device's service, and for the other device's answer to come through the proxy, which
/// waits out a slow link: up to 3 seconds to connect and 10 for the answer.
const LINK_LOG_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
const PEER_LINK_LOG_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

fn run_link_log() -> i32 {
    let mut call = |method: &str, path: &str, body: &str| {
        let wait = if path.starts_with("/api/peer/") { PEER_LINK_LOG_WAIT } else { LINK_LOG_WAIT };
        webui::http_call_port_waiting("127.0.0.1", webui::UI_PORT, method, path, body, wait)
    };
    let (text, status) = link_log_text(&mut call, &|| linklog::file_tail(linklog::DEFAULT_LINES));
    #[cfg(target_os = "windows")]
    match windows_console() {
        Some(mut say) => say(text.trim_end()),
        // Too long for a box, so it goes in a file the box names.
        None => {
            let path = settings::config_dir().join("link-logs.txt");
            let said = match std::fs::write(&path, &text) {
                Ok(()) => format!("The link logs of both devices are in {}.", path.display()),
                Err(e) => format!("Couldn't write the link logs to {}: {e}", path.display()),
            };
            show_box(&said, windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONINFORMATION);
        }
    }
    #[cfg(not(target_os = "windows"))]
    print!("{text}");
    status
}

/// This device's link log, then the other device's, each under a heading, and the exit status: 0
/// when both were read. With the service down, this device's file is read instead.
fn link_log_text(call: Call, own_file: &dyn Fn() -> Option<String>) -> (String, i32) {
    let path = format!("{}?lines={}", linklog::PATH, linklog::DEFAULT_LINES);
    let or_nothing = |body: String| if body.is_empty() { "Nothing logged yet.\n".to_string() } else { body };
    let mut text = String::from("=== This device ===\n");
    let mut status = 0;
    let running = match call("GET", &path, "") {
        Ok((200, body)) => {
            text.push_str(&or_nothing(body));
            true
        }
        Ok((code, _)) => {
            text.push_str(&format!("The service refused ({code}). It may be a version without a link log.\n"));
            status = 1;
            true
        }
        Err(_) => {
            text.push_str(&match own_file() {
                Some(file) => format!("mesh-kvm isn't running on this device. Its link log file:\n{}", or_nothing(file)),
                None => "mesh-kvm isn't running on this device, and has no link log file yet.\n".to_string(),
            });
            status = 1;
            false
        }
    };
    text.push_str("\n=== The other device ===\n");
    if !running {
        text.push_str("The service on this device isn't running, so the other device can't be asked.\n");
        return (text, 1);
    }
    match call("GET", &format!("/api/peer{path}"), "") {
        Ok((200, body)) => text.push_str(&or_nothing(body)),
        Ok((404, _)) => {
            text.push_str("The other device has no link log: it runs an older version.\n");
            status = 1;
        }
        Ok((502, body)) => {
            let said = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default();
            let error = said["error"].as_str().unwrap_or("no answer");
            text.push_str(&format!("The other device can't be reached: {error}\n"));
            status = 1;
        }
        Ok((code, _)) => {
            text.push_str(&format!("The other device refused ({code}).\n"));
            status = 1;
        }
        Err(e) => {
            text.push_str(&format!("The service on this device stopped answering: {e}\n"));
            status = 1;
        }
    }
    (text, status)
}

fn said_paired(say: &mut dyn FnMut(&str), status: &serde_json::Value) -> i32 {
    let name = status["peer_name"].as_str().unwrap_or("the other device");
    say(&format!("Paired with {name}. Fingerprint {}.", status["fingerprint"].as_str().unwrap_or("")));
    0
}

/// Asks the service to start, says what happens, and returns the exit status: 0 for a pairing.
fn pair_through(call: Call, request: &str, showing: bool, say: &mut dyn FnMut(&str), wait: &dyn Fn()) -> i32 {
    let started = match call("POST", "/api/local/action", request) {
        Ok((200, body)) => serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default(),
        Ok((status, _)) => {
            say(&format!("The service refused ({status})."));
            return 1;
        }
        Err(_) => {
            say("mesh-kvm isn't running on this device. Start it, then try again.");
            return 1;
        }
    };
    if started["ok"] != true {
        say(started["error"].as_str().unwrap_or("The service couldn't start pairing."));
        return 1;
    }
    let mut shown = false;
    // The service starts again when a pairing is saved, and may do so before this hears of it.
    let mut lost = false;
    for _ in 0..PAIR_POLLS {
        let status = match call("GET", "/api/pairing", "") {
            Ok((200, body)) => serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default(),
            _ => {
                lost = true;
                wait();
                continue;
            }
        };
        match status["state"].as_str().unwrap_or("") {
            "showing" if showing => {
                if !shown {
                    shown = true;
                    let code = status["code"].as_str().unwrap_or("");
                    say(&format!("Code: {} {}", code.get(..3).unwrap_or(""), code.get(3..).unwrap_or("")));
                    say("Enter it on the other device within 2 minutes.");
                }
            }
            "paired" => return said_paired(say, &status),
            "failed" => {
                say(status["error"].as_str().unwrap_or("Pairing failed."));
                return 1;
            }
            // A service that started again after saving has a peer and nothing going on.
            "idle" if lost && status["peer_name"].is_string() => return said_paired(say, &status),
            "idle" => {
                say("Pairing was cancelled.");
                return 1;
            }
            _ => {}
        }
        wait();
    }
    say("Gave up waiting for the result.");
    1
}

/// Runs the service until it is told to stop.
fn run_service(cli: Cli) {
    #[cfg(feature = "test-hooks")]
    update::trust::hang_if_told();
    wait_for_old_service();

    // First of all, so that a second service stops here and touches nothing. One that replaces
    // another waits for the port, as the old one may still be letting go of it.
    let listener = match app::launch::bind_patiently(webui::bind) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            println!("mesh-kvm is already running on this device.");
            std::process::exit(ALREADY_RUNNING_STATUS);
        }
        Err(e) => fail(&format!("Couldn't open port {}: {}", webui::UI_PORT, e)),
    };

    settings::load_and_apply();
    let secure = secure::services();
    if let Some(problem) = &secure.problem {
        eprintln!("[Secure] Working alone: {problem}");
    }
    let saved = settings::peer_ip().or_else(old_install_peer);
    let chosen = match choose_peer(cli.peer.as_deref(), saved.as_deref(), cli.port) {
        Ok(chosen) => chosen,
        Err(e) => fail(&e),
    };
    settings::remember_start_flags(chosen.as_ref().map(|(ip, ..)| ip.as_str()), cli.switch_edge);

    let peer_text = chosen.as_ref().map_or_else(String::new, |(_, addr, _)| addr.to_string());
    let peer_ip = chosen.as_ref().map_or_else(String::new, |(ip, ..)| ip.clone());
    let pinned = chosen.as_ref().is_some_and(|c| c.2);
    println!(
        "=== mesh-kvm v{} | Role: {} | Peer: {} ===",
        update::trust::running_version(),
        cli.role,
        if peer_text.is_empty() { "none yet" } else { &peer_text },
    );

    linklog::start(&peer_text);

    let platform = platform::current(cli.width.zip(cli.height));
    let state = spatial::SharedSpatialState::load(platform.as_ref(), peer_text, pinned);
    let state = std::sync::Arc::new(std::sync::RwLock::new(state));
    let displays = spatial::start_displays(state.clone(), platform.clone());
    let trial = displays.clone();
    update::start(Box::new(move || trial.trial_running()));
    let pairing = std::sync::Arc::new(pairing_service(secure, pinned));
    let handler = spatial::studio_handler(state.clone(), platform.clone(), displays, pairing);
    if let Err(e) = webui::start(listener, peer_ip.clone(), handler.clone()) {
        fail(&e);
    }

    let Some((_, peer_addr, _)) = chosen else {
        // Nothing to share with yet, so nothing is captured. The app asks for the address.
        println!("No other device is known yet. Waiting for its address from the app.");
        update::note_service_up();
        while !settings::restart_requested() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        restart();
    };

    spatial::start_sync(state.clone(), platform);
    // Wi-Fi streaming mode and packet priority, while there is someone to link with.
    lowlat::start();

    // Phase 2: Bidirectional Audio Bus (UDP 24851)
    let audio_target_str = cli
        .audio_send_to
        .clone()
        .unwrap_or_else(|| peer_ip.clone());
    match parse_peer_socket_addr(&audio_target_str, cli.audio_port) {
        Ok((_, audio_addr)) => {
            if cli.audio_listen {
                if let Err(e) = audio::start_audio_listener(cli.audio_port, audio_addr, secure.audio_in.clone()) {
                    eprintln!("[Audio] Warning: failed to start audio listener: {}", e);
                }
            }
            if let Err(e) = audio::start_audio_sender(audio_addr, secure.audio_out.clone()) {
                eprintln!("[Audio] Warning: failed to start audio sender: {}", e);
            }
        }
        Err(e) => eprintln!("[Audio] Invalid audio target address: {}", e),
    }

    // Phase 3: the control listener (TCP 24852) and the unified clipboard over it. The listener runs
    // with the clipboard off too, as the peer API needs it. Unpaired, neither has anyone to talk to.
    match secure.pairing.as_ref().filter(|p| p.peer.is_some()) {
        Some(pairing) => {
            match peer_ip.parse::<std::net::IpAddr>() {
                Ok(ip) => {
                    secure::control::set_target(std::net::SocketAddr::new(ip, cli.clipboard_port));
                    let api = handler.clone();
                    let incoming: secure::control::Incoming = std::sync::Arc::new(move |stream, purpose| match purpose {
                        secure::stream::Purpose::Clipboard => clipboard::take_incoming(stream),
                        secure::stream::Purpose::Api => secure::control::serve_api(stream, api.clone()),
                    });
                    if let Err(e) = secure::control::start(cli.clipboard_port, ip, pairing.clone(), incoming) {
                        eprintln!("[Control] Warning: failed to start the control listener: {}", e);
                    }
                }
                Err(e) => eprintln!("[Control] Bad peer address {peer_ip}: {e}"),
            }
            if cli.clipboard {
                if let Err(e) = clipboard::start_clipboard_sync(peer_ip, cli.clipboard_port) {
                    eprintln!("[Clipboard] Warning: failed to start clipboard sync: {}", e);
                }
            }
        }
        None => println!("[Control] Not paired, so the clipboard stays on this device."),
    }

    // Phase 1: Asymmetric Input & Screen-Edge Focus Engine (UDP 24850)
    #[cfg(target_os = "linux")]
    {
        if cli.role != NodeId::Debian {
            eprintln!(
                "Warning: Running on Linux with --role {}; proceeding with Linux input engine.",
                cli.role
            );
        }
        if let Err(e) = input::linux::run_debian_node(
            state,
            peer_addr,
            secure.input.clone(),
            cli.port,
            cli.width,
            cli.height,
            cli.mouse_device,
        ) {
            fail(&format!("[Debian] {e}"));
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = &cli.mouse_device;
        if cli.role != NodeId::Windows {
            eprintln!(
                "Warning: Running on Windows with --role {}; proceeding with Windows input engine.",
                cli.role
            );
        }
        if let Err(e) = input::windows::run_windows_node(state, peer_addr, secure.input.clone(), cli.port, cli.width, cli.height) {
            fail(&format!("[Windows] {e}"));
        }
    }

    mute::shutdown();
    if settings::restart_requested() {
        restart();
    }
    linklog::stop("ended", None);
}

/// An address left by an install from before addresses were saved.
fn old_install_peer() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        platform::windows::old_login_script_peer()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{is_bare_launch, protocol, Cli};
    use clap::Parser;

    #[test]
    fn a_window_that_cannot_start_says_where_the_log_is() {
        let text = super::window_failed_text(std::path::Path::new(r"C:\Users\me\mesh-kvm\mesh-kvm.log"));
        assert!(text.contains("window couldn't start"), "{text}");
        assert!(text.contains(r"C:\Users\me\mesh-kvm\mesh-kvm.log"), "{text}");
    }

    #[test]
    fn the_packaged_unit_does_not_restart_a_service_that_found_another_running() {
        let unit = include_str!("../packaging/deb/mesh-kvm.service");
        let line = format!("RestartPreventExitStatus={}\n", super::ALREADY_RUNNING_STATUS);
        assert!(unit.contains(&line), "{unit}");
    }

    #[test]
    fn peer_and_edge_flags_are_optional() {
        let cli = Cli::try_parse_from(["mesh-kvm"]).unwrap();
        assert_eq!((cli.peer, cli.switch_edge), (None, None));
        let cli = Cli::try_parse_from(["mesh-kvm", "--peer", "10.0.0.5", "--switch-edge", "top"]).unwrap();
        assert_eq!(cli.peer.as_deref(), Some("10.0.0.5"));
        assert_eq!(cli.switch_edge, Some(protocol::SwitchEdge::Top));
    }

    #[test]
    fn the_flag_wins_then_the_saved_peer_and_with_neither_there_is_none() {
        let port = 24850;
        let (ip, addr, pinned) = super::choose_peer(Some("10.0.0.5"), Some("10.0.0.6"), port).unwrap().unwrap();
        assert_eq!((ip.as_str(), addr.port(), pinned), ("10.0.0.5", port, true));
        let (ip, _, pinned) = super::choose_peer(None, Some("10.0.0.6"), port).unwrap().unwrap();
        assert_eq!((ip.as_str(), pinned), ("10.0.0.6", false));
        assert_eq!(super::choose_peer(None, None, port), Ok(None));
        // A port on the flag is kept, and a mistake on it stops the start.
        assert_eq!(super::choose_peer(Some("10.0.0.5:9"), None, port).unwrap().unwrap().1.port(), 9);
        assert!(super::choose_peer(Some("nonsense"), Some("10.0.0.6"), port).is_err());
    }

    #[test]
    fn bare_launch_has_no_arguments() {
        assert!(is_bare_launch(1));
    }

    #[test]
    fn studio_flag_opens_the_app_window() {
        assert!(Cli::try_parse_from(["mesh-kvm", "--app"]).unwrap().app);
        assert!(Cli::try_parse_from(["mesh-kvm", "--studio"]).unwrap().app);
        assert!(!Cli::try_parse_from(["mesh-kvm"]).unwrap().app);
    }

    #[test]
    fn a_copy_run_from_elsewhere_opens_the_installed_one_and_never_installs_itself() {
        use super::{bare_launch_plan, BarePlan};
        assert_eq!(bare_launch_plan(false, true, false), BarePlan::OpenInstalled);
        assert_eq!(bare_launch_plan(false, true, true), BarePlan::OpenInstalled);
    }

    #[test]
    fn with_nothing_installed_a_copy_runs_by_itself() {
        use super::{bare_launch_plan, BarePlan};
        assert_eq!(bare_launch_plan(false, false, false), BarePlan::Run { become_service: true });
        assert_eq!(bare_launch_plan(false, false, true), BarePlan::Run { become_service: false });
    }

    #[test]
    fn the_installed_copy_opens_the_tray_and_window_and_starts_the_service_if_needed() {
        use super::{bare_launch_plan, BarePlan};
        assert_eq!(bare_launch_plan(true, true, false), BarePlan::Run { become_service: true });
        assert_eq!(bare_launch_plan(true, true, true), BarePlan::Run { become_service: false });
    }

    #[test]
    fn quit_is_a_flag_of_its_own() {
        assert!(Cli::try_parse_from(["mesh-kvm", "--quit"]).unwrap().quit);
        assert!(!Cli::try_parse_from(["mesh-kvm"]).unwrap().quit);
    }

    #[test]
    fn the_pair_flags_are_parsed() {
        let cli = Cli::try_parse_from(["mesh-kvm", "--pair-show"]).unwrap();
        assert!(cli.pair_show && cli.pair.is_none());
        let cli = Cli::try_parse_from(["mesh-kvm", "--pair", "482 917"]).unwrap();
        assert_eq!(cli.pair.as_deref(), Some("482 917"));
        assert!(!cli.pair_show);
    }

    /// Plays the service: `answers` are what `/api/pairing` says in turn, after the action's reply.
    fn pair_run(action_reply: Result<(u16, String), String>, answers: &[Result<(u16, String), String>], showing: bool) -> (i32, Vec<String>) {
        let mut queue = answers.to_vec().into_iter();
        let mut call = |method: &str, path: &str, _: &str| match (method, path) {
            ("POST", "/api/local/action") => action_reply.clone(),
            ("GET", "/api/pairing") => queue.next().unwrap_or(Ok((200, r#"{"state":"idle","peer_name":null}"#.into()))),
            _ => Err("unexpected".into()),
        };
        let mut said = Vec::new();
        let status = super::pair_through(&mut call, "{}", showing, &mut |line| said.push(line.to_string()), &|| {});
        (status, said)
    }

    fn ok(body: &str) -> Result<(u16, String), String> {
        Ok((200, body.to_string()))
    }

    #[test]
    fn pair_show_prints_the_code_once_and_exits_0_on_success() {
        let showing = r#"{"state":"showing","code":"482917","seconds_left":100}"#;
        let (status, said) = pair_run(
            ok(r#"{"ok":true}"#),
            &[ok(showing), ok(showing), ok(r#"{"state":"paired","peer_name":"Laptop","fingerprint":"4F2A-91C3"}"#)],
            true,
        );
        assert_eq!(status, 0);
        assert_eq!(said.iter().filter(|l| l.contains("482 917")).count(), 1, "{said:?}");
        assert!(said.last().unwrap().contains("Laptop") && said.last().unwrap().contains("4F2A-91C3"));
    }

    #[test]
    fn a_failed_pairing_exits_non_zero_with_the_reason() {
        let (status, said) = pair_run(
            ok(r#"{"ok":true}"#),
            &[ok(r#"{"state":"entering"}"#), ok(r#"{"state":"failed","error":"The code was wrong."}"#)],
            false,
        );
        assert_eq!(status, 1);
        assert_eq!(said, ["The code was wrong."]);
    }

    #[test]
    fn a_refusal_or_no_service_exits_non_zero() {
        let (status, said) = pair_run(ok(r#"{"ok":false,"error":"Set the other device's address first."}"#), &[], false);
        assert_eq!((status, said.as_slice()), (1, ["Set the other device's address first.".to_string()].as_slice()));
        let (status, said) = pair_run(Err("refused".into()), &[], true);
        assert_eq!(status, 1);
        assert!(said[0].contains("isn't running"));
    }

    #[test]
    fn a_service_that_restarts_after_saving_counts_as_paired() {
        let showing = r#"{"state":"showing","code":"000123"}"#;
        let (status, _) = pair_run(
            ok(r#"{"ok":true}"#),
            &[ok(showing), Err("gone".into()), Err("gone".into()), ok(r#"{"state":"idle","peer_name":"Laptop","fingerprint":"AAAA-BBBB"}"#)],
            true,
        );
        assert_eq!(status, 0);
        // Idle with no loss in between is a cancel.
        let (status, _) = pair_run(ok(r#"{"ok":true}"#), &[ok(showing), ok(r#"{"state":"idle","peer_name":"Laptop"}"#)], true);
        assert_eq!(status, 1);
    }

    /// What `--link-log` says, with each path answered by `answers` and `file` for the file here.
    fn link_log_run(answers: &[(&str, Result<(u16, String), String>)], file: Option<&str>) -> (String, i32) {
        let mut asked = Vec::new();
        let mut call = |method: &str, path: &str, _body: &str| {
            assert_eq!(method, "GET");
            asked.push(path.to_string());
            answers.iter().find(|(p, _)| *p == path).map_or(Err("not asked for".to_string()), |(_, a)| a.clone())
        };
        let said = super::link_log_text(&mut call, &|| file.map(str::to_string));
        let peer_asked = asked.iter().any(|p| p.starts_with("/api/peer/"));
        assert!(asked.iter().all(|p| p.ends_with("/api/local/linklog?lines=500")), "{asked:?}");
        assert_eq!(peer_asked, answers.iter().any(|(p, _)| p.starts_with("/api/peer/")), "{asked:?}");
        said
    }

    const HERE: &str = "/api/local/linklog?lines=500";
    const THERE: &str = "/api/peer/api/local/linklog?lines=500";

    #[test]
    fn link_log_prints_both_devices_under_headings() {
        let (text, status) = link_log_run(&[(HERE, ok("a up\n")), (THERE, ok("b down\n"))], None);
        assert_eq!(status, 0);
        assert_eq!(text, "=== This device ===\na up\n\n=== The other device ===\nb down\n");
    }

    #[test]
    fn link_log_says_plainly_what_could_not_be_reached() {
        let unreachable = Ok((502, r#"{"error":"peer unreachable: timed out","peer":"192.168.8.210"}"#.to_string()));
        let (text, status) = link_log_run(&[(HERE, ok("a up\n")), (THERE, unreachable)], None);
        assert_eq!(status, 1);
        assert!(text.ends_with("=== The other device ===\nThe other device can't be reached: peer unreachable: timed out\n"), "{text}");
        let (text, _) = link_log_run(&[(HERE, ok("")), (THERE, Ok((404, "{}".into())))], None);
        assert!(text.contains("Nothing logged yet."), "{text}");
        assert!(text.contains("an older version"), "{text}");
        let (text, status) = link_log_run(&[], Some("old line\n"));
        assert_eq!(status, 1);
        assert!(text.contains("mesh-kvm isn't running on this device. Its link log file:\nold line\n"), "{text}");
        assert!(text.contains("so the other device can't be asked"), "{text}");
        let (text, _) = link_log_run(&[], None);
        assert!(text.contains("isn't running on this device, and has no link log file yet"), "{text}");
    }

    #[test]
    fn the_link_log_flag_is_parsed() {
        assert!(Cli::try_parse_from(["mesh-kvm", "--link-log"]).unwrap().link_log);
    }

    #[test]
    fn tray_flag_is_parsed() {
        assert!(Cli::try_parse_from(["mesh-kvm", "--tray"]).unwrap().tray);
        assert!(!Cli::try_parse_from(["mesh-kvm"]).unwrap().tray);
    }

    #[test]
    fn login_launcher_stays_silent() {
        assert!(!is_bare_launch(7));
    }
}
