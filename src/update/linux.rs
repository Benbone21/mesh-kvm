//! Installing on Debian: the root helper does the work, through `pkexec`, and this reports how it
//! went and restarts the service. Debian has no automatic way back, so a failure says how.

use super::store::Ready;
use super::trust::Version;
use super::Installer;
use crate::app::launch::{restart_service, running_exe};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const INSTALLED_EXE: &str = "/usr/bin/mesh-kvm";
const HELPER: &str = "/usr/lib/mesh-kvm/apply-update";
const PKEXEC: &str = "/usr/bin/pkexec";
const UNIT: &str = "mesh-kvm.service";
/// How long systemd gets to stop this service before it starts itself again instead.
const RESTART_WAIT: Duration = Duration::from_secs(10);
const LONGEST_NOTE: usize = 400;

pub struct Deb;

impl Installer for Deb {
    fn can_install(&self) -> bool {
        running_exe().is_ok_and(|exe| exe == Path::new(INSTALLED_EXE)) && Path::new(HELPER).exists()
    }

    fn file_name(&self, version: Version) -> String {
        format!("mesh-kvm_{version}_amd64.deb")
    }

    fn install(&self, ready: &Ready) -> Result<(), String> {
        let output = Command::new(PKEXEC)
            .arg(HELPER)
            .arg(ready.version.to_string())
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("The update couldn't start: {e}"))?;
        if !output.status.success() {
            return Err(failure_note(&output));
        }
        crate::mute::shutdown();
        restart();
    }
}

/// Starts the new version: through systemd if that's what runs this service, or else by itself.
fn restart() -> ! {
    let under_systemd = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", UNIT])
        .status()
        .is_ok_and(|status| status.success());
    if under_systemd {
        let _ = Command::new("systemctl").args(["--user", "--no-block", "restart", UNIT]).status();
        std::thread::sleep(RESTART_WAIT);
    }
    let problem = restart_service();
    crate::settings::log_fatal(&format!("mesh-kvm couldn't start itself after updating: {problem}"));
    std::process::exit(1);
}

fn failure_note(output: &Output) -> String {
    let said = String::from_utf8_lossy(&output.stdout);
    note_for(output.status.code(), &said)
}

/// `apt: ` lines hold apt's last word, `back: ` the way back by hand, and `refused: ` why the
/// helper wouldn't go on. pkexec's own failures come back as 126 and 127.
fn note_for(code: Option<i32>, said: &str) -> String {
    let line = |prefix: &str| said.lines().find_map(|l| l.strip_prefix(prefix)).map(readable);
    if let Some(reason) = line("refused: ") {
        return format!("The update was refused: {reason}");
    }
    if let Some(apt) = line("apt: ") {
        return match line("back: ") {
            Some(back) => format!("The update failed, and the old version keeps running. {apt} To go back by hand, run: {back}"),
            None => format!("The update failed, and the old version keeps running. {apt}"),
        };
    }
    match code {
        Some(126 | 127) => "This device didn't allow the update to run. The old version keeps running.".to_string(),
        _ => "The update stopped for an unknown reason. The old version keeps running.".to_string(),
    }
}

/// What a program printed, safe to put on a banner.
fn readable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(LONGEST_NOTE).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apt_failing_gives_its_message_and_the_way_back() {
        let note = note_for(Some(3), "apt: E: Sub-process /usr/bin/dpkg returned an error code (1)\nback: sudo apt-get install /var/cache/mesh-kvm/kept/mesh-kvm_0.7.0_amd64.deb\n");
        assert!(note.contains("old version keeps running"), "{note}");
        assert!(note.contains("E: Sub-process /usr/bin/dpkg"), "{note}");
        assert!(note.contains("sudo apt-get install /var/cache/mesh-kvm/kept/mesh-kvm_0.7.0_amd64.deb"), "{note}");
    }

    #[test]
    fn with_no_kept_package_there_is_no_way_back_to_give() {
        let note = note_for(Some(3), "apt: E: No.\n");
        assert!(note.contains("E: No.") && !note.contains("by hand"), "{note}");
    }

    #[test]
    fn a_refusal_says_why() {
        assert!(note_for(Some(2), "refused: It isn't newer.\n").contains("It isn't newer."));
    }

    #[test]
    fn pkexec_saying_no_is_explained_in_plain_words() {
        assert!(note_for(Some(126), "").contains("didn't allow"));
        assert!(note_for(Some(127), "").contains("didn't allow"));
        assert!(note_for(None, "").contains("unknown reason"));
    }

    #[test]
    fn what_a_program_printed_can_not_draw_on_the_banner() {
        let note = note_for(Some(3), &format!("apt: E: \u{1b}[31mred\u{7}{}\n", "x".repeat(1000)));
        assert!(!note.contains('\u{1b}') && !note.contains('\u{7}'));
        assert!(note.len() < 600, "{}", note.len());
    }

    #[test]
    fn the_file_is_the_one_a_release_names() {
        assert_eq!(Deb.file_name(Version::parse("0.7.0").unwrap()), "mesh-kvm_0.7.0_amd64.deb");
    }
}
