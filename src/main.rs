mod audio;
mod clipboard;
mod keymap;
#[cfg(target_os = "linux")]
mod linux;
mod protocol;
#[cfg(target_os = "linux")]
mod spatial;
#[cfg(target_os = "windows")]
mod windows;

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

fn main() {
    let cli = Cli::parse();

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
        if let Err(e) = linux::run_debian_node(
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
        if let Err(e) = windows::run_windows_node(
            peer_addr,
            cli.port,
            cli.switch_edge,
            cli.width,
            cli.height,
        ) {
            eprintln!("[Windows] Fatal error: {}", e);
            std::process::exit(1);
        }
    }
}
