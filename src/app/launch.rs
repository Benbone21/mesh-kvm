//! Starting mesh-kvm's own pieces: the tray and the window run as separate processes of this exe.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Quotes `exe` for a VBScript string that is itself inside a `WshShell.Run` command line.
fn vbs_run(exe: &str, args: &str) -> String {
    format!("WshShell.Run \"\"\"{exe}\"\" {args}\", 0, False\r\n")
}

const VBS_HEAD: &str = "Set WshShell = CreateObject(\"WScript.Shell\")\r\n";

/// The script in the Startup folder: starts the service and the tray at login, both windowless.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn windows_vbs(exe: &str, peer: &str, edge: &str) -> String {
    format!(
        "{VBS_HEAD}{}{}",
        vbs_run(exe, &format!("--role windows --peer {peer} --switch-edge {edge}")),
        vbs_run(exe, "--tray")
    )
}

/// The Start Menu script: opens the app window.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn windows_app_vbs(exe: &str) -> String {
    format!("{VBS_HEAD}{}", vbs_run(exe, "--app"))
}

const OLD_START_MENU_SCRIPT: &str = "mesh-kvm Studio.vbs";
const START_MENU_SCRIPT: &str = "mesh-kvm.vbs";
const START_MENU_SHORTCUT: &str = "mesh-kvm.lnk";
const LOGIN_SCRIPT: &str = "mesh-kvm-autostart.vbs";

/// A PowerShell string literal: single-quoted, with single quotes doubled.
fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The PowerShell that creates the Start Menu shortcut: opens the app, with the exe's own icon.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn shortcut_script(lnk: &str, exe: &str) -> String {
    let dir = Path::new(exe).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({}); \
         $s.TargetPath={}; $s.Arguments='--app'; $s.WorkingDirectory={}; \
         $s.IconLocation={}; $s.Save()",
        ps_quote(lnk),
        ps_quote(exe),
        ps_quote(&dir),
        ps_quote(&format!("{exe},0"))
    )
}

/// Creates `lnk` with PowerShell, with no console window. True when the shortcut exists after.
#[cfg(target_os = "windows")]
pub fn make_shortcut(lnk: &Path, exe: &str) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = shortcut_script(&lnk.to_string_lossy(), exe);
    let ran = Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(ran, Ok(status) if status.success()) && lnk.exists()
}

/// Writes the Start Menu entry (a shortcut, or the script if the shortcut can't be made), removes
/// the one older installs left, and (when `autostart` is on and the Startup folder exists) writes
/// the login script. Returns the login script's path if it was written.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn install_windows_launchers(
    programs_dir: &Path,
    exe: &str,
    peer: &str,
    edge: &str,
    autostart: bool,
    shortcut: &dyn Fn(&Path, &str) -> bool,
) -> Option<PathBuf> {
    let _ = std::fs::remove_file(programs_dir.join(OLD_START_MENU_SCRIPT));
    let script = programs_dir.join(START_MENU_SCRIPT);
    if shortcut(&programs_dir.join(START_MENU_SHORTCUT), exe) {
        let _ = std::fs::remove_file(&script);
    } else {
        let _ = std::fs::write(&script, windows_app_vbs(exe));
    }
    let startup = programs_dir.join("Startup");
    if !autostart || !startup.exists() {
        return None;
    }
    let login = startup.join(LOGIN_SCRIPT);
    std::fs::write(&login, windows_vbs(exe, peer, edge)).ok().map(|_| login)
}

/// Quotes `exe` as the Desktop Entry spec wants, when it needs it.
fn exec_path(exe: &str) -> String {
    let exe = exe.replace('%', "%%");
    if exe.chars().any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c)) {
        let escaped: String = exe
            .chars()
            .flat_map(|c| if "\"`$\\".contains(c) { vec!['\\', c] } else { vec![c] })
            .collect();
        format!("\"{escaped}\"")
    } else {
        exe
    }
}

/// A freedesktop launcher that runs `exe args`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn linux_desktop_entry(exe: &str, args: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=mesh-kvm\n\
         Comment=Share one mouse and keyboard between your computers\n\
         Exec={} {args}\n\
         Icon=mesh-kvm\n\
         Terminal=false\n\
         Categories=Utility;\n\
         StartupWMClass=mesh-kvm\n",
        exec_path(exe)
    )
}

/// Writes the menu entry, the tray's autostart entry, and the icon under `home`. Returns the
/// files written.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn write_linux_launchers(home: &Path, exe: &str) -> std::io::Result<Vec<PathBuf>> {
    let files = [
        (
            home.join(".local/share/applications/mesh-kvm.desktop"),
            linux_desktop_entry(exe, "--app").into_bytes(),
        ),
        (
            home.join(".config/autostart/mesh-kvm-tray.desktop"),
            linux_desktop_entry(exe, "--tray").into_bytes(),
        ),
        (home.join(".local/share/icons/hicolor/32x32/apps/mesh-kvm.png"), super::icon::png()),
    ];
    let mut written = Vec::new();
    for (path, bytes) in files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, bytes)?;
        written.push(path);
    }
    Ok(written)
}

/// `mesh-kvm --install-launchers`: writes the launchers for this exe and says what it wrote.
#[cfg(target_os = "linux")]
pub fn install_linux_launchers() -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME isn't set, so there is nowhere to write the launchers.".to_string())?;
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't find where mesh-kvm is installed: {e}"))?;
    let written = write_linux_launchers(Path::new(&home), &exe.to_string_lossy())
        .map_err(|e| format!("Couldn't write the launchers: {e}"))?;
    for path in written {
        println!("Wrote {}", path.display());
    }
    Ok(())
}

/// Starts this exe again with `arg` (such as `--app` or `--tray`) and does not wait for it.
pub fn spawn_self(arg: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Collect the child's exit status when it ends, so it doesn't linger as a zombie.
    let _ = std::thread::Builder::new().name("mesh-kvm-reap".into()).spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const EXE: &str = r"C:\Users\me\AppData\Local\mesh-kvm\mesh-kvm.exe";

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-launch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn login_script_starts_service_and_tray() {
        let vbs = windows_vbs(EXE, "192.168.8.210", "left");
        let runs: Vec<&str> = vbs.lines().filter(|l| l.contains("WshShell.Run")).collect();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().any(|l| l.contains("--role windows --peer 192.168.8.210 --switch-edge left")));
        assert_eq!(runs.iter().filter(|l| l.contains("--tray")).count(), 1);
        assert!(runs.iter().all(|l| l.contains(EXE) && l.ends_with(", 0, False")));
    }

    #[test]
    fn login_script_lines_are_exact_for_a_path_with_spaces() {
        let exe = r"C:\Users\Mary Jane\AppData\Local\mesh-kvm\mesh-kvm.exe";
        let vbs = windows_vbs(exe, "192.168.8.210", "left");
        let want_service = "WshShell.Run \"\"\"C:\\Users\\Mary Jane\\AppData\\Local\\mesh-kvm\\mesh-kvm.exe\"\" --role windows --peer 192.168.8.210 --switch-edge left\", 0, False";
        let want_tray = "WshShell.Run \"\"\"C:\\Users\\Mary Jane\\AppData\\Local\\mesh-kvm\\mesh-kvm.exe\"\" --tray\", 0, False";
        let lines: Vec<&str> = vbs.lines().collect();
        assert_eq!(lines[0], "Set WshShell = CreateObject(\"WScript.Shell\")");
        assert_eq!(lines[1], want_service);
        assert_eq!(lines[2], want_tray);
    }

    #[test]
    fn shortcut_script_quotes_single_quotes_in_paths() {
        let script = shortcut_script(r"C:\Users\O'Neil\Start Menu\mesh-kvm.lnk", r"C:\Users\O'Neil\mesh-kvm.exe");
        assert!(script.contains(r"CreateShortcut('C:\Users\O''Neil\Start Menu\mesh-kvm.lnk')"));
        assert!(script.contains(r"$s.TargetPath='C:\Users\O''Neil\mesh-kvm.exe'"));
        assert!(script.contains("$s.Arguments='--app'"));
        assert!(script.contains(r"$s.IconLocation='C:\Users\O''Neil\mesh-kvm.exe,0'"));
        assert!(script.ends_with("$s.Save()"));
    }

    #[test]
    fn percent_signs_in_the_exe_path_are_doubled_in_a_desktop_entry() {
        let entry = linux_desktop_entry("/opt/100%/mesh-kvm", "--app");
        assert!(entry.contains("Exec=/opt/100%%/mesh-kvm --app\n"));
    }

    #[test]
    fn start_menu_opens_the_app() {
        let vbs = windows_app_vbs(EXE);
        assert!(vbs.contains("--app"));
        assert!(!vbs.contains("--studio"));
        assert!(vbs.contains(EXE));
    }

    #[test]
    fn desktop_entry_is_valid() {
        let entry = linux_desktop_entry("/home/me/bin/mesh-kvm", "--app");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("\nType=Application\n"));
        assert!(entry.contains("\nExec=/home/me/bin/mesh-kvm --app\n"));
        assert!(entry.contains("\nIcon=mesh-kvm\n"));
    }

    #[test]
    fn desktop_entry_quotes_a_path_with_spaces() {
        let entry = linux_desktop_entry("/home/me/my apps/mesh-kvm", "--tray");
        assert!(entry.contains("\nExec=\"/home/me/my apps/mesh-kvm\" --tray\n"));
    }

    #[test]
    fn linux_launchers_are_written_under_home() {
        let home = temp("linux");
        let written = write_linux_launchers(&home, "/opt/mesh-kvm").unwrap();
        assert_eq!(written.len(), 3);
        let menu = std::fs::read_to_string(home.join(".local/share/applications/mesh-kvm.desktop")).unwrap();
        assert!(menu.contains("Exec=/opt/mesh-kvm --app"));
        let tray = std::fs::read_to_string(home.join(".config/autostart/mesh-kvm-tray.desktop")).unwrap();
        assert!(tray.contains("Exec=/opt/mesh-kvm --tray"));
        let png = std::fs::read(home.join(".local/share/icons/hicolor/32x32/apps/mesh-kvm.png")).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_dir_all(home);
    }

    fn programs(dir: &Path) -> std::path::PathBuf {
        let programs = dir.join("Programs");
        std::fs::create_dir_all(programs.join("Startup")).unwrap();
        programs
    }

    #[test]
    fn windows_launchers_replace_the_studio_script() {
        let dir = temp("win");
        let programs = programs(&dir);
        std::fs::write(programs.join("mesh-kvm Studio.vbs"), "old").unwrap();
        install_windows_launchers(&programs, EXE, "192.168.8.210", "left", true, &|_, _| false);
        assert!(!programs.join("mesh-kvm Studio.vbs").exists());
        assert!(std::fs::read_to_string(programs.join("mesh-kvm.vbs")).unwrap().contains("--app"));
        let login = std::fs::read_to_string(programs.join("Startup/mesh-kvm-autostart.vbs")).unwrap();
        assert!(login.contains("--tray"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_working_shortcut_replaces_the_script() {
        let dir = temp("win-lnk");
        let programs = programs(&dir);
        std::fs::write(programs.join("mesh-kvm.vbs"), "stale").unwrap();
        install_windows_launchers(&programs, EXE, "p", "left", true, &|lnk, exe| {
            assert_eq!(lnk, programs.join("mesh-kvm.lnk"));
            assert_eq!(exe, EXE);
            true
        });
        assert!(!programs.join("mesh-kvm.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_shortcut_falls_back_to_the_script() {
        let dir = temp("win-fallback");
        let programs = programs(&dir);
        install_windows_launchers(&programs, EXE, "p", "left", true, &|_, _| false);
        assert!(programs.join("mesh-kvm.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_autostart_writes_no_startup_script() {
        let dir = temp("win-off");
        let programs = programs(&dir);
        install_windows_launchers(&programs, EXE, "192.168.8.210", "left", false, &|_, _| false);
        assert!(programs.join("mesh-kvm.vbs").exists());
        assert!(!programs.join("Startup/mesh-kvm-autostart.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
