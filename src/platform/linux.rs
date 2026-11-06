//! Debian and other Linux desktops.

use super::{DisplayInfo, DisplayMode, Platform};
use crate::mute::platform::run_with_deadline;
use crate::protocol::{MonitorRect, NodeId};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const UNIT: &str = "mesh-kvm.service";
/// Where `systemctl --global enable` puts the link, as the package's install script does.
const GLOBAL_UNIT_DIR: &str = "/etc/systemd/user";
const GLOBAL_AUTOSTART_DIR: &str = "/etc/xdg/autostart";
const TRAY_ENTRY: &str = "mesh-kvm-tray.desktop";
/// The targets a unit is enabled under: the older unit uses the first, the packaged one the second.
const UNIT_TARGETS: [&str; 2] = ["default.target.wants", "graphical-session.target.wants"];
const KSCREEN: &str = "kscreen-doctor";
/// It can hang with no compositor to talk to, and the look at the monitors must carry on.
const KSCREEN_LIST_DEADLINE: Duration = Duration::from_secs(5);
/// A monitor can take a few seconds to come back on a new mode.
const KSCREEN_CHANGE_DEADLINE: Duration = Duration::from_secs(10);
const DRM_DIR: &str = "/sys/class/drm";

pub struct LinuxPlatform {
    pub size: Option<(i32, i32)>,
}

impl Platform for LinuxPlatform {
    fn node(&self) -> NodeId {
        NodeId::Debian
    }

    fn label(&self) -> &'static str {
        "Linux desktop"
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

    fn displays(&self) -> Vec<DisplayInfo> {
        if let Some(size) = self.size {
            return super::single_display(size);
        }
        let mut command = Command::new(KSCREEN);
        command.arg("-j");
        let listed = run_with_deadline(command, KSCREEN_LIST_DEADLINE)
            .filter(|output| output.status.success())
            .map(|output| parse_kscreen(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_default();
        let mut displays = if listed.is_empty() { self.listed_by_xrandr() } else { listed };
        for display in &mut displays {
            if let Some(model) = drm_model(Path::new(DRM_DIR), &display.id) {
                display.name = model;
            }
        }
        displays
    }

    fn set_display_mode(&self, output: &str, mode: &DisplayMode) -> Result<(), String> {
        let mut command = Command::new(KSCREEN);
        command.arg(mode_argument(output, mode));
        match run_with_deadline(command, KSCREEN_CHANGE_DEADLINE) {
            Some(result) if result.status.success() => Ok(()),
            Some(_) => Err("The desktop didn't accept that mode.".to_string()),
            None => Err("The desktop didn't answer. Check that kscreen-doctor is installed.".to_string()),
        }
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

impl LinuxPlatform {
    /// What `xrandr` says, for a desktop `kscreen-doctor` can't talk to. It can list, but not change.
    fn listed_by_xrandr(&self) -> Vec<DisplayInfo> {
        Command::new("xrandr")
            .arg("--current")
            .output()
            .map(|output| parse_xrandr_displays(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_default()
    }
}

/// `kscreen-doctor`'s setting for `mode` on `output`. Both come from its own listing.
fn mode_argument(output: &str, mode: &DisplayMode) -> String {
    format!("output.{output}.mode.{}", mode.id)
}

#[derive(Deserialize)]
struct KscreenList {
    #[serde(default)]
    outputs: Vec<KscreenOutput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KscreenOutput {
    name: String,
    #[serde(default)]
    connected: bool,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    current_mode_id: String,
    #[serde(default)]
    pos: KscreenPosition,
    #[serde(default)]
    modes: Vec<KscreenMode>,
}

#[derive(Default, Deserialize)]
struct KscreenPosition {
    x: i32,
    y: i32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KscreenMode {
    id: String,
    refresh_rate: f64,
    size: KscreenSize,
}

#[derive(Deserialize)]
struct KscreenSize {
    width: i32,
    height: i32,
}

impl KscreenMode {
    fn to_mode(&self) -> DisplayMode {
        DisplayMode {
            width: self.size.width,
            height: self.size.height,
            millihertz: (self.refresh_rate * 1000.0).round() as u32,
            id: self.id.clone(),
        }
    }
}

/// Reads `kscreen-doctor -j`. Anything else gives no displays.
fn parse_kscreen(text: &str) -> Vec<DisplayInfo> {
    let list: KscreenList = serde_json::from_str(text).unwrap_or(KscreenList { outputs: Vec::new() });
    let mut displays: Vec<DisplayInfo> = list
        .outputs
        .iter()
        .filter(|output| output.connected && output.enabled)
        .map(|output| {
            let current = output.modes.iter().find(|m| m.id == output.current_mode_id).map(KscreenMode::to_mode);
            let mut modes: Vec<DisplayMode> = output.modes.iter().map(KscreenMode::to_mode).collect();
            super::sort_modes(&mut modes);
            DisplayInfo {
                id: output.name.clone(),
                name: output.name.clone(),
                x: output.pos.x,
                y: output.pos.y,
                current,
                modes,
                can_change: true,
            }
        })
        .collect();
    displays.sort_by_key(|d| (d.x, d.y));
    displays
}

/// The monitors `xrandr --current` lists, with their modes and no way to change them.
fn parse_xrandr_displays(text: &str) -> Vec<DisplayInfo> {
    let mut displays: Vec<DisplayInfo> = Vec::new();
    let mut listing = false;
    for line in text.lines() {
        if line.contains(" connected") {
            let id = line.split_whitespace().next().unwrap_or("Display").to_string();
            displays.push(DisplayInfo { name: id.clone(), id, ..Default::default() });
            listing = true;
            continue;
        }
        listing &= !line.contains(" disconnected");
        let Some(display) = displays.last_mut().filter(|_| listing) else { continue };
        let mut words = line.split_whitespace();
        let Some((w, h)) = words.next().and_then(|size| size.split_once('x')) else { continue };
        let (Ok(width), Ok(height)) = (w.parse(), h.trim_end_matches('i').parse()) else { continue };
        for rate in words {
            let digits: String = rate.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            let Ok(hz) = digits.parse::<f64>() else { continue };
            let mode = DisplayMode { width, height, millihertz: (hz * 1000.0).round() as u32, id: String::new() };
            if rate.contains('*') {
                display.current = Some(mode.clone());
            }
            display.modes.push(mode);
        }
    }
    for display in &mut displays {
        super::sort_modes(&mut display.modes);
    }
    displays
}

/// The model name a monitor gives in its EDID, from the folder the kernel keeps it in, which is
/// named like `card1-DP-2`.
fn drm_model(drm: &Path, output: &str) -> Option<String> {
    let suffix = format!("-{output}");
    std::fs::read_dir(drm).ok()?.flatten().find_map(|entry| {
        let folder = entry.file_name().to_string_lossy().into_owned();
        (folder.starts_with("card") && folder.ends_with(&suffix))
            .then(|| std::fs::read(entry.path().join("edid")).ok())
            .flatten()
            .and_then(|edid| edid_model(&edid))
    })
}

/// The monitor name descriptor of an EDID block, if it has one.
fn edid_model(edid: &[u8]) -> Option<String> {
    const DESCRIPTORS: [usize; 4] = [54, 72, 90, 108];
    const NAME_TAG: u8 = 0xFC;
    DESCRIPTORS.iter().find_map(|&at| {
        let block = edid.get(at..at + 18)?;
        if block[..3] != [0, 0, 0] || block[3] != NAME_TAG {
            return None;
        }
        let text: String = block[5..].iter().take_while(|b| **b != 0x0A).map(|b| *b as char).collect();
        Some(text.trim().to_string()).filter(|name| !name.is_empty() && name.is_ascii())
    })
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

    /// What `kscreen-doctor -j` printed under a real KWin, with a second, richer output added.
    const KSCREEN_JSON: &str = r#"{"features":255,"outputs":[
        {"clones":[],"connected":true,"currentModeId":"3","enabled":true,"followPreferredMode":false,"icon":"","id":1,
         "modes":[{"id":"1","name":"2560x1440@144","refreshRate":143.912,"size":{"height":1440,"width":2560}},
                  {"id":"2","name":"1920x1080@60","refreshRate":60,"size":{"height":1080,"width":1920}},
                  {"id":"3","name":"2560x1440@360","refreshRate":359.8,"size":{"height":1440,"width":2560}}],
         "name":"DP-2","pos":{"x":1920,"y":0},"preferredModes":["1"],"priority":1,"replicationSource":0,"rotation":1,"scale":1,
         "size":{"height":1440,"width":2560},"sizeMM":{"height":-1,"width":-1},"type":0},
        {"connected":true,"currentModeId":"1","enabled":true,"id":2,
         "modes":[{"id":"1","name":"1920x1080@240","refreshRate":239.76,"size":{"height":1080,"width":1920}}],
         "name":"DP-3","pos":{"x":0,"y":360}},
        {"connected":false,"currentModeId":"","enabled":false,"id":3,"modes":[],"name":"HDMI-1","pos":{"x":0,"y":0}},
        {"connected":true,"currentModeId":"","enabled":false,"id":4,"modes":[],"name":"DP-4","pos":{"x":0,"y":0}}
    ],"screen":{"currentSize":{"height":1440,"width":4480},"id":0}}"#;

    #[test]
    fn kscreen_gives_each_enabled_monitor_its_modes_best_first() {
        let displays = parse_kscreen(KSCREEN_JSON);
        assert_eq!(displays.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["DP-3", "DP-2"]);
        let dp2 = &displays[1];
        assert!(dp2.can_change);
        assert_eq!((dp2.x, dp2.y), (1920, 0));
        let current = dp2.current.as_ref().unwrap();
        assert_eq!((current.width, current.millihertz, current.id.as_str()), (2560, 359_800, "3"));
        let rates: Vec<u32> = dp2.modes.iter().map(|m| m.millihertz).collect();
        assert_eq!(rates, [359_800, 143_912, 60_000]);
        assert_eq!(current.label(), "2560 \u{d7} 1440 at 360 Hz");
    }

    #[test]
    fn anything_but_kscreens_json_gives_no_displays() {
        assert!(parse_kscreen("").is_empty());
        assert!(parse_kscreen("kscreen.doctor: Invalid config.").is_empty());
        assert!(parse_kscreen(r#"{"outputs":"no"}"#).is_empty());
    }

    #[test]
    fn a_mode_is_set_by_kscreens_own_handle_for_it() {
        let mode = DisplayMode { width: 2560, height: 1440, millihertz: 143_912, id: "7".into() };
        assert_eq!(mode_argument("DP-2", &mode), "output.DP-2.mode.7");
    }

    #[test]
    fn xrandr_lists_modes_but_cannot_change_them() {
        let displays = parse_xrandr_displays(XRANDR);
        assert_eq!(displays.len(), 2);
        assert!(!displays[0].can_change);
        assert_eq!(displays[0].id, "DP-2");
        assert_eq!(displays[0].current.as_ref().unwrap().millihertz, 359_800);
        assert_eq!(displays[0].modes.len(), 2);
        assert_eq!(displays[1].modes[0].width, 1920);
        assert!(parse_xrandr_displays("").is_empty());
    }

    fn edid_naming(name: &[u8]) -> Vec<u8> {
        let mut edid = vec![0u8; 128];
        edid[72..77].copy_from_slice(&[0, 0, 0, 0xFC, 0]);
        edid[77..77 + name.len()].copy_from_slice(name);
        edid[77 + name.len()] = 0x0A;
        edid
    }

    #[test]
    fn a_monitor_says_its_model_in_its_edid() {
        assert_eq!(edid_model(&edid_naming(b"DELL U2720Q")).as_deref(), Some("DELL U2720Q"));
        assert_eq!(edid_model(&[0u8; 128]), None);
        assert_eq!(edid_model(&[1, 2, 3]), None, "too short");
        assert_eq!(edid_model(&edid_naming(b"   ")), None, "blank");
    }

    #[test]
    fn the_model_is_found_under_any_card() {
        let root = temp("drm");
        std::fs::create_dir_all(root.join("card1-DP-2")).unwrap();
        std::fs::create_dir_all(root.join("card1-DP-21")).unwrap();
        std::fs::write(root.join("card1-DP-2/edid"), edid_naming(b"Big one")).unwrap();
        assert_eq!(drm_model(&root, "DP-2").as_deref(), Some("Big one"));
        assert_eq!(drm_model(&root, "DP-1"), None);
        assert_eq!(drm_model(&root, "HDMI-1"), None);
        let _ = std::fs::remove_dir_all(root);
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
