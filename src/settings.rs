//! Per-device persisted settings (audio routing, toggles) + local actions shared by both nodes.

use crate::audio::{
    AUDIO_RX_MUTED, AUDIO_RX_PACKETS, AUDIO_RX_PEAK_PCT, AUDIO_RX_VOLUME_PCT, AUDIO_TEST_CHIME_REQ,
    AUDIO_TX_ENABLED, AUDIO_TX_PACKETS, AUDIO_TX_PEAK_PCT, AUDIO_TX_SOURCE_MODE,
    AUDIO_TX_VOLUME_PCT,
};
use crate::clipboard::{CLIPBOARD_RECEIVE, CLIPBOARD_SEND};
use crate::protocol::NodeId;
use crate::router::{DeviceId, Event, Sharing};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, PoisonError};

const MIN_POINTER_SPEED: f64 = 0.05;
const MAX_POINTER_SPEED: f64 = 2.0;
const DEFAULT_POINTER_SPEED: f32 = 1.0;
const DEFAULT_MAIN_DEVICE: DeviceId = NodeId::Debian as DeviceId;
pub const OK_RESPONSE: &str = r#"{"ok":true}"#;

static MOUSE_SHARING: AtomicBool = AtomicBool::new(true);
static KEYBOARD_SHARING: AtomicBool = AtomicBool::new(true);
/// The laptop's pointer speed, as `f32` bits. Debian's is `mouse_speed` in the layout.
static POINTER_SPEED_BITS: AtomicU32 = AtomicU32::new(DEFAULT_POINTER_SPEED.to_bits());
static MAIN_DEVICE: AtomicU8 = AtomicU8::new(DEFAULT_MAIN_DEVICE);
/// Set after the values above change, for the input loop to feed its router.
static SHARING_PENDING: AtomicBool = AtomicBool::new(false);
static MAIN_PENDING: AtomicBool = AtomicBool::new(false);
/// One save at a time: they share a temp file, and the last to finish must have the newest values.
static SAVING: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalSettings {
    pub audio_tx_enabled: bool,
    pub audio_tx_volume: u32,
    pub audio_tx_source: u32,
    pub audio_rx_muted: bool,
    pub audio_rx_volume: u32,
    pub mouse_sharing: bool,
    pub keyboard_sharing: bool,
    pub clipboard_send: bool,
    pub clipboard_receive: bool,
    pub pointer_speed: f32,
    pub main_device: u8,
}

impl Default for LocalSettings {
    fn default() -> Self {
        Self {
            audio_tx_enabled: false,
            audio_tx_volume: 100,
            audio_tx_source: 0,
            audio_rx_muted: false,
            audio_rx_volume: 100,
            mouse_sharing: true,
            keyboard_sharing: true,
            clipboard_send: true,
            clipboard_receive: true,
            pointer_speed: DEFAULT_POINTER_SPEED,
            main_device: DEFAULT_MAIN_DEVICE,
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
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/bread".to_string());
        PathBuf::from(home).join(".config").join("mesh-kvm")
    }
}

/// Writes a fatal error to `mesh-kvm.log`. The Windows build has no console, and a window started
/// from a menu has nowhere to print, so the error would otherwise vanish.
pub fn log_fatal(message: &str) {
    append_fatal(&config_dir(), message);
}

/// Writes a plain note to `mesh-kvm.log`, for things worth knowing that aren't fatal.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
    s
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

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn pointer_speed() -> f32 {
    f32::from_bits(POINTER_SPEED_BITS.load(Ordering::SeqCst))
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
    AUDIO_TX_ENABLED.store(s.audio_tx_enabled, Ordering::SeqCst);
    AUDIO_TX_VOLUME_PCT.store(s.audio_tx_volume.min(150), Ordering::SeqCst);
    AUDIO_TX_SOURCE_MODE.store(s.audio_tx_source.min(1), Ordering::SeqCst);
    AUDIO_RX_MUTED.store(s.audio_rx_muted, Ordering::SeqCst);
    AUDIO_RX_VOLUME_PCT.store(s.audio_rx_volume.min(150), Ordering::SeqCst);
    MOUSE_SHARING.store(s.mouse_sharing, Ordering::SeqCst);
    KEYBOARD_SHARING.store(s.keyboard_sharing, Ordering::SeqCst);
    CLIPBOARD_SEND.store(s.clipboard_send, Ordering::SeqCst);
    CLIPBOARD_RECEIVE.store(s.clipboard_receive, Ordering::SeqCst);
    POINTER_SPEED_BITS.store(s.pointer_speed.to_bits(), Ordering::SeqCst);
    MAIN_DEVICE.store(s.main_device, Ordering::SeqCst);
}

fn current_settings() -> LocalSettings {
    LocalSettings {
        audio_tx_enabled: AUDIO_TX_ENABLED.load(Ordering::Relaxed),
        audio_tx_volume: AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed),
        audio_tx_source: AUDIO_TX_SOURCE_MODE.load(Ordering::Relaxed),
        audio_rx_muted: AUDIO_RX_MUTED.load(Ordering::Relaxed),
        audio_rx_volume: AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed),
        mouse_sharing: MOUSE_SHARING.load(Ordering::SeqCst),
        keyboard_sharing: KEYBOARD_SHARING.load(Ordering::SeqCst),
        clipboard_send: CLIPBOARD_SEND.load(Ordering::SeqCst),
        clipboard_receive: CLIPBOARD_RECEIVE.load(Ordering::SeqCst),
        pointer_speed: f32::from_bits(POINTER_SPEED_BITS.load(Ordering::SeqCst)),
        main_device: main_device(),
    }
}

fn temp_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

/// Replaces `path` in one step, so that a crash leaves the old file or the new one, never half.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
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

pub fn audio_status_json(supports_source_select: bool) -> serde_json::Value {
    serde_json::json!({
        "tx_enabled": AUDIO_TX_ENABLED.load(Ordering::Relaxed),
        "tx_volume": AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed),
        "tx_peak": AUDIO_TX_PEAK_PCT.load(Ordering::Relaxed),
        "tx_packets": AUDIO_TX_PACKETS.load(Ordering::Relaxed),
        "tx_source": if supports_source_select {
            serde_json::json!(AUDIO_TX_SOURCE_MODE.load(Ordering::Relaxed))
        } else {
            serde_json::Value::Null
        },
        "rx_muted": AUDIO_RX_MUTED.load(Ordering::Relaxed),
        "rx_volume": AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed),
        "rx_peak": AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed),
        "rx_packets": AUDIO_RX_PACKETS.load(Ordering::Relaxed),
    })
}

/// Adds the toggles to a `/api/local` status. Debian's pointer speed lives in its layout, so each
/// node passes its own, along with whether its router has a main conflict.
pub fn add_toggle_status(status: &mut serde_json::Value, pointer_speed: f32, main_conflict: bool) {
    let sharing = sharing();
    status["sharing"] = serde_json::json!({ "mouse": sharing.mouse, "keyboard": sharing.keyboard });
    status["clipboard"] = serde_json::json!({
        "send": CLIPBOARD_SEND.load(Ordering::SeqCst),
        "receive": CLIPBOARD_RECEIVE.load(Ordering::SeqCst),
    });
    status["pointer_speed"] = serde_json::json!(pointer_speed);
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
        return refuse("Unknown device. Choose 0 for Debian or 1 for the laptop.");
    };
    if !link_up {
        return refuse("The devices aren't linked. Reconnect them, then change the main device.");
    }
    MAIN_DEVICE.store(device, Ordering::SeqCst);
    MAIN_PENDING.store(true, Ordering::SeqCst);
    save();
    OK_RESPONSE.to_string()
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
        "set_audio_tx" => {
            let en = flag("enabled").unwrap_or(!AUDIO_TX_ENABLED.load(Ordering::Relaxed));
            AUDIO_TX_ENABLED.store(en, Ordering::SeqCst);
        }
        "set_audio_tx_volume" => {
            if let Some(v) = num("volume") {
                AUDIO_TX_VOLUME_PCT.store(v.min(150) as u32, Ordering::SeqCst);
            }
        }
        "set_audio_tx_source" => {
            if let Some(v) = num("mode") {
                AUDIO_TX_SOURCE_MODE.store((v as u32).min(1), Ordering::SeqCst);
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
            AUDIO_TEST_CHIME_REQ.store(false, Ordering::SeqCst);
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
    fn other_actions_are_not_toggles() {
        assert!(!apply_toggle("set_audio_tx", &json!({})));
        assert!(!apply_toggle("nonsense", &json!({})));
    }

    #[test]
    fn status_reports_every_toggle() {
        let _guard = live_toggles();
        let mut status = json!({ "node": "debian" });
        add_toggle_status(&mut status, 0.5, true);
        assert_eq!(status["node"], "debian");
        assert_eq!(status["sharing"]["mouse"], true);
        assert_eq!(status["sharing"]["keyboard"], true);
        assert_eq!(status["clipboard"]["send"], true);
        assert_eq!(status["clipboard"]["receive"], true);
        assert_eq!(status["pointer_speed"], 0.5);
        assert_eq!(status["main_device"], 0);
        assert_eq!(status["main_conflict"], true);
    }
}
