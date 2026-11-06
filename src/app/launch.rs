//! Starting mesh-kvm's own pieces: the tray and the window run as separate processes of this exe.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Quotes `exe` for a VBScript string that is itself inside a `WshShell.Run` command line.
fn vbs_run(exe: &str, args: &str) -> String {
    format!("WshShell.Run \"\"\"{exe}\"\" {args}\", 0, False\r\n")
}

const VBS_HEAD: &str = "Set WshShell = CreateObject(\"WScript.Shell\")\r\n";

/// The script in the Startup folder: starts the service and the tray at login, both windowless.
/// The service reads the saved address, so that a change in the app is never undone at login.
/// `peer` is for an address that can't be saved yet.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn windows_vbs(exe: &str, peer: Option<&str>) -> String {
    let service = match peer {
        Some(peer) => format!("--role windows --peer {peer}"),
        None => "--role windows".to_string(),
    };
    format!("{VBS_HEAD}{}{}", vbs_run(exe, &service), vbs_run(exe, "--tray"))
}

/// The address in a login script an older version wrote, if it holds a usable one.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn peer_in_login_script(script: &str) -> Option<String> {
    let after = script.split_once("--peer ")?.1;
    let word = after.split(|c: char| c.is_whitespace() || c == '"').next()?;
    crate::settings::validate_peer(word).ok()
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
    peer: Option<&str>,
    autostart: bool,
    shortcut: &dyn Fn(&Path, &str) -> bool,
) -> Option<PathBuf> {
    let _ = std::fs::remove_file(programs_dir.join(OLD_START_MENU_SCRIPT));
    // The login script goes first: PowerShell takes a moment, and the app reads this file to
    // show the start-at-boot switch.
    let startup = programs_dir.join("Startup");
    let login = (autostart && startup.exists())
        .then(|| startup.join(LOGIN_SCRIPT))
        .and_then(|login| std::fs::write(&login, windows_vbs(exe, peer)).ok().map(|_| login));
    let script = programs_dir.join(START_MENU_SCRIPT);
    let lnk = programs_dir.join(START_MENU_SHORTCUT);
    if (lnk.exists() && shortcut_points_at(&lnk, exe)) || shortcut(&lnk, exe) {
        let _ = std::fs::remove_file(&script);
    } else {
        let _ = std::fs::write(&script, windows_app_vbs(exe));
    }
    login
}

/// True when the shortcut file holds `exe`, as text or as the wide characters Windows stores.
fn shortcut_points_at(lnk: &Path, exe: &str) -> bool {
    let Ok(bytes) = std::fs::read(lnk) else { return false };
    let wide: Vec<u8> = exe.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let has = |needle: &[u8]| !needle.is_empty() && bytes.windows(needle.len()).any(|w| w == needle);
    has(exe.as_bytes()) || has(&wide)
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
    let exe = running_exe().map_err(|e| format!("Couldn't find where mesh-kvm is installed: {e}"))?;
    let written = write_linux_launchers(Path::new(&home), &exe.to_string_lossy())
        .map_err(|e| format!("Couldn't write the launchers: {e}"))?;
    for path in written {
        println!("Wrote {}", path.display());
    }
    match refresh_menu(|tool| run_quietly(tool)) {
        Some(tool) => println!("Refreshed the app menu with {tool}"),
        None => println!("Log out and back in if mesh-kvm doesn't appear in your app menu."),
    }
    Ok(())
}

/// KDE reads its app menu from a cache, so a new entry stays hidden until the cache is rebuilt.
#[cfg(any(target_os = "linux", test))]
fn refresh_menu(run: impl Fn(&str) -> bool) -> Option<&'static str> {
    ["kbuildsycoca6", "kbuildsycoca5"].into_iter().find(|tool| run(tool))
}

#[cfg(target_os = "linux")]
fn run_quietly(tool: &str) -> bool {
    std::process::Command::new(tool)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Linux reports `current_exe` with " (deleted)" on the end once a rebuild has replaced the file.
fn without_deleted_suffix(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_suffix(" (deleted)")) {
        Some(clean) => PathBuf::from(clean),
        None => path,
    }
}

fn running_exe() -> std::io::Result<PathBuf> {
    std::env::current_exe().map(without_deleted_suffix)
}

/// Starts this exe again with `arg` (such as `--app` or `--tray`) and does not wait for it.
pub fn spawn_self(arg: &str) -> std::io::Result<()> {
    spawn_with(&[arg], &[])
}

/// Starts this exe again with `args` and does not wait for it. It outlives this process.
fn spawn_with(args: &[&str], envs: &[(&str, &str)]) -> std::io::Result<()> {
    let exe = running_exe()?;
    let mut command = Command::new(exe);
    command.args(args).envs(envs.iter().copied()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    // Collect the child's exit status when it ends, so it doesn't linger as a zombie.
    let _ = std::thread::Builder::new().name("mesh-kvm-reap".into()).spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Set on a Windows service started by the one it replaces, which still holds the ports.
pub const RESTARTING_ENV: &str = "MESH_KVM_RESTARTING";

/// The arguments a restarted service runs with. No arguments on Windows would open the window.
fn restart_args(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return vec!["--role".to_string(), crate::platform::ROLE.to_string()];
    }
    args
}

/// Replaces this service with a new one, which reads the settings again. Only returns on failure.
pub fn restart_service() -> String {
    let args = restart_args(std::env::args().skip(1).collect());
    #[cfg(unix)]
    {
        // This keeps the process id, so systemd sees nothing happen, and a detached service stays
        // running with nobody to start it again. Every socket here closes on exec.
        use std::os::unix::process::CommandExt;
        match running_exe() {
            Ok(exe) => Command::new(exe).args(&args).exec().to_string(),
            Err(e) => e.to_string(),
        }
    }
    #[cfg(not(unix))]
    {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match spawn_with(&args, &[(RESTARTING_ENV, "1")]) {
            Ok(()) => std::process::exit(0),
            Err(e) => e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_restart_keeps_the_arguments_and_never_turns_into_a_bare_launch() {
        let kept = vec!["--role".to_string(), "windows".to_string(), "--port".to_string(), "9".to_string()];
        assert_eq!(restart_args(kept.clone()), kept);
        assert_eq!(restart_args(vec![]), vec!["--role".to_string(), crate::platform::ROLE.to_string()]);
    }

    #[test]
    fn menu_refresh_prefers_the_current_kde_tool() {
        assert_eq!(refresh_menu(|_| true), Some("kbuildsycoca6"));
    }

    #[test]
    fn menu_refresh_falls_back_to_the_older_tool() {
        assert_eq!(refresh_menu(|tool| tool == "kbuildsycoca5"), Some("kbuildsycoca5"));
    }

    #[test]
    fn menu_refresh_is_optional_off_kde() {
        assert_eq!(refresh_menu(|_| false), None);
    }

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
        let vbs = windows_vbs(EXE, None);
        let runs: Vec<&str> = vbs.lines().filter(|l| l.contains("WshShell.Run")).collect();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().any(|l| l.contains("--role windows")));
        assert!(!vbs.contains("--peer"), "the saved address is used, so that an app change isn't undone");
        assert_eq!(runs.iter().filter(|l| l.contains("--tray")).count(), 1);
        assert!(runs.iter().all(|l| l.contains(EXE) && l.ends_with(", 0, False")));
    }

    #[test]
    fn login_script_lines_are_exact_for_a_path_with_spaces() {
        let exe = r"C:\Users\Mary Jane\AppData\Local\mesh-kvm\mesh-kvm.exe";
        let vbs = windows_vbs(exe, Some("192.168.8.210"));
        let want_service = "WshShell.Run \"\"\"C:\\Users\\Mary Jane\\AppData\\Local\\mesh-kvm\\mesh-kvm.exe\"\" --role windows --peer 192.168.8.210\", 0, False";
        let want_tray = "WshShell.Run \"\"\"C:\\Users\\Mary Jane\\AppData\\Local\\mesh-kvm\\mesh-kvm.exe\"\" --tray\", 0, False";
        let lines: Vec<&str> = vbs.lines().collect();
        assert_eq!(lines[0], "Set WshShell = CreateObject(\"WScript.Shell\")");
        assert_eq!(lines[1], want_service);
        assert_eq!(lines[2], want_tray);
    }

    #[test]
    fn the_address_comes_out_of_a_login_script_from_an_older_version() {
        let old = "Set WshShell = CreateObject(\"WScript.Shell\")\r\n\
                   WshShell.Run \"\"\"C:\\mesh-kvm.exe\"\" --role windows --peer 192.168.8.241 --switch-edge left\", 0, False\r\n";
        assert_eq!(peer_in_login_script(old).as_deref(), Some("192.168.8.241"));
        let at_end = windows_vbs(EXE, Some("192.168.8.241"));
        assert_eq!(peer_in_login_script(&at_end).as_deref(), Some("192.168.8.241"));
        assert_eq!(peer_in_login_script(&windows_vbs(EXE, None)), None);
        assert_eq!(peer_in_login_script("--peer nonsense"), None);
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
        install_windows_launchers(&programs, EXE, None, true, &|_, _| false);
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
        install_windows_launchers(&programs, EXE, None, true, &|lnk, exe| {
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
        install_windows_launchers(&programs, EXE, None, true, &|_, _| false);
        assert!(programs.join("mesh-kvm.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_replaced_exe_loses_the_deleted_suffix() {
        assert_eq!(
            without_deleted_suffix(PathBuf::from("/opt/mesh-kvm (deleted)")),
            PathBuf::from("/opt/mesh-kvm")
        );
        assert_eq!(without_deleted_suffix(PathBuf::from("/opt/mesh-kvm")), PathBuf::from("/opt/mesh-kvm"));
    }

    #[test]
    fn the_login_script_is_written_before_the_shortcut_is_made() {
        let dir = temp("win-order");
        let programs = programs(&dir);
        install_windows_launchers(&programs, EXE, None, true, &|_, _| {
            assert!(programs.join("Startup/mesh-kvm-autostart.vbs").exists());
            true
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    fn fake_lnk(path: &Path, exe: &str) {
        let wide: Vec<u8> = exe.encode_utf16().flat_map(u16::to_le_bytes).collect();
        std::fs::write(path, [b"L\0\0\0 junk ".as_slice(), &wide, b" more"].concat()).unwrap();
    }

    #[test]
    fn a_shortcut_that_already_points_at_the_exe_is_left_alone() {
        let dir = temp("win-skip");
        let programs = programs(&dir);
        fake_lnk(&programs.join("mesh-kvm.lnk"), EXE);
        std::fs::write(programs.join("mesh-kvm.vbs"), "stale").unwrap();
        install_windows_launchers(&programs, EXE, None, true, &|_, _| panic!("PowerShell ran"));
        assert!(!programs.join("mesh-kvm.vbs").exists());
        assert!(programs.join("mesh-kvm.lnk").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_shortcut_to_another_path_is_made_again() {
        let dir = temp("win-moved");
        let programs = programs(&dir);
        fake_lnk(&programs.join("mesh-kvm.lnk"), r"C:\old\mesh-kvm.exe");
        let made = std::cell::Cell::new(false);
        install_windows_launchers(&programs, EXE, None, true, &|_, _| {
            made.set(true);
            true
        });
        assert!(made.get());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_autostart_writes_no_startup_script() {
        let dir = temp("win-off");
        let programs = programs(&dir);
        install_windows_launchers(&programs, EXE, None, false, &|_, _| false);
        assert!(programs.join("mesh-kvm.vbs").exists());
        assert!(!programs.join("Startup/mesh-kvm-autostart.vbs").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
