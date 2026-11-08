//! Quitting mesh-kvm on this device: the window, the tray and the service all stop. The tray's
//! Quit does this, and so does `--quit`, which the Windows installer runs before it replaces the
//! exe. The other device then only sees this one as offline.

use crate::app::single::{self, Word};
use std::time::{Duration, Instant};

/// The service's action.
pub const ACTION: &str = "quit";
const ONLY_FROM_HERE: &str = r#"{"ok":false,"error":"mesh-kvm can only be quit from this device."}"#;
/// Long enough for the reply to get out before the service goes.
const REPLY_FIRST: Duration = Duration::from_millis(300);
/// How long a stop by systemd or by its own signal gets before the service ends itself.
#[cfg(target_os = "linux")]
const STOP_WAIT: Duration = Duration::from_secs(5);
/// How long `everything_here` waits for the service to go.
const GONE_WITHIN: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const UNIT: &str = "mesh-kvm.service";

/// The service's answer to `quit`. Only the app or tray on this device may ask: the other device
/// can't switch this one off. Mutes are given back before the reply, then the service ends.
pub fn answer(from_app_here: bool) -> String {
    if !from_app_here {
        return ONLY_FROM_HERE.to_string();
    }
    crate::mute::shutdown();
    crate::settings::log_note("Quit on this device, so the service stopped.");
    let spawned = std::thread::Builder::new().name("mesh-kvm-quit".into()).spawn(|| {
        std::thread::sleep(REPLY_FIRST);
        stop();
    });
    if spawned.is_err() {
        stop();
    }
    crate::settings::OK_RESPONSE.to_string()
}

/// Ends the service. Under systemd the unit is stopped, or systemd would start it again; anywhere
/// else the service ends by itself, the same way it does when it's told to stop.
fn stop() -> ! {
    #[cfg(target_os = "windows")]
    crate::input::windows::let_go();
    #[cfg(target_os = "linux")]
    {
        let pid = std::process::id();
        let main_pid = std::process::Command::new("systemctl")
            .args(["--user", "show", "--property", "MainPID", "--value", UNIT])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let under_systemd = main_pid.is_ok_and(|out| is_main_pid(&String::from_utf8_lossy(&out.stdout), pid));
        if under_systemd {
            let _ = std::process::Command::new("systemctl").args(["--user", "--no-block", "stop", UNIT]).status();
        } else {
            // The input loop lets go of the keyboard and mouse on this and ends the service.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        std::thread::sleep(STOP_WAIT);
    }
    std::process::exit(0);
}

/// True when `systemctl show --property MainPID --value` names this process.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_main_pid(said: &str, pid: u32) -> bool {
    said.trim().parse::<u32>().is_ok_and(|main| main == pid)
}

/// How quitting went.
#[derive(Debug, PartialEq, Eq)]
pub enum Quit {
    /// The service said yes and has gone.
    Stopped,
    /// No service was answering.
    NothingRunning,
    /// The service is still there: an older one that doesn't know how, or one that won't go.
    StillRunning,
}

/// Closes the window and the tray on this device, and stops the service. The tray goes last, as
/// its own Quit runs this.
pub fn everything_here() -> Quit {
    single::signal(single::WINDOW_PORT, Word::Quit);
    let quit = stop_service();
    single::signal(single::TRAY_PORT, Word::Quit);
    quit
}

fn stop_service() -> Quit {
    let body = serde_json::json!({ "action": ACTION }).to_string();
    let asked = crate::webui::http_call("127.0.0.1", "POST", "/api/local/action", &body);
    let accepted = match &asked {
        Ok((200, reply)) => serde_json::from_str::<serde_json::Value>(reply).is_ok_and(|r| r["ok"] == true),
        Ok(_) => false,
        Err(_) => return Quit::NothingRunning,
    };
    if !accepted {
        return Quit::StillRunning;
    }
    let started = Instant::now();
    while started.elapsed() < GONE_WITHIN {
        if crate::webui::http_call("127.0.0.1", "GET", "/api/local", "").is_err() {
            return Quit::Stopped;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Quit::StillRunning
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_device_can_quit_its_service() {
        let reply: serde_json::Value = serde_json::from_str(&answer(false)).unwrap();
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("this device"), "{reply}");
    }

    #[test]
    fn the_unit_is_stopped_only_when_it_is_what_runs_this_service() {
        assert!(is_main_pid("4242\n", 4242));
        assert!(!is_main_pid("0\n", 4242), "the unit isn't running");
        assert!(!is_main_pid("4243\n", 4242), "the unit runs another one");
        assert!(!is_main_pid("", 4242), "there's no unit, or no systemd");
    }
}
