#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]

mod app;
mod audio;
mod clipboard;
mod displays;
mod keymap;
mod input;
mod licenses;
mod mute;
mod platform;
mod protocol;
mod router;
mod secure;
mod settings;
mod spatial;
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

    /// Open the app window for the service on this device, and exit when it closes
    #[arg(long, alias = "studio")]
    app: bool,

    /// Show the tray icon for the service on this device, and exit when Quit is chosen
    #[arg(long)]
    tray: bool,

    /// Show the licences of what's built into this program, then exit
    #[arg(long, alias = "licences")]
    licenses: bool,

    /// Print what's on the clipboard, for the service to read when an app has stopped answering
    #[cfg(target_os = "linux")]
    #[arg(long, hide = true)]
    read_clipboard: bool,

    /// Write the menu and login launchers for this exe (Linux), then exit
    #[cfg(target_os = "linux")]
    #[arg(long)]
    install_launchers: bool,
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
struct BarePlan {
    /// Install this exe over the installed one now (a running service won't do it).
    install_exe: bool,
    /// This process turns into the service.
    become_service: bool,
}

/// A bare launch always opens the tray and window. With no service it becomes one (which installs
/// itself); with a service already running it installs only if it is a different, newer download.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn bare_launch_plan(service_running: bool, exe_differs: bool) -> BarePlan {
    BarePlan { install_exe: service_running && exe_differs, become_service: !service_running }
}

/// The address a running service says it uses.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn peer_in_status(body: &str) -> Option<String> {
    let status: serde_json::Value = serde_json::from_str(body).ok()?;
    settings::validate_peer(status.get("peer_ip")?.as_str()?).ok()
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
fn run_app_window() {
    match app::single::claim(app::single::WINDOW_PORT) {
        app::single::Instance::AlreadyRunning => {
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

    if cli.licenses {
        println!("{}", licenses::TEXT);
        // A Windows program with a window has nowhere to print, unless its output is redirected.
        #[cfg(target_os = "windows")]
        show_box(licenses::TEXT, windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONINFORMATION);
        return;
    }

    // Double-clicking the Windows exe passes no arguments. It opens the tray and the window, and
    // becomes the service unless one is already running; the login launcher always passes arguments.
    #[cfg(target_os = "windows")]
    if is_bare_launch(std::env::args_os().len()) {
        let running = webui::http_call("127.0.0.1", "GET", "/api/local", "").ok();
        let service_running = running.is_some();
        let running_peer = running.and_then(|(_, body)| peer_in_status(&body));
        let installed = app::install::installed_exe();
        let exe_differs = match (std::env::current_exe(), &installed) {
            (Ok(current), Some(installed)) => app::install::needs_install(&current, installed),
            _ => false,
        };
        let plan = bare_launch_plan(service_running, exe_differs);
        if plan.install_exe {
            if let (Ok(current), Some(installed)) = (std::env::current_exe(), &installed) {
                // A newer download was double-clicked while an older copy runs: install it now.
                match app::install::replace_exe(&current, installed) {
                    Ok(()) => settings::log_note(&format!(
                        "Installed the newer {} over the running copy; it takes effect once every mesh-kvm.exe is ended and started again.",
                        current.display()
                    )),
                    Err(e) => settings::log_fatal(&e),
                }
            }
            // The old service wrote the old launchers, which start no tray at login.
            settings::load_and_apply();
            let peer = settings::peer_ip().or(running_peer);
            platform::windows::rewrite_launchers(peer.as_deref());
        }
        for flag in ["--tray", "--app"] {
            if let Err(e) = app::launch::spawn_self(flag) {
                settings::log_fatal(&format!("Couldn't start mesh-kvm {flag}: {e}"));
            }
        }
        if !plan.become_service {
            return;
        }
    }

    if cli.pair_show || cli.pair.is_some() {
        std::process::exit(run_pair_command(&cli));
    }

    if cli.app {
        run_app_window();
        return;
    }
    if cli.tray {
        run_tray_icon();
        return;
    }
    #[cfg(target_os = "linux")]
    if cli.install_launchers {
        if let Err(e) = app::launch::install_linux_launchers() {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    run_service(cli);
}

/// Stops and leaves a note where the app can point to.
fn fail(message: &str) -> ! {
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
            secure::control::peer_call("POST", "/api/local/action", PEER_UNPAIRED_BODY).map(|_| ())
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

/// Where a Windows build says things: it has no console of its own. Redirected output is left
/// alone, then the calling terminal is attached, and with neither the lines go in message boxes.
#[cfg(target_os = "windows")]
fn windows_output() -> Box<dyn FnMut(&str)> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
    let out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if !out.is_null() && out != INVALID_HANDLE_VALUE {
        return Box::new(|line| println!("{line}"));
    }
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0 {
        use std::io::Write;
        if let Ok(mut console) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            return Box::new(move |line| {
                let _ = writeln!(console, "{line}");
            });
        }
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
    wait_for_old_service();

    // First of all, so that a second service stops here and touches nothing.
    let listener = match webui::bind() {
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
        env!("CARGO_PKG_VERSION"),
        cli.role,
        if peer_text.is_empty() { "none yet" } else { &peer_text },
    );

    let platform = platform::current(cli.width.zip(cli.height));
    let state = spatial::SharedSpatialState::load(platform.as_ref(), peer_text, pinned);
    let state = std::sync::Arc::new(std::sync::RwLock::new(state));
    let displays = spatial::start_displays(state.clone(), platform.clone());
    let pairing = std::sync::Arc::new(pairing_service(secure, pinned));
    let handler = spatial::studio_handler(state.clone(), platform.clone(), displays, pairing);
    if let Err(e) = webui::start(listener, peer_ip.clone(), handler.clone()) {
        fail(&e);
    }

    let Some((_, peer_addr, _)) = chosen else {
        // Nothing to share with yet, so nothing is captured. The app asks for the address.
        println!("No other device is known yet. Waiting for its address from the app.");
        while !settings::restart_requested() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        restart();
    };

    spatial::start_sync(state.clone(), platform);

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
    fn a_running_service_gives_its_peer() {
        assert_eq!(super::peer_in_status(r#"{"peer_ip":"192.168.8.241"}"#).as_deref(), Some("192.168.8.241"));
        assert_eq!(super::peer_in_status(r#"{"peer_ip":""}"#), None);
        assert_eq!(super::peer_in_status("not json"), None);
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
    fn bare_launch_plan_covers_every_case() {
        use super::{bare_launch_plan, BarePlan};
        assert_eq!(
            bare_launch_plan(false, false),
            BarePlan { install_exe: false, become_service: true }
        );
        assert_eq!(
            bare_launch_plan(false, true),
            BarePlan { install_exe: false, become_service: true },
            "a starting service installs the exe itself"
        );
        assert_eq!(
            bare_launch_plan(true, false),
            BarePlan { install_exe: false, become_service: false }
        );
        assert_eq!(
            bare_launch_plan(true, true),
            BarePlan { install_exe: true, become_service: false }
        );
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
