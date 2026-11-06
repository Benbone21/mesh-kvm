//! What the service needs from the operating system beyond input, one implementation per OS, in
//! the style of `InputBackend` and `MuteBackend`. A Mac port adds a file here.
//!
//! Audio capture and playback stay in `audio.rs` for now, and input capture in `input/`: stage 3
//! rewrites the audio code, so moving it behind this trait first would only be done twice.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
pub mod windows;

use crate::protocol::{MonitorRect, NodeId};
use std::path::PathBuf;
use std::sync::Arc;

/// The `--role` value for this device.
#[cfg(target_os = "linux")]
pub const ROLE: &str = "debian";
#[cfg(target_os = "windows")]
pub const ROLE: &str = "windows";

pub trait Platform: Send + Sync {
    fn node(&self) -> NodeId;
    /// The device's kind, as `/api/local` reports it.
    fn label(&self) -> &'static str;
    fn os_name(&self) -> &'static str;
    /// Every monitor, in this device's desktop coordinates. Empty if detection failed. Can be
    /// slow, so never call it holding a lock.
    fn monitors(&self) -> Vec<MonitorRect>;
    fn autostart_enabled(&self) -> bool;
    fn set_autostart(&self, enabled: bool);
    fn input_devices(&self) -> &'static [&'static str];
    /// True if this device can choose between system sound and the microphone.
    fn can_pick_audio_source(&self) -> bool;
    /// Where an install from before layouts moved to the config folder kept its `layout.json`.
    fn old_layout_dir(&self) -> Option<PathBuf> {
        None
    }
}

/// This device's platform. `size` replaces what is detected with one monitor of that size.
pub fn current(size: Option<(i32, i32)>) -> Arc<dyn Platform> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(linux::LinuxPlatform { size })
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(windows::WindowsPlatform { size })
    }
}

/// `--width` and `--height`, as one monitor.
pub fn single_monitor(size: (i32, i32)) -> Vec<MonitorRect> {
    vec![MonitorRect { name: "Display".to_string(), x: 0, y: 0, w: size.0, h: size.1 }]
}
