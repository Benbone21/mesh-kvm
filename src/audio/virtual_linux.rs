//! The other device's mic and speakers, as devices of this one while the two are linked.
//!
//! The mic is a pipe source: what the other device's mic sends is written to a FIFO, and the source
//! records it, or silence when nothing comes. Only a microphone shows in the sound settings. The
//! speakers are a null sink: what apps play on it goes nowhere here, and the engine captures it and
//! sends it with the rest of the sound.
//!
//! Both are `pactl` modules, which the sound server keeps after the process that loaded them has
//! gone. So they are unloaded by the ids they were loaded with, and at start any left by a service
//! that was killed are unloaded by name.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

pub const MIC_NAME: &str = "mesh-kvm-peer-mic";
pub const SPEAKERS_NAME: &str = "mesh-kvm-peer-speakers";
/// What every module this loads has in its arguments.
const MARK: &str = "mesh-kvm-peer-";
/// How long a failed load is left before it's tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(5);
/// Loading a module can be slow while the sound server is busy. One that answers after this may
/// have loaded anyway, and the next start clears it.
const PACTL_DEADLINE: Duration = Duration::from_secs(5);

/// The FIFO the mic source reads from.
pub fn fifo_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    dir.join(format!("{MIC_NAME}.fifo"))
}

/// The other device's name, made safe to put in a module's arguments: nothing in it can end the
/// quoted description or start another argument.
pub fn clean_name(peer: &str) -> String {
    let kept: String = peer.chars().filter(|c| !matches!(c, '"' | '\'' | '\\' | '=') && !c.is_control()).collect();
    let kept = kept.trim();
    if kept.is_empty() { "Other device".to_string() } else { kept.to_string() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mic,
    Speakers,
}

/// The arguments to `pactl load-module` for one of the devices. `peer` is already clean.
pub fn module_args(kind: Kind, peer: &str) -> Vec<String> {
    match kind {
        Kind::Mic => vec![
            "module-pipe-source".to_string(),
            format!("source_name={MIC_NAME}"),
            format!("file={}", fifo_path().display()),
            "format=float32le".to_string(),
            "rate=48000".to_string(),
            "channels=2".to_string(),
            format!("source_properties=device.description=\"{peer} mic (mesh-kvm)\""),
        ],
        Kind::Speakers => vec![
            "module-null-sink".to_string(),
            format!("sink_name={SPEAKERS_NAME}"),
            format!("sink_properties=device.description=\"{peer} speakers (mesh-kvm)\""),
        ],
    }
}

/// The name the devices should go by, while they should exist at all: paired and linked.
pub fn wanted(paired: bool, online: bool, peer: Option<&str>) -> Option<String> {
    (paired && online).then(|| clean_name(peer.unwrap_or("")))
}

struct Loaded {
    name: String,
    ids: Vec<u32>,
}

/// The two devices, as this service loaded them.
#[derive(Default)]
pub struct VirtualDevices {
    loaded: Option<Loaded>,
    failed_at: Option<Instant>,
    error: Option<String>,
}

fn unload(ids: &[u32], run: &mut dyn FnMut(&[String]) -> Option<String>) {
    for id in ids {
        run(&["unload-module".to_string(), id.to_string()]);
    }
}

impl VirtualDevices {
    /// Brings the devices in line with `wanted`. `run` runs `pactl` with these arguments and gives
    /// back what it printed, or None if it failed.
    pub fn step(&mut self, wanted: Option<String>, run: &mut dyn FnMut(&[String]) -> Option<String>) {
        self.step_at(wanted, run, Instant::now());
    }

    fn step_at(&mut self, wanted: Option<String>, run: &mut dyn FnMut(&[String]) -> Option<String>, now: Instant) {
        if self.loaded.as_ref().is_some_and(|loaded| Some(&loaded.name) != wanted.as_ref()) {
            if let Some(loaded) = self.loaded.take() {
                unload(&loaded.ids, run);
            }
        }
        let Some(name) = wanted else {
            self.failed_at = None;
            return;
        };
        if self.loaded.is_some() || self.failed_at.is_some_and(|at| now.duration_since(at) < RETRY_AFTER) {
            return;
        }
        let mut ids = Vec::new();
        for kind in [Kind::Mic, Kind::Speakers] {
            let mut args = vec!["load-module".to_string()];
            args.extend(module_args(kind, &name));
            match run(&args).and_then(|out| out.trim().parse::<u32>().ok()) {
                Some(id) => ids.push(id),
                None => {
                    // Half a pair is no use, and would be left over the next time.
                    unload(&ids, run);
                    self.failed_at = Some(now);
                    self.error = Some(take_pactl_error().unwrap_or_else(|| "pactl couldn't add the devices".to_string()));
                    return;
                }
            }
        }
        self.loaded = Some(Loaded { name, ids });
        self.failed_at = None;
        self.error = None;
    }

    /// The name both devices go by, while both are loaded.
    pub fn present(&self) -> Option<&str> {
        self.loaded.as_ref().map(|loaded| loaded.name.as_str())
    }

    /// Why the latest load failed, until one works.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// Unloads every module a service before this one left behind: the sound server keeps them after a
/// crash or a kill.
pub fn clear_leftovers(run: &mut dyn FnMut(&[String]) -> Option<String>) {
    let Some(listing) = run(&["list".to_string(), "short".to_string(), "modules".to_string()]) else { return };
    let ids: Vec<u32> = listing
        .lines()
        .filter(|line| line.contains(MARK))
        .filter_map(|line| line.split('\t').next()?.trim().parse().ok())
        .collect();
    unload(&ids, run);
}

/// The first line of `pactl`'s stderr when it last failed, until it's taken.
static PACTL_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn take_pactl_error() -> Option<String> {
    PACTL_ERROR.lock().unwrap_or_else(PoisonError::into_inner).take()
}

fn first_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines().map(str::trim).find(|line| !line.is_empty()).map(String::from)
}

/// Runs `pactl`, giving back what it printed when it worked. Arguments go to it as they are, with
/// no shell between.
pub fn pactl(args: &[String]) -> Option<String> {
    let mut command = Command::new("pactl");
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            *PACTL_ERROR.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("Couldn't run pactl: {e}"));
            return None;
        }
    };
    let pid = child.id();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    let output = match finished.recv_timeout(PACTL_DEADLINE) {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            *PACTL_ERROR.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("pactl failed: {e}"));
            return None;
        }
        Err(_) => {
            // The thread still owns the child, and reaps it once it's gone.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            *PACTL_ERROR.lock().unwrap_or_else(PoisonError::into_inner) = Some("pactl didn't answer".to_string());
            return None;
        }
    };
    if output.status.success() {
        return Some(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    *PACTL_ERROR.lock().unwrap_or_else(PoisonError::into_inner) =
        Some(first_line(&output.stderr).unwrap_or_else(|| format!("pactl {}", output.status)));
    None
}

// ============================================================================================
// The service's own pair
// ============================================================================================

/// The service's devices. Only the engine and the shutdown touch them.
static DEVICES: Mutex<Option<VirtualDevices>> = Mutex::new(None);
/// What `present` and `last_error` give, kept apart from `DEVICES` so that a slow `pactl` holds up
/// nobody reading them.
static PRESENT: Mutex<Option<String>> = Mutex::new(None);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Runs one step of the service's devices, and publishes the result. After `shut_down` nothing more
/// is loaded.
fn step_service(wanted: Option<String>, stopping: bool) {
    let mut devices = DEVICES.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(state) = devices.as_mut() else { return };
    state.step(wanted, &mut pactl);
    *PRESENT.lock().unwrap_or_else(PoisonError::into_inner) = state.present().map(String::from);
    *LAST_ERROR.lock().unwrap_or_else(PoisonError::into_inner) = state.error().map(String::from);
    if stopping {
        *devices = None;
    }
}

/// At the engine's start: clears what a killed service left, and starts keeping the devices.
pub fn start() {
    let mut devices = DEVICES.lock().unwrap_or_else(PoisonError::into_inner);
    clear_leftovers(&mut pactl);
    *devices = Some(VirtualDevices::default());
}

/// On every look: the devices exist while paired and linked.
pub fn look(paired: bool, online: bool, peer: Option<&str>) {
    step_service(wanted(paired, online, peer), false);
}

/// At service stop: the devices go, and stay gone.
pub fn shut_down() {
    step_service(None, true);
}

/// The other device's clean name, while both devices are loaded.
pub fn present() -> Option<String> {
    PRESENT.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The first line of what `pactl` said when the latest load failed. Cleared when one works. For the
/// app's status, which shows why the devices aren't there.
#[allow(dead_code)]
pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn names_cannot_smuggle_arguments() {
        let args = module_args(Kind::Mic, &clean_name("Bob's \"PC\" a=b"));
        assert_eq!(args.last().unwrap(), "source_properties=device.description=\"Bobs PC ab mic (mesh-kvm)\"");
        assert_eq!(clean_name("  \u{7}  "), "Other device");
    }

    #[test]
    fn the_module_arguments_are_exactly_these() {
        let fifo = fifo_path().to_string_lossy().into_owned();
        assert!(fifo.ends_with("/mesh-kvm-peer-mic.fifo"));
        assert_eq!(
            module_args(Kind::Mic, "Laptop"),
            [
                "module-pipe-source".to_string(),
                "source_name=mesh-kvm-peer-mic".into(),
                format!("file={fifo}"),
                "format=float32le".into(),
                "rate=48000".into(),
                "channels=2".into(),
                "source_properties=device.description=\"Laptop mic (mesh-kvm)\"".into(),
            ]
        );
        assert_eq!(
            module_args(Kind::Speakers, "Laptop"),
            [
                "module-null-sink",
                "sink_name=mesh-kvm-peer-speakers",
                "sink_properties=device.description=\"Laptop speakers (mesh-kvm)\"",
            ]
        );
        assert_eq!(clean_name("  Café\\PC\n "), "CaféPC", "non-ASCII stays, the rest goes");
    }

    #[test]
    fn devices_exist_only_while_paired_and_linked() {
        assert_eq!(wanted(true, true, Some("SACBOOKPRO")).as_deref(), Some("SACBOOKPRO"));
        assert_eq!(wanted(true, false, Some("SACBOOKPRO")), None);
        assert_eq!(wanted(false, true, None), None);
        assert_eq!(wanted(true, true, Some("a=\"b\"")).as_deref(), Some("ab"), "the name is cleaned");
    }

    /// A stand-in for `pactl`, keeping the modules it has loaded.
    #[derive(Default)]
    struct FakePactl {
        next_id: u32,
        loaded: BTreeMap<u32, String>,
        ever_loaded: Vec<u32>,
        unloaded: Vec<u32>,
        loads: usize,
        /// Loads of these modules fail.
        failing: Vec<&'static str>,
        listing: String,
    }

    impl FakePactl {
        fn run(&mut self, args: &[String]) -> Option<String> {
            match args.first().map(String::as_str) {
                Some("load-module") => {
                    self.loads += 1;
                    if self.failing.contains(&args[1].as_str()) {
                        return None;
                    }
                    self.next_id += 1;
                    self.loaded.insert(self.next_id, args[1].clone());
                    self.ever_loaded.push(self.next_id);
                    Some(format!("{}\n", self.next_id))
                }
                Some("unload-module") => {
                    let id: u32 = args[1].parse().unwrap();
                    self.loaded.remove(&id);
                    self.unloaded.push(id);
                    Some(String::new())
                }
                Some("list") => Some(self.listing.clone()),
                _ => None,
            }
        }
    }

    #[test]
    fn a_flapping_link_never_leaves_two_of_anything() {
        let mut fake = FakePactl::default();
        let mut devices = VirtualDevices::default();
        let name = |n: &str| Some(n.to_string());
        for wanted in [name("Laptop"), None, name("Laptop"), None, name("Laptop"), name("Renamed"), None] {
            let expect = wanted.clone();
            devices.step(wanted, &mut |args| fake.run(args));
            assert!(fake.loaded.len() <= 2, "{:?}", fake.loaded);
            assert_eq!(devices.present(), expect.as_deref());
            if expect.is_some() {
                let kinds: Vec<&str> = fake.loaded.values().map(String::as_str).collect();
                assert_eq!(kinds, ["module-pipe-source", "module-null-sink"]);
            }
        }
        assert!(fake.loaded.is_empty());
        let mut unloaded = fake.unloaded.clone();
        unloaded.sort();
        assert_eq!(unloaded, fake.ever_loaded, "every id loaded was unloaded, once");
        assert_eq!(fake.ever_loaded.len(), 8, "loaded four times, a rename included");
    }

    #[test]
    fn the_same_name_again_loads_nothing_more() {
        let mut fake = FakePactl::default();
        let mut devices = VirtualDevices::default();
        for _ in 0..3 {
            devices.step(Some("Laptop".into()), &mut |args| fake.run(args));
        }
        assert_eq!(fake.loads, 2);
    }

    #[test]
    fn a_failed_load_leaves_nothing_and_waits_before_trying_again() {
        let mut fake = FakePactl { failing: vec!["module-null-sink"], ..Default::default() };
        let mut devices = VirtualDevices::default();
        let t = Instant::now();
        devices.step_at(Some("Laptop".into()), &mut |args| fake.run(args), t);
        assert!(fake.loaded.is_empty(), "the mic that did load is taken away again");
        assert_eq!(devices.present(), None);
        assert!(devices.error().is_some());
        assert_eq!(fake.loads, 2);
        devices.step_at(Some("Laptop".into()), &mut |args| fake.run(args), t + RETRY_AFTER - Duration::from_millis(1));
        assert_eq!(fake.loads, 2, "not yet");
        fake.failing.clear();
        devices.step_at(Some("Laptop".into()), &mut |args| fake.run(args), t + RETRY_AFTER);
        assert_eq!(fake.loaded.len(), 2);
        assert_eq!(devices.present(), Some("Laptop"));
        assert_eq!(devices.error(), None, "cleared on success");
    }

    #[test]
    fn leftovers_from_a_crash_are_cleared() {
        let mut fake = FakePactl {
            listing: [
                "536870913\tmodule-always-sink\t\t",
                "41\tmodule-pipe-source\tsource_name=mesh-kvm-peer-mic file=/run/user/1000/mesh-kvm-peer-mic.fifo format=float32le\t",
                "7\tmodule-null-sink\tsink_name=someone-else\t",
                "42\tmodule-null-sink\tsink_name=mesh-kvm-peer-speakers sink_properties=device.description=\"x\"\t",
            ]
            .join("\n"),
            ..Default::default()
        };
        clear_leftovers(&mut |args| fake.run(args));
        assert_eq!(fake.unloaded, [41, 42]);
    }

    #[test]
    fn the_first_line_of_an_error_is_kept() {
        assert_eq!(first_line(b"Failure: Module initialization failed\nmore\n"), Some("Failure: Module initialization failed".into()));
        assert_eq!(first_line(b"\n"), None);
    }
}
