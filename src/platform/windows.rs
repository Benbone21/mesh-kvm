//! The Windows laptop.

use super::Platform;
use crate::protocol::{MonitorRect, NodeId};
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

pub struct WindowsPlatform {
    pub size: Option<(i32, i32)>,
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

    fn autostart_enabled(&self) -> bool {
        startup_vbs_path().is_some_and(|p| p.exists())
    }

    fn set_autostart(&self, enabled: bool) {
        let marker = autostart_disabled_marker();
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

/// Present when the user turned auto-boot off in the app, so startup doesn't reinstall it.
fn autostart_disabled_marker() -> PathBuf {
    crate::settings::config_dir().join("autostart-disabled")
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
        !autostart_disabled_marker().exists(),
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
    fn displays_are_named_as_windows_names_them() {
        assert_eq!(display_name(r"\\.\DISPLAY1"), "Display 1");
        assert_eq!(display_name(r"\\.\DISPLAY12"), "Display 12");
        assert_eq!(display_name("WinDisc"), "Display");
    }

    #[test]
    fn every_monitor_found_has_a_real_size_and_a_name() {
        // Wine with no X server has no monitors, which the layout treats as a failed detection.
        let monitors = enum_monitors();
        assert!(monitors.iter().all(|m| m.w > 0 && m.h > 0 && m.name.starts_with("Display")));
    }
}
