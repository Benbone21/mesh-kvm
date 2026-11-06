#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]

mod app;
mod audio;
mod clipboard;
mod keymap;
mod input;
mod protocol;
mod router;
mod settings;
#[cfg(target_os = "linux")]
mod spatial;
mod webui;

use clap::Parser;
use protocol::{
    NodeId, SwitchEdge, DEFAULT_AUDIO_PORT, DEFAULT_CLIPBOARD_PORT, DEFAULT_INPUT_PORT,
};
use std::net::SocketAddr;

#[cfg(target_os = "linux")]
const DEFAULT_ROLE: &str = "debian";
#[cfg(target_os = "windows")]
const DEFAULT_ROLE: &str = "windows";

#[cfg(target_os = "linux")]
const DEFAULT_PEER: &str = "192.168.8.210";
#[cfg(target_os = "windows")]
const DEFAULT_PEER: &str = "192.168.8.241";

#[cfg(target_os = "linux")]
const DEFAULT_EDGE: &str = "right";
#[cfg(target_os = "windows")]
const DEFAULT_EDGE: &str = "left";

#[derive(Parser, Debug)]
#[command(
    name = "mesh-kvm",
    version,
    about = "Zero-latency P2P Mesh Input, Clipboard, and Audio daemon bridging Debian 13 and Windows 11"
)]
struct Cli {
    /// Node role: 'debian' or 'windows'
    #[arg(long, default_value = DEFAULT_ROLE)]
    role: NodeId,

    /// Peer IP address (or IP:PORT)
    #[arg(long, default_value = DEFAULT_PEER)]
    peer: String,

    /// Screen edge that switches focus to the peer ('left', 'right', 'top', 'bottom')
    #[arg(long, default_value = DEFAULT_EDGE)]
    switch_edge: SwitchEdge,

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

    /// Open the app window for the service on this device, and exit when it closes
    #[arg(long, alias = "studio")]
    app: bool,

    /// Show the tray icon for the service on this device, and exit when Quit is chosen
    #[arg(long)]
    tray: bool,

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
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("mesh-kvm").as_ptr(), MB_OK | MB_ICONERROR) };
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
    if let Err(e) = app::ui::run_window() {
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

    // Double-clicking the Windows exe passes no arguments. It opens the tray and the window, and
    // becomes the service unless one is already running; the login launcher always passes arguments.
    #[cfg(target_os = "windows")]
    if is_bare_launch(std::env::args_os().len()) {
        let service_running = webui::http_call("127.0.0.1", "GET", "/api/local", "").is_ok();
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
            match parse_peer_socket_addr(&cli.peer, cli.port) {
                Ok((_, peer_addr)) => input::windows::rewrite_launchers(peer_addr, cli.switch_edge),
                Err(e) => settings::log_fatal(&e),
            }
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

    let (peer_ip, peer_addr) = match parse_peer_socket_addr(&cli.peer, cli.port) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    println!(
        "=== mesh-kvm v{} | Role: {} | Peer: {} | Edge: {} ===",
        env!("CARGO_PKG_VERSION"),
        cli.role,
        peer_addr,
        cli.switch_edge
    );

    settings::load_and_apply();

    // Phase 2: Bidirectional Audio Bus (UDP 24851)
    if cli.audio_listen {
        if let Err(e) = audio::start_audio_listener(cli.audio_port) {
            eprintln!("[Audio] Warning: failed to start audio listener: {}", e);
        }
    }
    let audio_target_str = cli
        .audio_send_to
        .clone()
        .unwrap_or_else(|| peer_ip.clone());
    match parse_peer_socket_addr(&audio_target_str, cli.audio_port) {
        Ok((_, audio_addr)) => {
            if let Err(e) = audio::start_audio_sender(audio_addr) {
                eprintln!("[Audio] Warning: failed to start audio sender: {}", e);
            }
        }
        Err(e) => eprintln!("[Audio] Invalid audio target address: {}", e),
    }

    // Phase 3: Unified Clipboard Sync (TCP 24852)
    if cli.clipboard {
        if let Err(e) = clipboard::start_clipboard_sync(peer_ip, cli.clipboard_port) {
            eprintln!("[Clipboard] Warning: failed to start clipboard sync: {}", e);
        }
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
            peer_addr,
            cli.port,
            cli.switch_edge,
            cli.width,
            cli.height,
            cli.mouse_device,
        ) {
            eprintln!("[Debian] Fatal error: {}", e);
            std::process::exit(1);
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
        if let Err(e) = input::windows::run_windows_node(
            peer_addr,
            cli.port,
            cli.switch_edge,
            cli.width,
            cli.height,
        ) {
            eprintln!("[Windows] Fatal error: {}", e);
            settings::log_fatal(&e);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_bare_launch, Cli};
    use clap::Parser;

    #[test]
    fn a_window_that_cannot_start_says_where_the_log_is() {
        let text = super::window_failed_text(std::path::Path::new(r"C:\Users\me\mesh-kvm\mesh-kvm.log"));
        assert!(text.contains("window couldn't start"), "{text}");
        assert!(text.contains(r"C:\Users\me\mesh-kvm\mesh-kvm.log"), "{text}");
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
    fn tray_flag_is_parsed() {
        assert!(Cli::try_parse_from(["mesh-kvm", "--tray"]).unwrap().tray);
        assert!(!Cli::try_parse_from(["mesh-kvm"]).unwrap().tray);
    }

    #[test]
    fn login_launcher_stays_silent() {
        assert!(!is_bare_launch(7));
    }
}
