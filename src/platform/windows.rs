//! The Windows laptop.

use super::{DisplayInfo, DisplayMode, Platform};
use crate::protocol::{MonitorRect, NodeId};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows_sys::Win32::Foundation::{BOOL, ERROR_SUCCESS, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    CDS_TEST, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISP_CHANGE_BADMODE,
    DISP_CHANGE_RESTART, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_INTERLACED, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;

pub struct WindowsPlatform {
    size: Option<(i32, i32)>,
}

impl WindowsPlatform {
    /// Only the service makes one of these. Without DPI awareness, Windows scales every monitor
    /// rectangle, so it comes before the first look at them. The app window leaves this to
    /// eframe, which asks for better than this and would be refused after it.
    pub fn new(size: Option<(i32, i32)>) -> Self {
        unsafe {
            SetProcessDPIAware();
        }
        Self { size }
    }
}

impl Platform for WindowsPlatform {
    fn node(&self) -> NodeId {
        NodeId::Windows
    }

    fn label(&self) -> &'static str {
        "Windows Laptop"
    }

    fn os_name(&self) -> &'static str {
        "Windows 11"
    }

    fn monitors(&self) -> Vec<MonitorRect> {
        match self.size {
            Some(size) => super::single_monitor(size),
            None => enum_monitors(),
        }
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        match self.size {
            Some(size) => super::single_display(size),
            None => enum_displays(),
        }
    }

    fn set_display_mode(&self, output: &str, mode: &DisplayMode) -> Result<(), String> {
        let device = attached_devices().into_iter().find(|device| display_name(device) == output);
        let device = device.ok_or_else(|| "That screen isn't connected any more.".to_string())?;
        change_mode(&device, mode)
    }

    fn autostart_enabled(&self) -> bool {
        startup_vbs_path().is_some_and(|p| p.exists())
    }

    fn set_autostart(&self, enabled: bool) {
        let marker = crate::settings::autostart_disabled_marker();
        if enabled {
            let _ = fs::remove_file(&marker);
            ensure_autorun_installed();
        } else {
            let _ = fs::create_dir_all(crate::settings::config_dir());
            let _ = fs::write(&marker, "");
            if let Some(vbs) = startup_vbs_path() {
                let _ = fs::remove_file(vbs);
            }
        }
    }

    fn input_devices(&self) -> &'static [&'static str] {
        &["Laptop keyboard", "Laptop trackpad"]
    }

    fn can_pick_audio_source(&self) -> bool {
        false
    }
}

/// What the person sees in Windows' display settings: `\\.\DISPLAY2` is "Display 2".
fn display_name(device: &str) -> String {
    match device.rsplit_once("DISPLAY") {
        Some((_, number)) if !number.is_empty() => format!("Display {number}"),
        _ => "Display".to_string(),
    }
}

unsafe extern "system" fn add_monitor(monitor: HMONITOR, _dc: HDC, _clip: *mut RECT, found: LPARAM) -> BOOL {
    let found = &mut *(found as *mut Vec<MonitorRect>);
    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) != 0 {
        let device = String::from_utf16_lossy(&info.szDevice);
        let r = info.monitorInfo.rcMonitor;
        found.push(MonitorRect {
            name: display_name(device.trim_end_matches('\0')),
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        });
    }
    TRUE
}

/// Every monitor, at its place on the virtual desktop. The main one is at 0,0, and one to its left
/// or above it has negative coordinates.
fn enum_monitors() -> Vec<MonitorRect> {
    let mut found: Vec<MonitorRect> = Vec::new();
    unsafe {
        EnumDisplayMonitors(ptr::null_mut(), ptr::null(), Some(add_monitor), &mut found as *mut _ as LPARAM);
    }
    found.sort_by_key(|m| (m.x, m.y));
    found
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(text: &[u16]) -> String {
    let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

fn empty_devmode() -> DEVMODEW {
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    mode
}

/// The names of the display devices that are part of the desktop, like `\\.\DISPLAY1`.
fn attached_devices() -> Vec<String> {
    let mut found = Vec::new();
    for index in 0.. {
        let mut device: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        if unsafe { EnumDisplayDevicesW(ptr::null(), index, &mut device, 0) } == 0 {
            break;
        }
        if device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0 {
            found.push(from_wide(&device.DeviceName));
        }
    }
    found
}

/// A mode as Windows lists it, unless it is interlaced or its colour depth isn't the one in use.
fn listed_mode(mode: &DEVMODEW, bits_in_use: u32) -> Option<DisplayMode> {
    let interlaced = unsafe { mode.Anonymous2.dmDisplayFlags } & DM_INTERLACED != 0;
    (!interlaced && mode.dmBitsPerPel == bits_in_use && mode.dmDisplayFrequency > 1).then(|| DisplayMode {
        width: mode.dmPelsWidth as i32,
        height: mode.dmPelsHeight as i32,
        millihertz: mode.dmDisplayFrequency * 1000,
        id: String::new(),
    })
}

fn describe(device: &str, names: &HashMap<String, String>) -> Option<DisplayInfo> {
    let name = wide(device);
    let mut current = empty_devmode();
    if unsafe { EnumDisplaySettingsW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut current) } == 0 {
        return None;
    }
    let mut modes = Vec::new();
    for index in 0.. {
        let mut mode = empty_devmode();
        if unsafe { EnumDisplaySettingsW(name.as_ptr(), index, &mut mode) } == 0 {
            break;
        }
        modes.extend(listed_mode(&mode, current.dmBitsPerPel));
    }
    super::sort_modes(&mut modes);
    let position = unsafe { current.Anonymous1.Anonymous2.dmPosition };
    let id = display_name(device);
    Some(DisplayInfo {
        name: names.get(device).cloned().unwrap_or_else(|| id.clone()),
        id,
        x: position.x,
        y: position.y,
        current: listed_mode(&current, current.dmBitsPerPel),
        modes,
        can_change: true,
    })
}

fn enum_displays() -> Vec<DisplayInfo> {
    let names = monitor_names();
    let mut found: Vec<DisplayInfo> = attached_devices().iter().filter_map(|device| describe(device, &names)).collect();
    found.sort_by_key(|d| (d.x, d.y));
    found
}

/// What a device's mode looks like to `ChangeDisplaySettingsExW`: only size and rate are set.
fn mode_request(mode: &DisplayMode) -> DEVMODEW {
    let mut request = empty_devmode();
    request.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    request.dmPelsWidth = mode.width as u32;
    request.dmPelsHeight = mode.height as u32;
    request.dmDisplayFrequency = mode.whole_hertz();
    request
}

/// Tries the mode first, as a mode the driver refuses should change nothing. It goes in the
/// registry so that a kept mode lasts, and going back writes the old one over it.
fn change_mode(device: &str, mode: &DisplayMode) -> Result<(), String> {
    let name = wide(device);
    let request = mode_request(mode);
    let change = |flags| unsafe { ChangeDisplaySettingsExW(name.as_ptr(), &request, ptr::null_mut(), flags, ptr::null()) };
    match change(CDS_TEST) {
        DISP_CHANGE_SUCCESSFUL => {}
        DISP_CHANGE_BADMODE => return Err("Windows says this screen can't use that mode.".to_string()),
        _ => return Err("Windows didn't accept that mode.".to_string()),
    }
    match change(CDS_UPDATEREGISTRY) {
        DISP_CHANGE_SUCCESSFUL => Ok(()),
        DISP_CHANGE_RESTART => Err("Windows needs to restart to use that mode.".to_string()),
        _ => Err("Windows didn't accept that mode.".to_string()),
    }
}

/// The model each display device shows, by device name. A laptop's own panel usually has none.
fn monitor_names() -> HashMap<String, String> {
    let mut names = HashMap::new();
    let (mut path_count, mut mode_count) = (0u32, 0u32);
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count) } != ERROR_SUCCESS {
        return names;
    }
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![unsafe { std::mem::zeroed() }; path_count as usize];
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![unsafe { std::mem::zeroed() }; mode_count as usize];
    let queried = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            ptr::null_mut(),
        )
    };
    if queried != ERROR_SUCCESS {
        return names;
    }
    for path in &paths[..path_count as usize] {
        let mut source: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { std::mem::zeroed() };
        source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        source.header.adapterId = path.sourceInfo.adapterId;
        source.header.id = path.sourceInfo.id;
        let mut target: DISPLAYCONFIG_TARGET_DEVICE_NAME = unsafe { std::mem::zeroed() };
        target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        target.header.adapterId = path.targetInfo.adapterId;
        target.header.id = path.targetInfo.id;
        let answered = unsafe {
            DisplayConfigGetDeviceInfo(&mut source.header) == ERROR_SUCCESS as i32
                && DisplayConfigGetDeviceInfo(&mut target.header) == ERROR_SUCCESS as i32
        };
        let model = from_wide(&target.monitorFriendlyDeviceName);
        if answered && !model.is_empty() {
            names.insert(from_wide(&source.viewGdiDeviceName), model);
        }
    }
    names
}

fn startup_vbs_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("mesh-kvm-autostart.vbs"),
    )
}

/// The address an older login script passed with `--peer`, for an install that has none saved.
pub fn old_login_script_peer() -> Option<String> {
    let script = fs::read_to_string(startup_vbs_path()?).ok()?;
    crate::app::launch::peer_in_login_script(&script)
}

/// Installs `mesh-kvm.exe` into `%LOCALAPPDATA%\mesh-kvm\mesh-kvm.exe` and creates a silent
/// windowless Startup launcher (service and tray) in `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\mesh-kvm-autostart.vbs`
/// so the Windows 11 laptop automatically starts `mesh-kvm` in the background on every boot/login.
pub fn ensure_autorun_installed() {
    // The shortcut comes from PowerShell, which takes a moment: keep it off the caller's thread.
    let _ = std::thread::Builder::new().name("mesh-kvm-install".into()).spawn(install_launchers);
}

/// Rewrites the Start Menu entry and the login script for the installed exe, without touching the
/// exe. A double-click over an old running service uses this, so the next login starts the tray.
/// That service may overwrite the settings file, which it reads none of the new fields from, so
/// the login script carries `peer` until a current service has started once and saved it.
pub fn rewrite_launchers(peer: Option<&str>) {
    if let Some(installed_exe) = crate::app::install::installed_exe() {
        write_launchers(&installed_exe, peer);
    }
}

fn install_launchers() {
    use crate::app::install;
    let Some(installed_exe) = install::installed_exe() else {
        return;
    };

    install::clear_stale(&installed_exe);
    if let Ok(cur_exe) = std::env::current_exe() {
        if install::needs_install(&cur_exe, &installed_exe) {
            if let Err(e) = install::replace_exe(&cur_exe, &installed_exe) {
                crate::settings::log_fatal(&e);
            }
        }
    }

    write_launchers(&installed_exe, None);
}

fn write_launchers(installed_exe: &Path, peer: Option<&str>) {
    let Ok(appdata) = std::env::var("APPDATA") else {
        return;
    };
    let exe_to_run = if installed_exe.exists() {
        installed_exe.to_path_buf()
    } else if let Ok(cur_exe) = std::env::current_exe() {
        cur_exe
    } else {
        return;
    };

    let programs_dir = PathBuf::from(&appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs");

    // When auto-boot is turned off in the app, no Startup script is written at all.
    let login_script = crate::app::launch::install_windows_launchers(
        &programs_dir,
        &exe_to_run.to_string_lossy(),
        peer,
        !crate::settings::autostart_disabled_marker().exists(),
        &crate::app::launch::make_shortcut,
    );
    if let Some(vbs_path) = login_script {
        println!(
            "[Windows] Auto-Boot installed: '{}' -> starts the service and tray silently at login.",
            vbs_path.to_string_lossy(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn making_the_platform_turns_on_dpi_awareness_before_any_monitor_is_read() {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsProcessDPIAware;
        let platform = crate::platform::current(None);
        assert!(unsafe { IsProcessDPIAware() } != 0);
        let _ = platform.monitors();
    }

    #[test]
    fn displays_are_named_as_windows_names_them() {
        assert_eq!(display_name(r"\\.\DISPLAY1"), "Display 1");
        assert_eq!(display_name(r"\\.\DISPLAY12"), "Display 12");
        assert_eq!(display_name("WinDisc"), "Display");
    }

    #[test]
    fn a_mode_change_asks_for_size_and_rate_and_nothing_else() {
        let mode = DisplayMode { width: 2560, height: 1600, millihertz: 165_000, id: String::new() };
        let request = mode_request(&mode);
        assert_eq!(request.dmFields, DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY);
        assert_eq!((request.dmPelsWidth, request.dmPelsHeight, request.dmDisplayFrequency), (2560, 1600, 165));
        assert_eq!(request.dmSize as usize, std::mem::size_of::<DEVMODEW>());
    }

    fn devmode(width: u32, bits: u32, hz: u32, flags: u32) -> DEVMODEW {
        let mut mode = empty_devmode();
        mode.dmPelsWidth = width;
        mode.dmPelsHeight = 1080;
        mode.dmBitsPerPel = bits;
        mode.dmDisplayFrequency = hz;
        mode.Anonymous2.dmDisplayFlags = flags;
        mode
    }

    #[test]
    fn only_progressive_modes_at_the_depth_in_use_are_offered() {
        assert_eq!(listed_mode(&devmode(1920, 32, 60, 0), 32).unwrap().millihertz, 60_000);
        assert!(listed_mode(&devmode(1920, 16, 60, 0), 32).is_none());
        assert!(listed_mode(&devmode(1920, 32, 60, DM_INTERLACED), 32).is_none());
        // Windows says 1 for "the hardware's default rate".
        assert!(listed_mode(&devmode(1920, 32, 1, 0), 32).is_none());
    }

    #[test]
    fn every_listed_display_can_be_found_again_by_its_id() {
        for display in enum_displays() {
            assert!(attached_devices().iter().any(|device| display_name(device) == display.id), "{}", display.id);
            assert!(display.current.is_some());
        }
    }

    #[test]
    fn a_screen_that_has_gone_says_so() {
        let platform = crate::platform::current(None);
        let mode = DisplayMode { width: 1920, height: 1080, millihertz: 60_000, id: String::new() };
        let error = platform.set_display_mode("Display 99", &mode).unwrap_err();
        assert!(error.contains("isn't connected"), "{error}");
    }

    #[test]
    fn every_monitor_found_has_a_real_size_and_a_name() {
        // Wine with no X server has no monitors, which the layout treats as a failed detection.
        let monitors = enum_monitors();
        assert!(monitors.iter().all(|m| m.w > 0 && m.h > 0 && m.name.starts_with("Display")));
    }
}
