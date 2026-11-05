//! Per-device persisted settings (audio routing prefs) + local actions shared by both nodes.

use crate::audio::{
    AUDIO_RX_MUTED, AUDIO_RX_PACKETS, AUDIO_RX_PEAK_PCT, AUDIO_RX_VOLUME_PCT, AUDIO_TEST_CHIME_REQ,
    AUDIO_TX_ENABLED, AUDIO_TX_PACKETS, AUDIO_TX_PEAK_PCT, AUDIO_TX_SOURCE_MODE,
    AUDIO_TX_VOLUME_PCT,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalSettings {
    pub audio_tx_enabled: bool,
    pub audio_tx_volume: u32,
    pub audio_tx_source: u32,
    pub audio_rx_muted: bool,
    pub audio_rx_volume: u32,
}

impl Default for LocalSettings {
    fn default() -> Self {
        Self {
            audio_tx_enabled: false,
            audio_tx_volume: 100,
            audio_tx_source: 0,
            audio_rx_muted: false,
            audio_rx_volume: 100,
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

fn settings_path() -> PathBuf {
    config_dir().join("local-settings.json")
}

/// Loads persisted settings and applies them to the live audio atomics.
pub fn load_and_apply() {
    let s: LocalSettings = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    AUDIO_TX_ENABLED.store(s.audio_tx_enabled, Ordering::SeqCst);
    AUDIO_TX_VOLUME_PCT.store(s.audio_tx_volume.min(150), Ordering::SeqCst);
    AUDIO_TX_SOURCE_MODE.store(s.audio_tx_source.min(1), Ordering::SeqCst);
    AUDIO_RX_MUTED.store(s.audio_rx_muted, Ordering::SeqCst);
    AUDIO_RX_VOLUME_PCT.store(s.audio_rx_volume.min(150), Ordering::SeqCst);
}

pub fn save_current() {
    let s = LocalSettings {
        audio_tx_enabled: AUDIO_TX_ENABLED.load(Ordering::Relaxed),
        audio_tx_volume: AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed),
        audio_tx_source: AUDIO_TX_SOURCE_MODE.load(Ordering::Relaxed),
        audio_rx_muted: AUDIO_RX_MUTED.load(Ordering::Relaxed),
        audio_rx_volume: AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed),
    };
    let _ = std::fs::create_dir_all(config_dir());
    if let Ok(json) = serde_json::to_string_pretty(&s) {
        let _ = std::fs::write(settings_path(), json);
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

/// Handles audio actions common to both nodes. Returns true if the action was recognised.
pub fn handle_common_action(act: &str, val: &serde_json::Value) -> bool {
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
