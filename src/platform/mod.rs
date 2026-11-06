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
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// The `--role` value for this device.
#[cfg(target_os = "linux")]
pub const ROLE: &str = "debian";
#[cfg(target_os = "windows")]
pub const ROLE: &str = "windows";

/// A resolution and refresh rate a monitor can run at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayMode {
    pub width: i32,
    pub height: i32,
    /// In thousandths of a hertz, as rates like 59.94 are real and must compare exactly.
    pub millihertz: u32,
    /// The operating system's own handle for the mode, where it has one.
    #[serde(default)]
    pub id: String,
}

impl DisplayMode {
    /// True for the same resolution and rate, whatever the handles say.
    pub fn same_as(&self, other: &DisplayMode) -> bool {
        (self.width, self.height, self.millihertz) == (other.width, other.height, other.millihertz)
    }

    /// The rate to the nearest hertz, which is what monitors are sold by.
    pub fn whole_hertz(&self) -> u32 {
        (self.millihertz + 500) / 1000
    }

    pub fn label(&self) -> String {
        format!("{} \u{d7} {} at {} Hz", self.width, self.height, self.whole_hertz())
    }
}

/// One monitor, and what it can do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayInfo {
    /// How the operating system knows it. A `MonitorRect`'s name starts with this.
    pub id: String,
    /// What the person would call it: its model if it says, else `id`.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub current: Option<DisplayMode>,
    /// Best first.
    pub modes: Vec<DisplayMode>,
    /// False when the modes can only be listed here.
    pub can_change: bool,
}

/// Puts modes in the order a menu wants them, largest and fastest first, with no repeats.
pub fn sort_modes(modes: &mut Vec<DisplayMode>) {
    modes.sort_by_key(|m| std::cmp::Reverse((m.width * m.height, m.width, m.millihertz)));
    modes.dedup_by(|a, b| a.same_as(b));
}

pub trait Platform: Send + Sync {
    fn node(&self) -> NodeId;
    /// The device's kind, as `/api/local` reports it.
    fn label(&self) -> &'static str;
    fn os_name(&self) -> &'static str;
    /// Every monitor, in this device's desktop coordinates. Empty if detection failed. Can be
    /// slow, so never call it holding a lock.
    fn monitors(&self) -> Vec<MonitorRect>;
    /// Every monitor and the modes it offers. Slower than `monitors`, so never call it holding a
    /// lock or on a request: `DisplayManager` keeps the answer.
    fn displays(&self) -> Vec<DisplayInfo> {
        Vec::new()
    }
    /// Switches `output`, a `DisplayInfo::id`, to `mode`. `Err` is worded for the person to read.
    fn set_display_mode(&self, _output: &str, _mode: &DisplayMode) -> Result<(), String> {
        Err("This device can't change its screens yet.".to_string())
    }
    fn autostart_enabled(&self) -> bool;
    fn set_autostart(&self, enabled: bool);
    fn input_devices(&self) -> &'static [&'static str];
    /// True if this device can choose between system sound and the microphone.
    fn can_pick_audio_source(&self) -> bool;
    /// Where an install from before layouts moved to the config folder may have left its
    /// `layout.json`, best guess first.
    fn old_layout_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
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
        Arc::new(windows::WindowsPlatform::new(size))
    }
}

/// The one monitor of `--width` and `--height`, which is only a size and has no modes to change.
pub fn single_display(size: (i32, i32)) -> Vec<DisplayInfo> {
    let current = DisplayMode { width: size.0, height: size.1, millihertz: 0, id: String::new() };
    vec![DisplayInfo { id: "Display".into(), name: "Display".into(), current: Some(current), ..Default::default() }]
}

/// `--width` and `--height`, as one monitor.
pub fn single_monitor(size: (i32, i32)) -> Vec<MonitorRect> {
    vec![MonitorRect { name: "Display".to_string(), x: 0, y: 0, w: size.0, h: size.1 }]
}
