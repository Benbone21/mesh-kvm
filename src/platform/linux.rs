//! Debian and other Linux desktops.

use super::Platform;
use crate::protocol::{MonitorRect, NodeId};
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT: &str = "mesh-kvm.service";
/// Where `systemctl --global enable` puts the link, as the package's install script does.
const GLOBAL_UNIT_DIR: &str = "/etc/systemd/user";
const GLOBAL_AUTOSTART_DIR: &str = "/etc/xdg/autostart";
const TRAY_ENTRY: &str = "mesh-kvm-tray.desktop";
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
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let places = AutostartPlaces::at(home.as_deref().unwrap_or(Path::new(".")), Path::new(GLOBAL_UNIT_DIR), Path::new(GLOBAL_AUTOSTART_DIR));
        places.starts_at_login(crate::settings::autostart_disabled_marker().exists())
    }

    fn set_autostart(&self, enabled: bool) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
        let places = AutostartPlaces::at(&home, Path::new(GLOBAL_UNIT_DIR), Path::new(GLOBAL_AUTOSTART_DIR));
        let marker = crate::settings::autostart_disabled_marker();
        if enabled {
            let _ = std::fs::remove_file(&marker);
            if !places.has_tray_entry() {
                if let Err(e) = crate::app::launch::write_tray_autostart(&home) {
                    crate::settings::log_note(&format!("Couldn't turn on starting at boot: {e}"));
                }
            }
        } else {
            let _ = std::fs::create_dir_all(crate::settings::config_dir());
            if let Err(e) = std::fs::write(&marker, "") {
                crate::settings::log_note(&format!("Couldn't turn off starting at boot: {e}"));
            }
        }
        for verb in autostart_commands(enabled, places.has_own_unit_file()) {
            let _ = Command::new("systemctl").args(["--user", verb, UNIT]).output();
        }
    }

    fn input_devices(&self) -> &'static [&'static str] {
        &["Physical mouse (evdev)", "Desk keyboard (evdev)"]
    }

    fn can_pick_audio_source(&self) -> bool {
        true
    }

    fn old_layout_dirs(&self) -> Vec<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let exe = crate::app::launch::running_exe().ok();
        old_layout_dirs(std::env::current_dir().ok(), home, exe)
    }
}

/// The old service kept `layout.json` in the folder it ran from, which was the source folder: a
/// start from anywhere else has to go and look. That's usually `~/mesh-kvm`, or above the exe when
/// it is run from `target/release`.
fn old_layout_dirs(current: Option<PathBuf>, home: Option<PathBuf>, exe: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = current.into_iter().collect();
    dirs.extend(home.map(|home| home.join("mesh-kvm")));
    if let Some(exe) = exe {
        dirs.extend(exe.ancestors().skip(1).map(Path::to_path_buf));
    }
    dirs
}

/// What a login does here. There are three ways a device can be set up: a unit file of the
/// person's own, the package's unit, which is enabled for everyone, or no unit at all. In all
/// three the tray's login entry starts the service too, if it finds none running.
struct AutostartPlaces {
    user_units: PathBuf,
    global_units: PathBuf,
    tray_entries: [PathBuf; 2],
}

impl AutostartPlaces {
    fn at(home: &Path, global_units: &Path, global_autostart: &Path) -> Self {
        Self {
            user_units: home.join(".config/systemd/user"),
            global_units: global_units.to_path_buf(),
            tray_entries: [home.join(".config/autostart").join(TRAY_ENTRY), global_autostart.join(TRAY_ENTRY)],
        }
    }

    /// A real unit file in the person's folder, as opposed to none, or the link a mask leaves.
    fn has_own_unit_file(&self) -> bool {
        std::fs::symlink_metadata(self.user_units.join(UNIT)).is_ok_and(|m| m.file_type().is_file())
    }

    fn is_masked(&self) -> bool {
        std::fs::read_link(self.user_units.join(UNIT)).is_ok_and(|target| target == Path::new("/dev/null"))
    }

    fn unit_enabled(&self) -> bool {
        let enabled_in = |dir: &Path| UNIT_TARGETS.iter().any(|target| dir.join(target).join(UNIT).exists());
        !self.is_masked() && (enabled_in(&self.user_units) || enabled_in(&self.global_units))
    }

    fn has_tray_entry(&self) -> bool {
        self.tray_entries.iter().any(|entry| entry.exists())
    }

    /// True if the service will be running after the next login. `turned_off` is the person's
    /// choice, which the tray keeps to as well.
    fn starts_at_login(&self, turned_off: bool) -> bool {
        !turned_off && (self.unit_enabled() || self.has_tray_entry())
    }
}

/// The `systemctl --user` verbs that make a login start the unit or not. `--user disable` can't
/// touch the link the package enables for everyone, so that unit is masked for this person. A
/// unit file of their own can't be masked, as the mask would have to replace it.
fn autostart_commands(enabled: bool, own_unit_file: bool) -> &'static [&'static str] {
    match (enabled, own_unit_file) {
        (true, true) => &["enable"],
        (false, true) => &["disable"],
        (true, false) => &["unmask"],
        (false, false) => &["mask"],
    }
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

    fn places(name: &str) -> (PathBuf, AutostartPlaces) {
        let root = temp(name);
        let places = AutostartPlaces::at(&root.join("home"), &root.join("global"), &root.join("xdg"));
        std::fs::create_dir_all(&places.user_units).unwrap();
        (root, places)
    }

    fn enable_in(dir: &Path, target: &str) {
        std::fs::create_dir_all(dir.join(target)).unwrap();
        std::fs::write(dir.join(target).join(UNIT), "").unwrap();
    }

    #[test]
    fn a_unit_is_seen_as_enabled_under_either_target_for_this_person_or_for_everyone() {
        for target in UNIT_TARGETS {
            let (root, places) = places(&format!("{target}-user"));
            assert!(!places.starts_at_login(false));
            enable_in(&places.user_units, target);
            assert!(places.starts_at_login(false), "{target}");
            let _ = std::fs::remove_dir_all(root);
        }
        let (root, places) = places("packaged");
        enable_in(&places.global_units, UNIT_TARGETS[1]);
        assert!(places.starts_at_login(false));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_masked_packaged_unit_is_off_even_with_the_global_link_still_there() {
        let (root, places) = places("masked");
        enable_in(&places.global_units, UNIT_TARGETS[1]);
        std::os::unix::fs::symlink("/dev/null", places.user_units.join(UNIT)).unwrap();
        assert!(places.is_masked());
        assert!(!places.has_own_unit_file(), "the mask isn't a unit file of the person's own");
        assert!(!places.starts_at_login(false));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn with_no_unit_at_all_the_tray_entry_is_what_starts_it() {
        let (root, places) = places("none");
        std::fs::create_dir_all(places.tray_entries[0].parent().unwrap()).unwrap();
        assert!(!places.starts_at_login(false));
        std::fs::write(&places.tray_entries[0], "").unwrap();
        assert!(places.starts_at_login(false));
        assert!(!places.starts_at_login(true), "the tray keeps to the person's choice");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn turned_off_means_off_however_the_unit_is_enabled() {
        let (root, places) = places("off");
        enable_in(&places.global_units, UNIT_TARGETS[1]);
        enable_in(&places.user_units, UNIT_TARGETS[0]);
        assert!(!places.starts_at_login(true));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_toggle_uses_what_works_for_each_kind_of_unit() {
        let (root, places) = places("own-file");
        assert!(!places.has_own_unit_file());
        std::fs::write(places.user_units.join(UNIT), "[Unit]\n").unwrap();
        assert!(places.has_own_unit_file());
        let _ = std::fs::remove_dir_all(root);
        assert_eq!(autostart_commands(false, true), ["disable"]);
        assert_eq!(autostart_commands(true, true), ["enable"]);
        assert_eq!(autostart_commands(false, false), ["mask"]);
        assert_eq!(autostart_commands(true, false), ["unmask"]);
    }

    #[test]
    fn an_old_layout_is_looked_for_here_then_in_home_then_above_the_exe() {
        let dirs = old_layout_dirs(
            Some("/somewhere".into()),
            Some("/home/me".into()),
            Some("/home/me/src/mesh-kvm/target/release/mesh-kvm".into()),
        );
        assert_eq!(dirs[0], Path::new("/somewhere"));
        assert_eq!(dirs[1], Path::new("/home/me/mesh-kvm"));
        assert!(dirs.contains(&PathBuf::from("/home/me/src/mesh-kvm")));
        assert!(!dirs.contains(&PathBuf::from("/home/me/src/mesh-kvm/target/release/mesh-kvm")));
        assert!(old_layout_dirs(None, None, None).is_empty());
    }
}
