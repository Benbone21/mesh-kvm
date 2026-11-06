//! Per-device persisted settings (audio routing, toggles) + local actions shared by both nodes.

use crate::audio::devices::{self, Choice};
use crate::audio::{
    other_version_arriving, peer_listening, reaching_peer, AUDIO_BITRATE_KBPS, AUDIO_PLAY_ONLY_ON_OTHER, AUDIO_RX_BUFFER_MS, AUDIO_RX_LIVE,
    AUDIO_RX_LOSS, AUDIO_RX_MUTED, AUDIO_RX_PACKETS, AUDIO_RX_PEAK_PCT, AUDIO_RX_UNREPAIRED, AUDIO_RX_VOLUME_PCT, AUDIO_TX_LOSS,
    AUDIO_TX_PACKETS, AUDIO_TX_PEAK_PCT, AUDIO_TX_VOLUME_PCT, BITRATES, DEFAULT_BITRATE,
};
use crate::clipboard::{clipboard_note, CLIPBOARD_RECEIVE, CLIPBOARD_SEND};
use crate::input::link::LinkQuality;
use crate::protocol::{NodeId, SwitchEdge};
use crate::router::{DeviceId, Event, Sharing};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, PoisonError};

const MIN_POINTER_SPEED: f64 = 0.05;
const MAX_POINTER_SPEED: f64 = 2.0;
const DEFAULT_POINTER_SPEED: f32 = 1.0;
const DEFAULT_MAIN_DEVICE: DeviceId = NodeId::Debian as DeviceId;
const RESTART_DELAY: std::time::Duration = std::time::Duration::from_millis(300);
pub const OK_RESPONSE: &str = r#"{"ok":true}"#;
/// The longest device name. The layout holds names to the same length.
pub const MAX_DEVICE_NAME: usize = 32;

static MOUSE_SHARING: AtomicBool = AtomicBool::new(true);
static KEYBOARD_SHARING: AtomicBool = AtomicBool::new(true);
/// This device's pointer speed, as `f32` bits.
static POINTER_SPEED_BITS: AtomicU32 = AtomicU32::new(DEFAULT_POINTER_SPEED.to_bits());
static MAIN_DEVICE: AtomicU8 = AtomicU8::new(DEFAULT_MAIN_DEVICE);
/// Set after the values above change, for the input loop to feed its router.
static SHARING_PENDING: AtomicBool = AtomicBool::new(false);
static MAIN_PENDING: AtomicBool = AtomicBool::new(false);
/// The other device's address, or empty when none is known yet.
static PEER_IP: Mutex<String> = Mutex::new(String::new());
/// What this device is called, or empty to go by its hostname.
static DEVICE_NAME: Mutex<String> = Mutex::new(String::new());
static SWITCH_EDGE: Mutex<Option<SwitchEdge>> = Mutex::new(None);
/// Set when the service should start again, for the loop that owns its shutdown.
static RESTART_PENDING: AtomicBool = AtomicBool::new(false);
/// One save at a time: they share a temp file, and the last to finish must have the newest values.
static SAVING: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalSettings {
    /// From before the choice of devices: only read, to work out the choices below.
    #[serde(skip_serializing)]
    pub audio_tx_enabled: bool,
    #[serde(skip_serializing)]
    pub audio_tx_source: u32,
    /// What to send, as a device id, `default`, or empty for nothing. Missing from older settings.
    pub audio_input: Option<String>,
    pub audio_output: Option<String>,
    /// Where received audio plays: a device id or `default`.
    pub audio_play: String,
    /// In kbit/s, with zero meaning uncompressed.
    pub audio_bitrate: u32,
    pub audio_tx_volume: u32,
    pub audio_rx_muted: bool,
    pub audio_rx_volume: u32,
    #[serde(alias = "mute_while_sending")]
    pub play_only_on_other: bool,
    pub mouse_sharing: bool,
    pub keyboard_sharing: bool,
    pub clipboard_send: bool,
    pub clipboard_receive: bool,
    pub pointer_speed: f32,
    pub main_device: u8,
    /// The other device's address. Empty until it is known.
    pub peer_ip: String,
    /// What this device is called. Empty means its hostname.
    pub device_name: String,
    /// The side of this device the other one sits on, for a layout that doesn't exist yet.
    pub switch_edge: Option<SwitchEdge>,
}

impl Default for LocalSettings {
    fn default() -> Self {
        Self {
            audio_tx_enabled: false,
            audio_tx_source: 0,
            audio_input: None,
            audio_output: None,
            audio_play: devices::DEFAULT.to_string(),
            audio_bitrate: DEFAULT_BITRATE,
            audio_tx_volume: 100,
            audio_rx_muted: false,
            audio_rx_volume: 100,
            play_only_on_other: true,
            mouse_sharing: true,
            keyboard_sharing: true,
            clipboard_send: true,
            clipboard_receive: true,
            pointer_speed: DEFAULT_POINTER_SPEED,
            main_device: DEFAULT_MAIN_DEVICE,
            peer_ip: String::new(),
            device_name: String::new(),
            switch_edge: None,
        }
    }
}

pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("APPDATA"))
            .unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("mesh-kvm")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("mesh-kvm")
    }
}

/// Present when the person turned starting at boot off, so that nothing at login undoes that.
pub fn autostart_disabled_marker() -> PathBuf {
    config_dir().join("autostart-disabled")
}

/// Where `log_fatal` and `log_note` write.
pub fn log_path() -> PathBuf {
    config_dir().join("mesh-kvm.log")
}

/// Writes a fatal error to `mesh-kvm.log`. The Windows build has no console, and a window started
/// from a menu has nowhere to print, so the error would otherwise vanish.
pub fn log_fatal(message: &str) {
    append_fatal(&config_dir(), message);
}

/// Writes a plain note to `mesh-kvm.log`, for things worth knowing that aren't fatal.
pub fn log_note(message: &str) {
    append_line(&config_dir(), &format!("Note: {}", message));
}

fn append_fatal(dir: &Path, message: &str) {
    append_line(dir, &format!("Fatal error: {}", message));
}

fn append_line(dir: &Path, line: &str) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(mut file) =
        std::fs::OpenOptions::new().create(true).append(true).open(dir.join("mesh-kvm.log"))
    {
        let _ = writeln!(file, "{}", line);
    }
}

fn settings_path() -> PathBuf {
    config_dir().join("local-settings.json")
}

/// Reads a settings file's text. Anything unusable gives the defaults, and a value out of range
/// is repaired, so no file can start the router on anything but sane values.
fn parse_settings(text: &str) -> LocalSettings {
    let mut s: LocalSettings = serde_json::from_str(text).unwrap_or_default();
    s.pointer_speed = clamp_pointer_speed(s.pointer_speed as f64).unwrap_or(DEFAULT_POINTER_SPEED);
    if NodeId::try_from(s.main_device).is_err() {
        s.main_device = DEFAULT_MAIN_DEVICE;
    }
    s.peer_ip = validate_peer(&s.peer_ip).unwrap_or_default();
    s
}

/// Checks an address for the other device, with an optional port, and returns its plain form.
/// Only IPv4 works: the API and the link build `ip:port` strings.
pub fn validate_peer(text: &str) -> Result<String, String> {
    let text = text.trim();
    let ip = text
        .parse::<Ipv4Addr>()
        .or_else(|_| text.parse::<SocketAddrV4>().map(|a| *a.ip()))
        .map_err(|_| "Enter an address like 192.168.1.20.".to_string())?;
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() || ip.is_broadcast() {
        return Err("That address can't belong to another device. Enter one like 192.168.1.20.".to_string());
    }
    Ok(ip.to_string())
}

fn clamp_pointer_speed(speed: f64) -> Option<f32> {
    (!speed.is_nan()).then(|| speed.clamp(MIN_POINTER_SPEED, MAX_POINTER_SPEED) as f32)
}

/// The clamped speed in a `set_pointer_speed` request, if it has one.
pub fn requested_pointer_speed(val: &serde_json::Value) -> Option<f32> {
    val.get("speed").and_then(|v| v.as_f64()).and_then(clamp_pointer_speed)
}

/// The device in a `set_main_device` request, if it is a known one.
fn requested_main_device(val: &serde_json::Value) -> Option<DeviceId> {
    let device = u8::try_from(val.get("device")?.as_u64()?).ok()?;
    NodeId::try_from(device).is_ok().then_some(device)
}

pub fn sharing() -> Sharing {
    Sharing {
        mouse: MOUSE_SHARING.load(Ordering::SeqCst),
        keyboard: KEYBOARD_SHARING.load(Ordering::SeqCst),
    }
}

/// The saved main device, for the router's start. Later changes arrive as `Event::SetMain`.
pub fn main_device() -> DeviceId {
    MAIN_DEVICE.load(Ordering::SeqCst)
}

pub fn pointer_speed() -> f32 {
    f32::from_bits(POINTER_SPEED_BITS.load(Ordering::SeqCst))
}

/// Takes on the speed an old layout file carried, which was this device's real speed until now.
pub fn adopt_pointer_speed(speed: f32) {
    let speed = clamp_pointer_speed(speed as f64).unwrap_or(DEFAULT_POINTER_SPEED);
    POINTER_SPEED_BITS.store(speed.to_bits(), Ordering::SeqCst);
    save_current();
}

fn peer_slot() -> std::sync::MutexGuard<'static, String> {
    PEER_IP.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The saved address of the other device, if there is one.
pub fn peer_ip() -> Option<String> {
    Some(peer_slot().clone()).filter(|ip| !ip.is_empty())
}

/// What this device is called: the name it was given, or else its hostname.
pub fn device_name() -> String {
    let name = DEVICE_NAME.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if name.is_empty() {
        crate::webui::hostname()
    } else {
        name
    }
}

/// Handles `set_device_name` and returns the response body. A blank name means the hostname.
pub fn set_device_name(val: &serde_json::Value) -> String {
    change_device_name(val, save_current)
}

fn change_device_name(val: &serde_json::Value, save: impl FnOnce()) -> String {
    let refuse = |error: &str| serde_json::json!({ "ok": false, "error": error }).to_string();
    let Some(name) = val.get("name").and_then(|v| v.as_str()).map(str::trim) else {
        return refuse("That isn't a name.");
    };
    if name.chars().count() > MAX_DEVICE_NAME {
        return refuse("That name is too long. Use 32 characters or fewer.");
    }
    if name.chars().any(char::is_control) {
        return refuse("A name can't have line breaks or tabs in it.");
    }
    *DEVICE_NAME.lock().unwrap_or_else(PoisonError::into_inner) = name.to_string();
    save();
    OK_RESPONSE.to_string()
}

/// The side of this device the other one sits on, if one was ever given.
pub fn switch_edge() -> Option<SwitchEdge> {
    *SWITCH_EDGE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Keeps what the service was started with, so that it carries on without those flags. A
/// `--peer` that isn't another device's address, as in a test on one machine, is used but not kept.
pub fn remember_start_flags(peer: Option<&str>, edge: Option<SwitchEdge>) {
    let peer = peer.and_then(|text| validate_peer(text).ok());
    let new_peer = peer.as_ref().is_some_and(|ip| *peer_slot() != *ip);
    let new_edge = edge.is_some() && switch_edge() != edge;
    if let (true, Some(ip)) = (new_peer, peer) {
        *peer_slot() = ip;
    }
    if new_edge {
        *SWITCH_EDGE.lock().unwrap_or_else(PoisonError::into_inner) = edge;
    }
    if new_peer || new_edge {
        save_current();
    }
}

/// True once the service should start again. Its main loop then shuts down and calls
/// `restart_service`.
pub fn restart_requested() -> bool {
    RESTART_PENDING.load(Ordering::SeqCst)
}

/// Asks for a restart, after a moment so the reply to the request still gets out.
fn request_restart() {
    let spawned = std::thread::Builder::new().name("mesh-kvm-restart".to_string()).spawn(|| {
        std::thread::sleep(RESTART_DELAY);
        RESTART_PENDING.store(true, Ordering::SeqCst);
    });
    if spawned.is_err() {
        RESTART_PENDING.store(true, Ordering::SeqCst);
    }
}

/// Who is asking `set_peer`, and what the service can say about the address.
pub struct PeerChange<'a> {
    /// The app on this device. Anyone else on the network could otherwise pair this device with
    /// themselves, which would hand them its keyboard and clipboard.
    pub from_this_device: bool,
    /// `--peer` was given at start. It would win again at the next start.
    pub pinned: bool,
    pub is_own_address: &'a dyn Fn(&str) -> bool,
}

/// Handles `set_peer` and returns the response body. The service restarts to use the new address.
pub fn set_peer(val: &serde_json::Value, who: PeerChange) -> String {
    change_peer(val, who, save_current, request_restart)
}

fn change_peer(val: &serde_json::Value, who: PeerChange, save: impl FnOnce(), restart: impl FnOnce()) -> String {
    let refuse = |error: &str| serde_json::json!({ "ok": false, "error": error }).to_string();
    if !who.from_this_device {
        return refuse("The other device's address can only be changed from the app on this device.");
    }
    if who.pinned {
        return refuse("mesh-kvm was started with --peer, which sets this address. Remove it from how mesh-kvm starts, then try again.");
    }
    let text = val.get("peer").and_then(|v| v.as_str()).unwrap_or("");
    let ip = match validate_peer(text) {
        Ok(ip) => ip,
        Err(e) => return refuse(&e),
    };
    if (who.is_own_address)(&ip) {
        return refuse("That's this device's own address. Enter the other device's address.");
    }
    if *peer_slot() != ip {
        *peer_slot() = ip;
        save();
        restart();
    }
    OK_RESPONSE.to_string()
}

/// Toggle changes the router has yet to hear about. The input loops call this every tick and
/// feed the events in, so that releases and blocking happen on the thread that owns them.
pub fn take_router_events() -> Vec<Event> {
    let mut events = Vec::new();
    if SHARING_PENDING.swap(false, Ordering::SeqCst) {
        events.push(Event::SetSharing(sharing()));
    }
    if MAIN_PENDING.swap(false, Ordering::SeqCst) {
        events.push(Event::SetMain(main_device()));
    }
    events
}

/// Loads persisted settings and applies them to the live atomics. Runs before the router is
/// built, which reads them directly, so nothing is left pending.
pub fn load_and_apply() {
    let s = std::fs::read_to_string(settings_path())
        .map(|text| parse_settings(&text))
        .unwrap_or_default();
    devices::set_choice(audio_choice(&s, cfg!(target_os = "linux")));
    AUDIO_BITRATE_KBPS.store(usable_bitrate(s.audio_bitrate), Ordering::SeqCst);
    AUDIO_TX_VOLUME_PCT.store(s.audio_tx_volume.min(150), Ordering::SeqCst);
    AUDIO_RX_MUTED.store(s.audio_rx_muted, Ordering::SeqCst);
    AUDIO_RX_VOLUME_PCT.store(s.audio_rx_volume.min(150), Ordering::SeqCst);
    AUDIO_PLAY_ONLY_ON_OTHER.store(s.play_only_on_other, Ordering::SeqCst);
    MOUSE_SHARING.store(s.mouse_sharing, Ordering::SeqCst);
    KEYBOARD_SHARING.store(s.keyboard_sharing, Ordering::SeqCst);
    CLIPBOARD_SEND.store(s.clipboard_send, Ordering::SeqCst);
    CLIPBOARD_RECEIVE.store(s.clipboard_receive, Ordering::SeqCst);
    POINTER_SPEED_BITS.store(s.pointer_speed.to_bits(), Ordering::SeqCst);
    MAIN_DEVICE.store(s.main_device, Ordering::SeqCst);
    *peer_slot() = s.peer_ip;
    *DEVICE_NAME.lock().unwrap_or_else(PoisonError::into_inner) = s.device_name;
    *SWITCH_EDGE.lock().unwrap_or_else(PoisonError::into_inner) = s.switch_edge;
}

/// What to send and play. Older settings had one Send switch and, on Linux, a choice of source.
/// Windows only ever sent its system sound, whatever a settings file held.
fn audio_choice(s: &LocalSettings, mic_selectable: bool) -> Choice {
    let from_old_switch = |wants_mic: bool| {
        let selected = s.audio_tx_enabled && (s.audio_tx_source == 1 && mic_selectable) == wants_mic;
        if selected { devices::DEFAULT } else { devices::NONE }.to_string()
    };
    Choice {
        input: s.audio_input.clone().unwrap_or_else(|| from_old_switch(true)),
        output: s.audio_output.clone().unwrap_or_else(|| from_old_switch(false)),
        play: if s.audio_play.is_empty() { devices::DEFAULT.to_string() } else { s.audio_play.clone() },
    }
}

fn usable_bitrate(kbps: u32) -> u32 {
    if BITRATES.contains(&kbps) {
        kbps
    } else {
        DEFAULT_BITRATE
    }
}

fn current_settings() -> LocalSettings {
    let choice = devices::choice();
    LocalSettings {
        audio_input: Some(choice.input),
        audio_output: Some(choice.output),
        audio_play: choice.play,
        audio_bitrate: AUDIO_BITRATE_KBPS.load(Ordering::Relaxed),
        audio_tx_volume: AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed),
        audio_rx_muted: AUDIO_RX_MUTED.load(Ordering::Relaxed),
        audio_rx_volume: AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed),
        play_only_on_other: AUDIO_PLAY_ONLY_ON_OTHER.load(Ordering::SeqCst),
        audio_tx_enabled: false,
        audio_tx_source: 0,
        mouse_sharing: MOUSE_SHARING.load(Ordering::SeqCst),
        keyboard_sharing: KEYBOARD_SHARING.load(Ordering::SeqCst),
        clipboard_send: CLIPBOARD_SEND.load(Ordering::SeqCst),
        clipboard_receive: CLIPBOARD_RECEIVE.load(Ordering::SeqCst),
        pointer_speed: f32::from_bits(POINTER_SPEED_BITS.load(Ordering::SeqCst)),
        main_device: main_device(),
        peer_ip: peer_slot().clone(),
        device_name: DEVICE_NAME.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        switch_edge: switch_edge(),
    }
}

fn temp_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

/// Replaces `path` in one step, so that a crash leaves the old file or the new one, never half.
pub fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let temp = temp_path(path);
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

pub fn save_current() {
    let _saving = SAVING.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = std::fs::create_dir_all(config_dir());
    if let Ok(json) = serde_json::to_string_pretty(&current_settings()) {
        let _ = write_atomically(&settings_path(), &json);
    }
}

fn audio_notes(mut notes: Vec<String>) -> Vec<String> {
    if crate::mute::barred() {
        notes.push("Sound stopped as soon as this output was muted, so it's staying on for now.".to_string());
    }
    notes
}

pub fn audio_status_json() -> serde_json::Value {
    let (choice, lists, in_use) = (devices::choice(), devices::lists(), devices::in_use());
    let now = std::time::Instant::now();
    serde_json::json!({
        "tx_enabled": devices::sending(),
        "input": choice.input,
        "output": choice.output,
        "play": choice.play,
        "inputs": lists.inputs,
        "outputs": lists.outputs,
        "notes": audio_notes(in_use.notes),
        "echo_safe": in_use.echo_safe,
        "bitrate": AUDIO_BITRATE_KBPS.load(Ordering::Relaxed),
        "tx_volume": AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed),
        "tx_peak": AUDIO_TX_PEAK_PCT.load(Ordering::Relaxed),
        "tx_packets": AUDIO_TX_PACKETS.load(Ordering::Relaxed),
        "tx_loss": AUDIO_TX_LOSS.load(Ordering::Relaxed),
        "tx_reaching": reaching_peer(now, std::time::Duration::from_secs(2)),
        "tx_connected": peer_listening(now, std::time::Duration::from_secs(2)),
        "play_only_on_other": AUDIO_PLAY_ONLY_ON_OTHER.load(Ordering::SeqCst),
        "rx_muted": AUDIO_RX_MUTED.load(Ordering::Relaxed),
        "rx_volume": AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed),
        "rx_peak": AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed),
        "rx_packets": AUDIO_RX_PACKETS.load(Ordering::Relaxed),
        "rx_loss": f64::from(AUDIO_RX_LOSS.load(Ordering::Relaxed)) / 10.0,
        "rx_unrepaired": f64::from(AUDIO_RX_UNREPAIRED.load(Ordering::Relaxed)) / 10.0,
        "rx_buffer_ms": AUDIO_RX_BUFFER_MS.load(Ordering::Relaxed),
        "rx_live": AUDIO_RX_LIVE.load(Ordering::Relaxed),
        "rx_other_version": other_version_arriving(now),
    })
}

/// The link's own figures, with what audio has to say about the same network. Audio sends far more
/// packets than heartbeats do, and it counts what arrived too late to play as well as what never
/// arrived at all. Each direction only counts while it's actually in use.
pub fn link_status_json(quality: &LinkQuality) -> serde_json::Value {
    let now = std::time::Instant::now();
    let audio_in = AUDIO_RX_LIVE.load(Ordering::Relaxed).then(|| f64::from(AUDIO_RX_LOSS.load(Ordering::Relaxed)) / 10.0);
    let audio_out = peer_listening(now, std::time::Duration::from_secs(2)).then(|| f64::from(AUDIO_TX_LOSS.load(Ordering::Relaxed)));
    let mut status = serde_json::json!(quality);
    status["audio_loss_in"] = serde_json::json!(audio_in.filter(|_| quality.measured));
    status["audio_loss_out"] = serde_json::json!(audio_out.filter(|_| quality.measured));
    status
}

/// Adds the toggles to a `/api/local` status, along with whether the router has a main conflict.
pub fn add_toggle_status(status: &mut serde_json::Value, main_conflict: bool) {
    let sharing = sharing();
    status["sharing"] = serde_json::json!({ "mouse": sharing.mouse, "keyboard": sharing.keyboard });
    status["clipboard"] = serde_json::json!({
        "send": CLIPBOARD_SEND.load(Ordering::SeqCst),
        "receive": CLIPBOARD_RECEIVE.load(Ordering::SeqCst),
        "note": clipboard_note(),
    });
    status["pointer_speed"] = serde_json::json!(pointer_speed());
    status["main_device"] = serde_json::json!(main_device());
    status["main_conflict"] = serde_json::json!(main_conflict);
}

/// Applies a toggle action to the live atomics, without saving. A field left out keeps its value,
/// and an unusable value changes nothing. Returns true if the action is a toggle.
fn apply_toggle(act: &str, val: &serde_json::Value) -> bool {
    let flag = |k: &str| val.get(k).and_then(|v| v.as_bool());
    let store = |target: &AtomicBool, k: &str| {
        if let Some(on) = flag(k) {
            target.store(on, Ordering::SeqCst);
        }
    };
    match act {
        "set_sharing" => {
            store(&MOUSE_SHARING, "mouse");
            store(&KEYBOARD_SHARING, "keyboard");
            SHARING_PENDING.store(true, Ordering::SeqCst);
        }
        "set_clipboard" => {
            store(&CLIPBOARD_SEND, "send");
            store(&CLIPBOARD_RECEIVE, "receive");
        }
        "set_play_only_on_other" => store(&AUDIO_PLAY_ONLY_ON_OTHER, "enabled"),
        "set_pointer_speed" => {
            if let Some(speed) = requested_pointer_speed(val) {
                POINTER_SPEED_BITS.store(speed.to_bits(), Ordering::SeqCst);
            }
        }
        _ => return false,
    }
    true
}

/// Handles `set_main_device` and returns the response body. Not a common action, as each node
/// must say whether its link is up: changing one device alone leaves two mains or none.
pub fn set_main_device(val: &serde_json::Value, link_up: bool) -> String {
    change_main_device(val, link_up, save_current)
}

fn change_main_device(val: &serde_json::Value, link_up: bool, save: impl FnOnce()) -> String {
    let refuse = |error: &str| serde_json::json!({ "ok": false, "error": error }).to_string();
    let Some(device) = requested_main_device(val) else {
        return refuse("Unknown device. Choose one of the two devices.");
    };
    if !link_up {
        return refuse("The devices aren't linked. Reconnect them, then change the main device.");
    }
    MAIN_DEVICE.store(device, Ordering::SeqCst);
    MAIN_PENDING.store(true, Ordering::SeqCst);
    save();
    OK_RESPONSE.to_string()
}

/// Longer than any id PipeWire or Windows gives a device.
const MAX_AUDIO_DEVICE_ID: usize = 512;

/// Handles `set_audio_device` and returns the response body. Not a common action, as it has to
/// know who is asking: the other device must not be able to switch on this one's microphone.
pub fn set_audio_device(val: &serde_json::Value, from_this_device: bool) -> String {
    change_audio_device(val, from_this_device, save_current)
}

fn change_audio_device(val: &serde_json::Value, from_this_device: bool, save: impl FnOnce()) -> String {
    let refuse = |error: &str| serde_json::json!({ "ok": false, "error": error }).to_string();
    let Some(id) = val.get("id").and_then(|v| v.as_str()) else {
        return refuse("That isn't a device.");
    };
    if id.len() > MAX_AUDIO_DEVICE_ID || id.chars().any(char::is_control) {
        return refuse("That isn't a device.");
    }
    let mut choice = devices::choice();
    match val.get("role").and_then(|v| v.as_str()) {
        Some("input") if id != devices::NONE && !from_this_device => {
            return refuse("A microphone can only be chosen on the device it belongs to.");
        }
        Some("input") => choice.input = id.to_string(),
        Some("output") => choice.output = id.to_string(),
        Some("play") if id == devices::NONE => return refuse("Received audio has to play somewhere."),
        Some("play") => choice.play = id.to_string(),
        _ => return refuse("That isn't one of the audio choices."),
    }
    devices::set_choice(choice);
    crate::audio::choices_changed();
    save();
    OK_RESPONSE.to_string()
}

fn change_bitrate(kbps: Option<u64>) {
    if let Some(kbps) = kbps.filter(|kbps| BITRATES.contains(&(*kbps as u32))) {
        AUDIO_BITRATE_KBPS.store(kbps as u32, Ordering::SeqCst);
    }
}

/// Handles actions common to both nodes. Returns true if the action was recognised.
pub fn handle_common_action(act: &str, val: &serde_json::Value) -> bool {
    if apply_toggle(act, val) {
        save_current();
        return true;
    }
    let num = |k: &str| val.get(k).and_then(|v| v.as_u64());
    let flag = |k: &str| val.get(k).and_then(|v| v.as_bool());
    match act {
        "set_audio_bitrate" => {
            change_bitrate(num("kbps"));
        }
        "set_audio_tx_volume" => {
            if let Some(v) = num("volume") {
                AUDIO_TX_VOLUME_PCT.store(v.min(150) as u32, Ordering::SeqCst);
            }
        }
        "set_audio_rx_muted" => {
            let m = flag("muted").unwrap_or(!AUDIO_RX_MUTED.load(Ordering::Relaxed));
            AUDIO_RX_MUTED.store(m, Ordering::SeqCst);
        }
        "set_audio_rx_volume" => {
            if let Some(v) = num("volume") {
                AUDIO_RX_VOLUME_PCT.store(v.min(150) as u32, Ordering::SeqCst);
            }
        }
        "test_audio_chime" => {
            // target: "peer" sends a chime across the audio bus to the other device,
            //         "local" plays it on this device's speakers (Linux only).
            let to_peer = val.get("target").and_then(|v| v.as_str()) != Some("local");
            crate::audio::trigger_test_chime(!to_peer, to_peer);
            return true;
        }
        _ => return false,
    }
    save_current();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{Router, RouterConfig};

    #[test]
    fn a_device_name_is_trimmed_and_a_bad_one_changes_nothing() {
        let _guard = lock_names();
        let set = |name: serde_json::Value| change_device_name(&serde_json::json!({ "name": name }), || {});
        let saved = || DEVICE_NAME.lock().unwrap().clone();
        assert_eq!(set("  Desk PC ".into()), OK_RESPONSE);
        assert_eq!(saved(), "Desk PC");
        assert!(set("x".repeat(33).into()).contains("too long"));
        assert!(set("two\nlines".into()).contains("line breaks"));
        assert!(set(5.into()).contains("isn't a name"));
        assert_eq!(saved(), "Desk PC");
        assert_eq!(set("".into()), OK_RESPONSE);
        assert_eq!(device_name(), crate::webui::hostname(), "blank goes back to the hostname");
    }

    fn lock_names() -> std::sync::MutexGuard<'static, ()> {
        static NAMES: Mutex<()> = Mutex::new(());
        NAMES.lock().unwrap_or_else(PoisonError::into_inner)
    }
    use serde_json::json;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// The tests below share the live toggle statics. None of them saves: that is the real file.
    static LIVE_TOGGLES: Mutex<()> = Mutex::new(());

    fn live_toggles() -> MutexGuard<'static, ()> {
        LIVE_TOGGLES.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn assert_toggle_defaults(s: &LocalSettings) {
        assert!(s.mouse_sharing);
        assert!(s.keyboard_sharing);
        assert!(s.clipboard_send);
        assert!(s.clipboard_receive);
        assert_eq!(s.pointer_speed, 1.0);
        assert_eq!(s.main_device, 0);
    }

    #[test]
    fn fatal_errors_add_to_the_log() {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-fatal-{}", std::process::id()));
        append_fatal(&dir, "no OpenGL");
        append_fatal(&dir, "still no OpenGL");
        let log = std::fs::read_to_string(dir.join("mesh-kvm.log")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(log, "Fatal error: no OpenGL\nFatal error: still no OpenGL\n");
    }

    #[test]
    fn old_settings_file_gets_defaults() {
        let s = parse_settings(
            r#"{"audio_tx_enabled":true,"audio_tx_volume":80,"audio_tx_source":1,
                "audio_rx_muted":true,"audio_rx_volume":90}"#,
        );
        assert_toggle_defaults(&s);
        assert!(s.audio_tx_enabled);
        assert_eq!(s.audio_tx_volume, 80);
    }

    #[test]
    fn corrupt_settings_file_gives_defaults() {
        for text in ["{nope", "", "null", "42", r#"{"mouse_sharing":"yes"}"#, r#"{"main_device":-1}"#] {
            assert_toggle_defaults(&parse_settings(text));
        }
    }

    #[test]
    fn saved_values_out_of_range_are_repaired() {
        let s = parse_settings(r#"{"pointer_speed":9.0,"main_device":7,"mouse_sharing":false}"#);
        assert_eq!(s.pointer_speed, 2.0);
        assert_eq!(s.main_device, 0);
        assert!(!s.mouse_sharing);
        assert_eq!(parse_settings(r#"{"pointer_speed":-3}"#).pointer_speed, 0.05);
    }

    #[test]
    fn saved_toggles_survive_a_round_trip() {
        let s = LocalSettings {
            mouse_sharing: false,
            keyboard_sharing: false,
            clipboard_send: false,
            clipboard_receive: false,
            pointer_speed: 0.5,
            main_device: 1,
            ..LocalSettings::default()
        };
        let back = parse_settings(&serde_json::to_string_pretty(&s).unwrap());
        assert!(!back.mouse_sharing);
        assert!(!back.keyboard_sharing);
        assert!(!back.clipboard_send);
        assert!(!back.clipboard_receive);
        assert_eq!(back.pointer_speed, 0.5);
        assert_eq!(back.main_device, 1);
    }

    #[test]
    fn bad_settings_start_with_local_input() {
        for me in [0, 1] {
            let s = parse_settings("{nope");
            let mut router = Router::new(RouterConfig {
                me,
                peer: 1 - me,
                main: s.main_device,
                sharing: Sharing { mouse: s.mouse_sharing, keyboard: s.keyboard_sharing },
                pointer_speed: s.pointer_speed,
            });
            router.handle(Event::PeerAlive { now_ms: 1000 });
            assert_eq!(router.active(), me);
            assert!(!router.handle(Event::LocalKey { scancode: 30, pressed: true }).swallow);
            assert!(!router.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);
        }
    }

    #[test]
    fn a_peer_address_is_checked_and_put_in_plain_form() {
        assert_eq!(validate_peer("192.168.8.210").unwrap(), "192.168.8.210");
        assert_eq!(validate_peer("  10.0.0.5\n").unwrap(), "10.0.0.5");
        assert_eq!(validate_peer("192.168.8.210:24850").unwrap(), "192.168.8.210");
        for bad in [
            "", "   ", "laptop", "192.168.8", "192.168.8.300", "192.168.8.210:99999", "http://192.168.8.210",
            "fe80::1", "192.168.8.210; rm -rf /", "0.0.0.0", "127.0.0.1", "127.0.0.2", "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(validate_peer(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_saved_peer_that_is_no_good_is_dropped_and_the_rest_is_kept() {
        let s = parse_settings(r#"{"peer_ip":"not an address","device_name":"Desk","mouse_sharing":false}"#);
        assert_eq!(s.peer_ip, "");
        assert_eq!(s.device_name, "Desk");
        assert!(!s.mouse_sharing);
        assert_eq!(parse_settings(r#"{"peer_ip":"192.168.8.210"}"#).peer_ip, "192.168.8.210");
    }

    #[test]
    fn peer_name_and_edge_survive_a_round_trip_and_old_files_have_none() {
        let s = LocalSettings {
            peer_ip: "192.168.8.210".into(),
            device_name: "Desk".into(),
            switch_edge: Some(SwitchEdge::Bottom),
            ..LocalSettings::default()
        };
        let back = parse_settings(&serde_json::to_string_pretty(&s).unwrap());
        assert_eq!(back.peer_ip, "192.168.8.210");
        assert_eq!(back.device_name, "Desk");
        assert_eq!(back.switch_edge, Some(SwitchEdge::Bottom));
        let old = parse_settings(r#"{"audio_tx_enabled":true}"#);
        assert_eq!((old.peer_ip.as_str(), old.device_name.as_str(), old.switch_edge), ("", "", None));
    }

    fn try_peer(val: serde_json::Value, from_this_device: bool, pinned: bool) -> (String, u32, u32) {
        let (saves, restarts) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
        let who = PeerChange { from_this_device, pinned, is_own_address: &|ip| ip == "192.168.8.241" };
        let reply = change_peer(&val, who, || saves.set(saves.get() + 1), || restarts.set(restarts.get() + 1));
        (reply, saves.get(), restarts.get())
    }

    #[test]
    fn a_good_peer_is_saved_once_and_restarts_the_service_once() {
        let _guard = live_toggles();
        let (before, peer) = (peer_slot().clone(), json!({ "peer": "192.168.8.210" }));
        peer_slot().clear();
        assert_eq!(try_peer(peer.clone(), true, false), (OK_RESPONSE.to_string(), 1, 1));
        assert_eq!(peer_ip().as_deref(), Some("192.168.8.210"));
        // Asking again for the same address changes nothing.
        assert_eq!(try_peer(peer, true, false), (OK_RESPONSE.to_string(), 0, 0));
        *peer_slot() = before;
    }

    #[test]
    fn a_peer_is_refused_with_a_reason_and_nothing_changes() {
        let _guard = live_toggles();
        let before = peer_slot().clone();
        for (val, here, pinned, says) in [
            (json!({ "peer": "192.168.8.210" }), false, false, "from the app on this device"),
            (json!({ "peer": "192.168.8.210" }), true, true, "--peer"),
            (json!({ "peer": "laptop" }), true, false, "like 192.168.1.20"),
            (json!({}), true, false, "like 192.168.1.20"),
            (json!({ "peer": "127.0.0.1" }), true, false, "another device"),
            (json!({ "peer": "192.168.8.241" }), true, false, "own address"),
        ] {
            let (reply, saves, restarts) = try_peer(val, here, pinned);
            assert_refused(&reply);
            assert!(reply.contains(says), "{reply}");
            assert_eq!((saves, restarts), (0, 0));
        }
        assert_eq!(*peer_slot(), before);
    }

    #[test]
    fn pointer_speed_is_clamped() {
        let _guard = live_toggles();
        assert!(apply_toggle("set_pointer_speed", &json!({ "speed": 9.0 })));
        assert_eq!(pointer_speed(), 2.0);
        apply_toggle("set_pointer_speed", &json!({ "speed": 0.0 }));
        assert_eq!(pointer_speed(), 0.05);
        apply_toggle("set_pointer_speed", &json!({ "speed": 1e300 }));
        assert_eq!(pointer_speed(), 2.0);
        apply_toggle("set_pointer_speed", &json!({ "speed": 0.5 }));
        assert_eq!(pointer_speed(), 0.5);
        // Nothing usable leaves it alone.
        apply_toggle("set_pointer_speed", &json!({ "speed": "fast" }));
        apply_toggle("set_pointer_speed", &json!({}));
        assert_eq!(pointer_speed(), 0.5);
    }

    #[test]
    fn unknown_main_device_is_ignored() {
        let _guard = live_toggles();
        take_router_events();
        assert_eq!(change_main_device(&json!({ "device": 1 }), true, || ()), OK_RESPONSE);
        assert_eq!(take_router_events(), vec![Event::SetMain(1)]);
        // 256 and 257 must not wrap round to a real device.
        for device in [json!(7), json!(256), json!(257), json!(-1), json!(0.5), json!("0")] {
            let response = change_main_device(&json!({ "device": device }), true, || ());
            assert_refused(&response);
            assert_eq!(main_device(), 1);
            assert!(take_router_events().is_empty());
        }
        change_main_device(&json!({ "device": 0 }), true, || ());
        assert_eq!(take_router_events(), vec![Event::SetMain(0)]);
        assert_eq!(main_device(), 0);
    }

    fn assert_refused(response: &str) {
        let body: serde_json::Value = serde_json::from_str(response).unwrap();
        assert_eq!(body["ok"], false);
        assert!(!body["error"].as_str().unwrap().is_empty());
    }

    #[test]
    fn refused_main_change_is_not_saved() {
        let _guard = live_toggles();
        take_router_events();
        let before = main_device();
        let saves = std::cell::Cell::new(0);
        let save = || saves.set(saves.get() + 1);

        // Link down, with a device that would otherwise be fine.
        assert_refused(&change_main_device(&json!({ "device": 1 - before }), false, save));
        // Link up, with an unknown device.
        assert_refused(&change_main_device(&json!({ "device": 7 }), true, save));
        assert_eq!(saves.get(), 0);
        assert_eq!(main_device(), before);
        assert_eq!(current_settings().main_device, before);
        assert!(take_router_events().is_empty());

        assert_eq!(change_main_device(&json!({ "device": before }), true, save), OK_RESPONSE);
        assert_eq!(saves.get(), 1);
        take_router_events();
    }

    #[test]
    fn set_main_device_is_not_a_plain_toggle() {
        assert!(!apply_toggle("set_main_device", &json!({ "device": 1 })));
    }

    /// A fresh directory for a test of real file writes, away from the real settings.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saving_replaces_the_file_and_leaves_nothing_behind() {
        let dir = test_dir("save");
        let path = dir.join("local-settings.json");
        std::fs::write(&path, "old").unwrap();
        write_atomically(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_save_keeps_the_old_file() {
        let dir = test_dir("failed-save");
        let path = dir.join("local-settings.json");
        std::fs::write(&path, "old").unwrap();
        // Something in the temp file's place stops the write before the real file is touched.
        std::fs::create_dir(temp_path(&path)).unwrap();
        assert!(write_atomically(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_rename_cleans_up_the_temp_file() {
        let dir = test_dir("failed-rename");
        // A directory cannot be replaced by a file.
        let path = dir.join("local-settings.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), "").unwrap();
        assert!(write_atomically(&path, "new").is_err());
        assert!(!temp_path(&path).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sharing_changes_reach_the_router_once() {
        let _guard = live_toggles();
        take_router_events();
        assert!(take_router_events().is_empty());
        apply_toggle("set_sharing", &json!({ "mouse": false, "keyboard": true }));
        let want = Sharing { mouse: false, keyboard: true };
        assert_eq!(take_router_events(), vec![Event::SetSharing(want)]);
        assert!(take_router_events().is_empty());
        // A field left out keeps its value.
        apply_toggle("set_sharing", &json!({ "keyboard": false }));
        assert_eq!(sharing(), Sharing { mouse: false, keyboard: false });
        apply_toggle("set_sharing", &json!({ "mouse": true, "keyboard": true }));
        assert_eq!(sharing(), Sharing { mouse: true, keyboard: true });
        take_router_events();
    }

    #[test]
    fn clipboard_toggles_set_each_direction() {
        let _guard = live_toggles();
        assert!(apply_toggle("set_clipboard", &json!({ "send": false, "receive": true })));
        assert!(!CLIPBOARD_SEND.load(Ordering::SeqCst));
        assert!(CLIPBOARD_RECEIVE.load(Ordering::SeqCst));
        apply_toggle("set_clipboard", &json!({ "receive": false }));
        assert!(!CLIPBOARD_SEND.load(Ordering::SeqCst));
        assert!(!CLIPBOARD_RECEIVE.load(Ordering::SeqCst));
        apply_toggle("set_clipboard", &json!({ "send": true, "receive": true }));
        assert!(CLIPBOARD_SEND.load(Ordering::SeqCst));
        assert!(CLIPBOARD_RECEIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn play_only_on_the_other_device_is_on_unless_a_file_says_otherwise() {
        assert!(LocalSettings::default().play_only_on_other);
        assert!(parse_settings(r#"{"audio_tx_enabled":true}"#).play_only_on_other);
        assert!(!parse_settings(r#"{"mute_while_sending":false}"#).play_only_on_other, "the old name");
        assert!(!parse_settings(r#"{"play_only_on_other":false}"#).play_only_on_other);
    }

    #[test]
    fn play_only_on_the_other_device_is_a_saved_toggle() {
        let _guard = live_toggles();
        assert!(apply_toggle("set_play_only_on_other", &json!({ "enabled": false })));
        assert!(!AUDIO_PLAY_ONLY_ON_OTHER.load(Ordering::SeqCst));
        assert!(!current_settings().play_only_on_other);
        assert_eq!(audio_status_json()["play_only_on_other"], false);
        // Nothing usable leaves it alone.
        apply_toggle("set_play_only_on_other", &json!({ "enabled": "yes" }));
        apply_toggle("set_play_only_on_other", &json!({}));
        assert!(!AUDIO_PLAY_ONLY_ON_OTHER.load(Ordering::SeqCst));
        apply_toggle("set_play_only_on_other", &json!({ "enabled": true }));
        assert!(current_settings().play_only_on_other);
        assert_eq!(audio_status_json()["play_only_on_other"], true);
    }

    #[test]
    fn the_old_send_switch_becomes_a_device_choice() {
        let choice = |text: &str, linux: bool| audio_choice(&parse_settings(text), linux);
        let (off, default) = (devices::NONE.to_string(), devices::DEFAULT.to_string());

        let system_sound = choice(r#"{"audio_tx_enabled":true,"audio_tx_source":0}"#, true);
        assert_eq!((system_sound.input, system_sound.output), (off.clone(), default.clone()));

        let microphone = choice(r#"{"audio_tx_enabled":true,"audio_tx_source":1}"#, true);
        assert_eq!((microphone.input, microphone.output), (default.clone(), off.clone()));

        let windows = choice(r#"{"audio_tx_enabled":true,"audio_tx_source":1}"#, false);
        assert_eq!((windows.input, windows.output), (off.clone(), default.clone()), "it only ever sent system sound");

        let not_sending = choice(r#"{"audio_tx_enabled":false,"audio_tx_source":1}"#, true);
        assert_eq!((not_sending.input, not_sending.output), (off.clone(), off.clone()));
        assert_eq!(not_sending.play, default);
    }

    #[test]
    fn a_saved_choice_wins_over_the_old_switch_and_the_old_switch_is_not_saved_again() {
        let text = r#"{"audio_tx_enabled":true,"audio_input":"usb-mic","audio_output":"","audio_play":"headset"}"#;
        let s = parse_settings(text);
        let choice = audio_choice(&s, true);
        assert_eq!((choice.input.as_str(), choice.output.as_str(), choice.play.as_str()), ("usb-mic", "", "headset"));
        let saved = serde_json::to_string(&s).unwrap();
        assert!(!saved.contains("audio_tx_enabled") && !saved.contains("audio_tx_source"), "{saved}");
        assert!(saved.contains("play_only_on_other") && !saved.contains("mute_while_sending"), "{saved}");
    }

    #[test]
    fn a_bitrate_that_is_not_on_offer_becomes_the_default() {
        assert_eq!(usable_bitrate(64), 64);
        assert_eq!(usable_bitrate(0), 0, "uncompressed");
        assert_eq!(usable_bitrate(77), DEFAULT_BITRATE);
        assert_eq!(parse_settings("{}").audio_bitrate, DEFAULT_BITRATE);
    }

    #[test]
    fn device_choices_and_the_bitrate_change_through_actions() {
        let _guard = live_toggles();
        let before = devices::choice();
        let set = |role: &str, id: &str| {
            let saves = std::cell::Cell::new(0);
            let reply = change_audio_device(&json!({ "role": role, "id": id }), true, || saves.set(saves.get() + 1));
            assert_eq!(saves.get(), u32::from(reply == OK_RESPONSE), "saved only if changed: {reply}");
            reply
        };
        assert_eq!(set("input", "usb-mic"), OK_RESPONSE);
        assert_eq!(set("output", "speakers"), OK_RESPONSE);
        assert_eq!(set("play", "headset"), OK_RESPONSE);
        assert_eq!(audio_status_json()["input"], "usb-mic");
        assert_eq!(audio_status_json()["tx_enabled"], true, "choosing anything starts sending");
        assert!(set("play", devices::NONE).contains("has to play somewhere"));
        assert_eq!(devices::choice().play, "headset");
        assert!(set("nonsense", "x").contains("audio choices"));
        for role in ["input", "output"] {
            set(role, devices::NONE);
        }
        assert_eq!(audio_status_json()["tx_enabled"], false);
        devices::set_choice(before);

        let kbps = AUDIO_BITRATE_KBPS.load(Ordering::SeqCst);
        change_bitrate(Some(256));
        assert_eq!(audio_status_json()["bitrate"], 256);
        change_bitrate(Some(5));
        assert_eq!(audio_status_json()["bitrate"], 256, "not on offer");
        AUDIO_BITRATE_KBPS.store(kbps, Ordering::SeqCst);
    }

    #[test]
    fn a_device_id_has_to_be_one() {
        let _guard = live_toggles();
        let before = devices::choice();
        let try_it = |val: serde_json::Value| change_audio_device(&val, true, || panic!("nothing to save"));
        for refused in [
            json!({ "role": "output" }),
            json!({ "role": "output", "id": 5 }),
            json!({ "role": "output", "id": null }),
            json!({ "role": "output", "id": "x".repeat(MAX_AUDIO_DEVICE_ID + 1) }),
            json!({ "role": "output", "id": "two\nlines" }),
            json!({ "role": "output", "id": "bell\u{7}" }),
            json!({ "id": "speakers" }),
        ] {
            assert!(try_it(refused.clone()).contains("\"ok\":false"), "{refused}");
        }
        assert_eq!(devices::choice(), before, "a bad id isn't taken to mean None");
    }

    #[test]
    fn the_other_device_can_t_switch_on_this_one_s_microphone() {
        let _guard = live_toggles();
        let before = devices::choice();
        let asked = |role: &str, id: &str, here: bool| {
            let val = json!({ "role": role, "id": id });
            change_audio_device(&val, here, || {})
        };
        assert!(asked("input", "usb-mic", false).contains("device it belongs to"));
        assert!(asked("input", devices::DEFAULT, false).contains("device it belongs to"));
        assert_eq!(devices::choice(), before);
        assert_eq!(asked("output", "speakers", false), OK_RESPONSE, "an output is fair game");
        assert_eq!(asked("input", "usb-mic", true), OK_RESPONSE);
        assert_eq!(asked("input", devices::NONE, false), OK_RESPONSE, "and it can be switched off from there");
        devices::set_choice(before);
    }

    #[test]
    fn status_says_what_the_app_needs_to_show() {
        let status = audio_status_json();
        for field in ["rx_loss", "rx_unrepaired", "rx_buffer_ms", "tx_loss", "tx_reaching", "inputs", "outputs", "notes"] {
            assert!(!status[field].is_null(), "{field}");
        }
    }

    #[test]
    fn other_actions_are_not_toggles() {
        assert!(!apply_toggle("set_audio_tx_volume", &json!({})));
        assert!(!apply_toggle("nonsense", &json!({})));
    }

    #[test]
    fn status_reports_every_toggle() {
        let _guard = live_toggles();
        let mut status = json!({ "node": "debian" });
        add_toggle_status(&mut status, true);
        assert_eq!(status["node"], "debian");
        assert_eq!(status["sharing"]["mouse"], true);
        assert_eq!(status["sharing"]["keyboard"], true);
        assert_eq!(status["clipboard"]["send"], true);
        assert_eq!(status["clipboard"]["receive"], true);
        assert!(status["pointer_speed"].is_number());
        assert_eq!(status["main_device"], 0);
        assert_eq!(status["main_conflict"], true);
    }
}
