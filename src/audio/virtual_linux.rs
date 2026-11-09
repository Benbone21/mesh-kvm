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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

pub const MIC_NAME: &str = "mesh-kvm-peer-mic";
pub const SPEAKERS_NAME: &str = "mesh-kvm-peer-speakers";
/// What every module this loads has in its arguments.
const MARK: &str = "mesh-kvm-peer-";
/// How long a failed load is left before it's tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(5);
/// Loading a module can be slow while the sound server is busy. One that answers after this may
/// have loaded anyway: the retry, or the unlink, clears it by name first.
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
            // A load given up on may have finished anyway, after its ids were lost.
            if self.failed_at.take().is_some() {
                clear_leftovers(run);
            }
            self.error = None;
            return;
        };
        if self.loaded.is_some() || self.failed_at.is_some_and(|at| now.duration_since(at) < RETRY_AFTER) {
            return;
        }
        if self.failed_at.is_some() {
            // The load that failed may have been only slow, and loaded after `pactl` was given up
            // on: it goes by name, so the retry doesn't make a second pair on the same FIFO.
            clear_leftovers(run);
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

    /// Squares what this keeps with the sound server's graph, which the engine reads on every look.
    /// A device missing from it is gone: the sound server restarted, or someone unloaded it. The ids
    /// kept may belong to other modules by now, so nothing is unloaded by them. What of ours is
    /// still there goes by name, in case a device was only slow to show, and the next step loads
    /// both again.
    pub fn notice_graph(&mut self, mic_there: bool, speakers_there: bool, run: &mut dyn FnMut(&[String]) -> Option<String>) {
        if self.loaded.is_none() || (mic_there && speakers_there) {
            return;
        }
        self.loaded = None;
        self.failed_at = None;
        clear_leftovers(run);
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

/// Set at service stop. Nothing is loaded after it, whatever the engine is in the middle of.
static STOPPED: AtomicBool = AtomicBool::new(false);
/// How long the stop waits for the devices to go. A stuck sound server mustn't hold up the exit:
/// what's left is cleared at the next start.
const SHUT_DOWN_WAIT: Duration = Duration::from_secs(2);

fn publish(state: Option<&VirtualDevices>) {
    *PRESENT.lock().unwrap_or_else(PoisonError::into_inner) = state.and_then(VirtualDevices::present).map(String::from);
    *LAST_ERROR.lock().unwrap_or_else(PoisonError::into_inner) = state.and_then(VirtualDevices::error).map(String::from);
}

/// Runs one step of the service's devices, and publishes the result.
fn step_service(wanted: Option<String>, graph: Option<(bool, bool)>) {
    let mut devices = DEVICES.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(state) = devices.as_mut() else { return };
    if let Some((mic_there, speakers_there)) = graph {
        state.notice_graph(mic_there, speakers_there, &mut pactl);
    }
    let stopped = STOPPED.load(Ordering::SeqCst);
    state.step(if stopped { None } else { wanted }, &mut pactl);
    publish(Some(state));
    if stopped {
        *devices = None;
    }
}

/// At the engine's start: clears what a killed service left, and starts keeping the devices.
pub fn start() {
    let mut devices = DEVICES.lock().unwrap_or_else(PoisonError::into_inner);
    if STOPPED.load(Ordering::SeqCst) {
        return;
    }
    clear_leftovers(&mut pactl);
    // Checked again, as a stop may have come while that ran.
    if !STOPPED.load(Ordering::SeqCst) {
        *devices = Some(VirtualDevices::default());
    }
}

/// On every look: the devices exist while paired and linked. `graph` says whether the mic and the
/// speakers are in the sound server's graph, when it could be read.
pub fn look(paired: bool, online: bool, peer: Option<&str>, graph: Option<(bool, bool)>) {
    step_service(wanted(paired, online, peer), graph);
}

/// Whether `work`, run on a thread of its own, is done within `limit`. If not, it carries on alone.
fn finishes_within(limit: Duration, work: impl FnOnce() + Send + 'static) -> bool {
    let (done, finished) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("mesh-kvm-audio-devices-stop".to_string()).spawn(move || {
        work();
        let _ = done.send(());
    });
    spawned.is_ok() && finished.recv_timeout(limit).is_ok()
}

/// At service stop: the devices go, and stay gone. It waits at most `SHUT_DOWN_WAIT`, which may be
/// spent waiting for the engine to finish a step: the step after a stop takes the devices away.
pub fn shut_down() {
    STOPPED.store(true, Ordering::SeqCst);
    *PRESENT.lock().unwrap_or_else(PoisonError::into_inner) = None;
    let finished = finishes_within(SHUT_DOWN_WAIT, || step_service(None, None));
    if !finished {
        crate::settings::log_note("The other device's mic and speakers were still going at stop; the next start clears them.");
    }
}

/// The other device's clean name, while both devices are loaded.
pub fn present() -> Option<String> {
    PRESENT.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The first line of what `pactl` said when the latest load failed. Cleared when one works. For the
/// app's status, which shows why the devices aren't there.
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
        /// Loads of these modules take too long: they're given up on, but load anyway.
        slow: Vec<&'static str>,
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
                    self.loaded.insert(self.next_id, args[1..].join(" "));
                    self.ever_loaded.push(self.next_id);
                    if self.slow.contains(&args[1].as_str()) {
                        return None;
                    }
                    Some(format!("{}\n", self.next_id))
                }
                Some("unload-module") => {
                    let id: u32 = args[1].parse().unwrap();
                    self.loaded.remove(&id);
                    self.unloaded.push(id);
                    Some(String::new())
                }
                Some("list") if !self.listing.is_empty() => Some(self.listing.clone()),
                Some("list") => Some(
                    self.loaded
                        .iter()
                        .map(|(id, args)| {
                            let (module, rest) = args.split_once(' ').unwrap_or((args, ""));
                            format!("{id}\t{module}\t{rest}\n")
                        })
                        .collect(),
                ),
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
                let kinds: Vec<&str> = fake.loaded.values().map(|args| args.split(' ').next().unwrap()).collect();
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
    fn a_load_given_up_on_that_loaded_anyway_is_not_doubled_by_the_retry() {
        let mut fake = FakePactl { slow: vec!["module-null-sink"], ..Default::default() };
        let mut devices = VirtualDevices::default();
        let t = Instant::now();
        devices.step_at(Some("Laptop".into()), &mut |args| fake.run(args), t);
        assert_eq!(devices.present(), None);
        assert_eq!(fake.loaded.len(), 1, "the speakers loaded after all; the mic was taken away");
        fake.slow.clear();
        devices.step_at(Some("Laptop".into()), &mut |args| fake.run(args), t + RETRY_AFTER);
        assert_eq!(devices.present(), Some("Laptop"));
        let kinds: Vec<&str> = fake.loaded.values().map(|args| args.split(' ').next().unwrap()).collect();
        assert_eq!(kinds, ["module-pipe-source", "module-null-sink"], "one of each, not two speakers");
    }

    #[test]
    fn unlinking_after_a_failure_forgets_the_error() {
        let mut fake = FakePactl { failing: vec!["module-null-sink"], ..Default::default() };
        let mut devices = VirtualDevices::default();
        devices.step(Some("Laptop".into()), &mut |args| fake.run(args));
        assert!(devices.error().is_some());
        devices.step(None, &mut |args| fake.run(args));
        assert_eq!(devices.error(), None, "the cell goes back to 'Appears when linked'");
        assert!(fake.loaded.is_empty());
    }

    #[test]
    fn unlinking_after_a_slow_load_takes_away_what_loaded_late() {
        let mut fake = FakePactl { slow: vec!["module-null-sink"], ..Default::default() };
        let mut devices = VirtualDevices::default();
        devices.step(Some("Laptop".into()), &mut |args| fake.run(args));
        assert_eq!(fake.loaded.len(), 1);
        devices.step(None, &mut |args| fake.run(args));
        assert!(fake.loaded.is_empty(), "{:?}", fake.loaded);
    }

    #[test]
    fn a_sound_server_restart_is_noticed_and_the_devices_come_back() {
        let mut fake = FakePactl::default();
        let mut devices = VirtualDevices::default();
        devices.step(Some("Laptop".into()), &mut |args| fake.run(args));
        // The sound server restarts: its modules are gone, and their ids may go to others.
        fake.loaded.clear();
        devices.notice_graph(false, false, &mut |args| fake.run(args));
        assert_eq!(devices.present(), None, "it doesn't claim what isn't there");
        assert!(fake.unloaded.is_empty(), "no stale id is unloaded");
        devices.step(Some("Laptop".into()), &mut |args| fake.run(args));
        assert_eq!(fake.loaded.len(), 2);
        assert_eq!(devices.present(), Some("Laptop"));
    }

    #[test]
    fn a_device_only_late_to_show_in_the_graph_is_never_doubled() {
        let mut fake = FakePactl::default();
        let mut devices = VirtualDevices::default();
        devices.step(Some("Laptop".into()), &mut |args| fake.run(args));
        devices.notice_graph(true, false, &mut |args| fake.run(args));
        devices.step(Some("Laptop".into()), &mut |args| fake.run(args));
        assert_eq!(fake.loaded.len(), 2, "{:?}", fake.loaded);
        assert_eq!(fake.unloaded, [1, 2]);
    }

    #[test]
    fn both_devices_in_the_graph_change_nothing() {
        let mut fake = FakePactl::default();
        let mut devices = VirtualDevices::default();
        devices.step(Some("Laptop".into()), &mut |args| fake.run(args));
        let mut calls = 0;
        devices.notice_graph(true, true, &mut |_| {
            calls += 1;
            None
        });
        assert_eq!(calls, 0);
        assert_eq!(devices.present(), Some("Laptop"));
    }

    #[test]
    fn a_stopped_service_never_loads_again() {
        shut_down();
        start();
        look(true, true, Some("Laptop"), Some((false, false)));
        assert_eq!(present(), None);
        assert!(DEVICES.lock().unwrap().is_none());
    }

    #[test]
    fn a_stuck_teardown_does_not_hold_up_the_exit() {
        let started = Instant::now();
        assert!(!finishes_within(Duration::from_millis(50), || std::thread::sleep(Duration::from_secs(5))));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(finishes_within(Duration::from_secs(5), || {}));
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
