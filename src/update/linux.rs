//! Installing on Debian: the root helper does the work, through `pkexec`, and this reports how it
//! went and restarts the service. Debian has no automatic way back, so a failure says how.

use super::status::Preflight;
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
const PKCHECK: &str = "/usr/bin/pkcheck";
const ACTION: &str = "io.github.benbone21.mesh-kvm.apply-update";
const PREFLIGHT_LIMIT: Duration = Duration::from_secs(10);
/// Long enough for a Timeshift snapshot on a slow disk. It only limits how long this waits.
const INSTALL_LIMIT: Duration = Duration::from_secs(30 * 60);

pub struct Deb;

impl Installer for Deb {
    fn can_install(&self) -> bool {
        running_exe().is_ok_and(|exe| exe == Path::new(INSTALLED_EXE)) && Path::new(HELPER).exists()
    }

    fn file_name(&self, version: Version) -> String {
        format!("mesh-kvm_{version}_amd64.deb")
    }

    fn install(&self, ready: &Ready) -> Result<(), String> {
        let by_hand = by_hand_command(&ready.file);
        if let Some(note) = blocked_note(self.preflight(), &by_hand) {
            return Err(note);
        }
        let mut command = Command::new(PKEXEC);
        command.arg("--disable-internal-agent").arg(HELPER).arg(ready.version.to_string());
        let output = match run_limited(command, INSTALL_LIMIT) {
            Ran::Finished(output) => output,
            Ran::TimedOut => return Err(too_long_note()),
            Ran::CouldNotStart(why) => return Err(format!("The update couldn't start: {why} The old version keeps running. To update by hand, run: {by_hand}")),
        };
        if !output.status.success() {
            return Err(note_for(output.status.code(), &String::from_utf8_lossy(&output.stdout), &by_hand));
        }
        crate::mute::shutdown();
        restart();
    }

    fn preflight(&self) -> Preflight {
        let mut command = Command::new(PKCHECK);
        command.args(["--action-id", ACTION, "--process", &std::process::id().to_string()]);
        match run_limited(command, PREFLIGHT_LIMIT) {
            Ran::Finished(output) => read_pkcheck(output.status.code()),
            _ => Preflight::Unknown,
        }
    }

    fn by_hand(&self, ready: &Ready) -> Option<String> {
        Some(by_hand_command(&ready.file))
    }
}

/// `pkcheck` asks polkit the same question `pkexec` would, and installs nothing. With no
/// `--allow-user-interaction` it never prompts: 2 means it would need a password or an agent.
fn read_pkcheck(code: Option<i32>) -> Preflight {
    match code {
        Some(0) => Preflight::Allowed,
        Some(1) => Preflight::Denied,
        Some(2) => Preflight::NeedsPassword,
        _ => Preflight::Unknown,
    }
}

/// The note for a device that polkit says won't run the helper. Nothing is tried, as an update
/// that may be started from the other device shouldn't put up a password box here.
fn blocked_note(answer: Preflight, by_hand: &str) -> Option<String> {
    let why = match answer {
        Preflight::Denied => "This device doesn't allow mesh-kvm to install updates",
        Preflight::NeedsPassword => "This device wants a password before installing, and mesh-kvm can't ask for one",
        Preflight::Allowed | Preflight::Unknown => return None,
    };
    Some(format!("{why}, so the old version keeps running. To update by hand, run: {by_hand}"))
}

/// The command that does by hand what the helper would have.
fn by_hand_command(file: &Path) -> String {
    let path = file.to_string_lossy();
    if path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-~+".contains(c)) {
        return format!("sudo apt-get install {path}");
    }
    format!("sudo apt-get install '{}'", path.replace('\'', r"'\''"))
}

fn too_long_note() -> String {
    format!(
        "The update has taken over {} minutes, so mesh-kvm has stopped waiting. apt may still be running, and if it finishes the new version starts at the next restart.",
        INSTALL_LIMIT.as_secs() / 60
    )
}

enum Ran {
    Finished(Output),
    TimedOut,
    CouldNotStart(String),
}

/// Runs `command` for at most `limit`. A root helper can't be stopped from here and shouldn't be,
/// halfway through `apt`, so running out of time only ends the wait.
fn run_limited(mut command: Command, limit: Duration) -> Ran {
    let spawned = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let child = match spawned {
        Ok(child) => child,
        Err(e) => return Ran::CouldNotStart(e.to_string()),
    };
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    match finished.recv_timeout(limit) {
        Ok(Ok(output)) => Ran::Finished(output),
        Ok(Err(e)) => Ran::CouldNotStart(e.to_string()),
        Err(_) => Ran::TimedOut,
    }
}

/// Starts the new version: through systemd if that's what runs this service, or else by itself.
fn restart() -> ! {
    crate::linklog::stop("update", None);
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

/// `apt: ` lines hold apt's last word, `back: ` the way back by hand, and `refused: ` why the
/// helper wouldn't go on. pkexec's own failures come back as 126 and 127.
fn note_for(code: Option<i32>, said: &str, by_hand: &str) -> String {
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
        Some(126 | 127) => format!("This device didn't allow the update to run, so the old version keeps running. To update by hand, run: {by_hand}"),
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
        let note = note_for(Some(3), "apt: E: Sub-process /usr/bin/dpkg returned an error code (1)\nback: sudo apt-get install /var/cache/mesh-kvm/kept/mesh-kvm_0.7.0_amd64.deb\n", BY_HAND);
        assert!(note.contains("old version keeps running"), "{note}");
        assert!(note.contains("E: Sub-process /usr/bin/dpkg"), "{note}");
        assert!(note.contains("sudo apt-get install /var/cache/mesh-kvm/kept/mesh-kvm_0.7.0_amd64.deb"), "{note}");
    }

    #[test]
    fn with_no_kept_package_there_is_no_way_back_to_give() {
        let note = note_for(Some(3), "apt: E: No.\n", BY_HAND);
        assert!(note.contains("E: No.") && !note.contains("by hand"), "{note}");
    }

    const BY_HAND: &str = "sudo apt-get install /home/me/.config/mesh-kvm/updates/0.7.1/mesh-kvm_0.7.1_amd64.deb";

    #[test]
    fn pkexec_saying_no_gives_the_exact_command_and_says_the_old_version_stays() {
        for code in [126, 127] {
            let note = note_for(Some(code), "", BY_HAND);
            assert!(note.contains("didn't allow") && note.contains("old version keeps running"), "{note}");
            assert!(note.ends_with(BY_HAND), "{note}");
        }
    }

    #[test]
    fn the_preflight_answers_mean_what_pkcheck_says() {
        assert_eq!(read_pkcheck(Some(0)), Preflight::Allowed);
        assert_eq!(read_pkcheck(Some(1)), Preflight::Denied);
        assert_eq!(read_pkcheck(Some(2)), Preflight::NeedsPassword);
        assert_eq!(read_pkcheck(Some(127)), Preflight::Unknown, "polkit itself couldn't say");
        assert_eq!(read_pkcheck(None), Preflight::Unknown);
    }

    #[test]
    fn a_preflight_that_says_no_stops_before_pkexec_with_the_command() {
        for answer in [Preflight::Denied, Preflight::NeedsPassword] {
            let note = blocked_note(answer, BY_HAND).unwrap();
            assert!(note.contains("old version keeps running") && note.ends_with(BY_HAND), "{note}");
        }
        assert!(blocked_note(Preflight::NeedsPassword, BY_HAND).unwrap().contains("password"));
        assert_eq!(blocked_note(Preflight::Allowed, BY_HAND), None);
        assert_eq!(blocked_note(Preflight::Unknown, BY_HAND), None, "pkexec may still work");
    }

    #[test]
    fn a_path_with_a_space_is_quoted_in_the_command() {
        assert_eq!(by_hand_command(Path::new("/home/a b/x.deb")), "sudo apt-get install '/home/a b/x.deb'");
        assert_eq!(by_hand_command(Path::new("/home/a/x.deb")), "sudo apt-get install /home/a/x.deb");
        assert_eq!(by_hand_command(Path::new("/home/o'neil/x.deb")), r"sudo apt-get install '/home/o'\''neil/x.deb'");
    }

    #[test]
    fn a_helper_that_outlasts_the_limit_stops_the_wait_and_not_the_helper() {
        let started = std::time::Instant::now();
        let mut command = Command::new("sleep");
        command.arg("30");
        assert!(matches!(run_limited(command, Duration::from_millis(100)), Ran::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_helper_within_the_limit_gives_its_output() {
        let mut command = Command::new("echo");
        command.arg("done");
        let Ran::Finished(output) = run_limited(command, Duration::from_secs(5)) else { panic!("it finished") };
        assert_eq!(output.stdout, b"done\n");
    }

    #[test]
    fn running_out_of_time_says_apt_may_still_be_going() {
        let note = too_long_note();
        assert!(note.contains("may still be running"), "{note}");
    }

    #[test]
    fn the_polkit_files_name_this_program_and_this_action() {
        let rule = include_str!("../../packaging/deb/50-mesh-kvm-update.rules");
        let policy = include_str!("../../packaging/deb/io.github.benbone21.mesh-kvm.apply-update.policy");
        for file in [rule, policy] {
            assert!(file.contains(ACTION) && file.contains(HELPER), "{file}");
        }
    }

    #[test]
    fn a_refusal_says_why() {
        assert!(note_for(Some(2), "refused: It isn't newer.\n", BY_HAND).contains("It isn't newer."));
    }

    #[test]
    fn pkexec_saying_no_is_explained_in_plain_words() {
        assert!(note_for(Some(126), "", BY_HAND).contains("didn't allow"));
        assert!(note_for(Some(127), "", BY_HAND).contains("didn't allow"));
        assert!(note_for(None, "", BY_HAND).contains("unknown reason"));
    }

    #[test]
    fn what_a_program_printed_can_not_draw_on_the_banner() {
        let note = note_for(Some(3), &format!("apt: E: \u{1b}[31mred\u{7}{}\n", "x".repeat(1000)), BY_HAND);
        assert!(!note.contains('\u{1b}') && !note.contains('\u{7}'));
        assert!(note.len() < 600, "{}", note.len());
    }

    #[test]
    fn the_file_is_the_one_a_release_names() {
        assert_eq!(Deb.file_name(Version::parse("0.7.0").unwrap()), "mesh-kvm_0.7.0_amd64.deb");
    }
}
