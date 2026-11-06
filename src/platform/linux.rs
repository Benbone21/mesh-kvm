//! Debian and other Linux desktops.

use super::Platform;
use crate::protocol::{MonitorRect, NodeId};
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT: &str = "mesh-kvm.service";
/// Where `systemctl --global enable` puts the link, as the package's install script does.
const GLOBAL_UNIT_DIR: &str = "/etc/systemd/user";
/// The targets a unit is enabled under: the older unit uses the first, the packaged one the second.
const UNIT_TARGETS: [&str; 2] = ["default.target.wants", "graphical-session.target.wants"];

pub struct LinuxPlatform {
    pub size: Option<(i32, i32)>,
}

impl Platform for LinuxPlatform {
    fn node(&self) -> NodeId {
        NodeId::Debian
    }

    fn label(&self) -> &'static str {
        "Debian Desktop"
    }

    fn os_name(&self) -> &'static str {
        "Debian 13 (KDE Plasma 6 Wayland)"
    }

    fn monitors(&self) -> Vec<MonitorRect> {
        if let Some(size) = self.size {
            return super::single_monitor(size);
        }
        Command::new("xrandr")
            .arg("--current")
            .output()
            .map(|output| parse_xrandr(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_default()
    }

    fn autostart_enabled(&self) -> bool {
        let user_dir = std::env::var_os("HOME").map(|home| Path::new(&home).join(".config/systemd/user"));
        let dirs: Vec<PathBuf> = user_dir.into_iter().chain([PathBuf::from(GLOBAL_UNIT_DIR)]).collect();
        enabled_in(&dirs)
    }

    fn set_autostart(&self, enabled: bool) {
        let verb = if enabled { "enable" } else { "disable" };
        let _ = Command::new("systemctl").args(["--user", verb, UNIT]).output();
    }

    fn input_devices(&self) -> &'static [&'static str] {
        &["Physical mouse (evdev)", "Desk keyboard (evdev)"]
    }

    fn can_pick_audio_source(&self) -> bool {
        true
    }

    /// The old service kept `layout.json` in the folder it ran from.
    fn old_layout_dir(&self) -> Option<PathBuf> {
        std::env::current_dir().ok()
    }
}

/// True if the unit is enabled under either target in any of `dirs`.
fn enabled_in(dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|dir| UNIT_TARGETS.iter().any(|target| dir.join(target).join(UNIT).exists()))
}

/// Reads the connected monitors, their positions and refresh rates from `xrandr --current`.
fn parse_xrandr(text: &str) -> Vec<MonitorRect> {
    let mut monitors = Vec::new();
    let mut current_mon: Option<MonitorRect> = None;

    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') && line.contains(" connected") {
            if let Some(m) = current_mon.take() {
                monitors.push(m);
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            let out_name = parts.first().copied().unwrap_or("Display").to_string();
            let is_primary = line.contains(" primary ");
            // Find token matching WxH+X+Y
            for tok in &parts {
                if tok.contains('x') && tok.contains('+') {
                    let nums: Vec<i32> = tok
                        .split(|c: char| !c.is_ascii_digit() && c != '-')
                        .filter(|s| !s.is_empty())
                        .filter_map(|s| s.parse::<i32>().ok())
                        .collect();
                    if nums.len() >= 4 {
                        let label = if is_primary {
                            format!("{} (Primary Native)", out_name)
                        } else {
                            format!("{} (Native)", out_name)
                        };
                        current_mon = Some(MonitorRect {
                            name: label,
                            w: nums[0],
                            h: nums[1],
                            x: nums[2],
                            y: nums[3],
                        });
                        break;
                    }
                }
            }
        } else if line.contains('*') {
            // Active refresh rate line, e.g. "   2560x1440    359.80*+"
            if let Some(ref mut m) = current_mon {
                for tok in line.split_whitespace() {
                    if tok.contains('*') {
                        let clean: String = tok
                            .chars()
                            .take_while(|c| c.is_ascii_digit() || *c == '.')
                            .collect();
                        if let Ok(hz) = clean.parse::<f32>() {
                            let base_name = m.name.split(' ').next().unwrap_or(&m.name).to_string();
                            m.name = format!("{} ({:.0}Hz Native)", base_name, hz.round());
                        }
                        break;
                    }
                }
            }
        }
    }
    if let Some(m) = current_mon.take() {
        monitors.push(m);
    }
    monitors.sort_by_key(|m| (m.x, m.y));
    monitors
}

#[cfg(test)]
mod tests {
    use super::*;

    const XRANDR: &str = "Screen 0: minimum 16 x 16, current 4480 x 1440, maximum 32767 x 32767\n\
DP-2 connected primary 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm\n\
   2560x1440    359.80*+\n\
   1920x1080    144.00\n\
DP-3 connected 1920x1080+0+360 (normal left inverted right x axis y axis) 527mm x 296mm\n\
   1920x1080    239.76*+\n\
HDMI-1 disconnected (normal left inverted right x axis y axis)\n";

    #[test]
    fn xrandr_gives_every_connected_monitor_left_to_right() {
        let monitors = parse_xrandr(XRANDR);
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0], MonitorRect { name: "DP-3 (240Hz Native)".into(), x: 0, y: 360, w: 1920, h: 1080 });
        assert_eq!(monitors[1], MonitorRect { name: "DP-2 (360Hz Native)".into(), x: 1920, y: 0, w: 2560, h: 1440 });
    }

    #[test]
    fn no_xrandr_output_means_no_monitors() {
        assert!(parse_xrandr("").is_empty());
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-autostart-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn autostart_is_seen_under_either_target_and_in_the_global_folder() {
        for target in UNIT_TARGETS {
            let (user, global) = (temp(&format!("{target}-user")), temp(&format!("{target}-global")));
            let dirs = [user.clone(), global.clone()];
            assert!(!enabled_in(&dirs));
            std::fs::create_dir_all(global.join(target)).unwrap();
            std::fs::write(global.join(target).join(UNIT), "").unwrap();
            assert!(enabled_in(&dirs), "{target}");
            let _ = std::fs::remove_dir_all(user);
            let _ = std::fs::remove_dir_all(global);
        }
    }
}
