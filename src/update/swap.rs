//! The Windows swap, and the watcher that undoes it. A new exe that won't run must not leave the
//! laptop without mesh-kvm, so a marker stays until the new copy proves itself, and a small
//! watcher running from the old exe puts it back if that doesn't happen in time.
//!
//! None of this needs Windows to run, so it's tested everywhere.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))] // Only the Windows build swaps.

use super::trust::Version;
use crate::app::install::{aside_path, replace_exe};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the new copy has to run before it counts as healthy.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(30);
/// How long after the swap it has to get there.
pub const RESTORE_AFTER: Duration = Duration::from_secs(60);
const LOOK_EVERY: Duration = Duration::from_millis(500);
const MARKER_NAME: &str = "update-pending.json";
const NOTE_NAME: &str = "update-note.txt";

/// Says a swap is waiting for the new copy to prove itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub from: String,
    pub to: String,
}

/// The files a swap uses, beside the settings and the installed exe.
#[derive(Debug, Clone)]
pub struct Paths {
    pub installed: PathBuf,
    pub folder: PathBuf,
}

impl Paths {
    fn marker(&self) -> PathBuf {
        self.folder.join(MARKER_NAME)
    }

    fn note(&self) -> PathBuf {
        self.folder.join(NOTE_NAME)
    }

    /// Where a new exe that failed goes, as it may still be running.
    fn failed(&self) -> PathBuf {
        let mut name = self.installed.as_os_str().to_owned();
        name.push(".failed");
        PathBuf::from(name)
    }

    pub fn read_marker(&self) -> Option<Marker> {
        serde_json::from_slice(&fs::read(self.marker()).ok()?).ok()
    }

    pub fn swap_pending(&self) -> bool {
        self.marker().exists()
    }

    /// The new copy has proved itself.
    pub fn clear_marker(&self) {
        let _ = fs::remove_file(self.marker());
    }

    pub fn leave_note(&self, note: &str) {
        let _ = fs::create_dir_all(&self.folder);
        let _ = fs::write(self.note(), note);
    }

    /// What a failed update left for the app, once.
    pub fn take_note(&self) -> Option<String> {
        let note = fs::read_to_string(self.note()).ok()?;
        let _ = fs::remove_file(self.note());
        Some(note)
    }

    /// A failed exe is left behind if it couldn't be stopped. Any later start clears it.
    pub fn clear_failed(&self) {
        let _ = fs::remove_file(self.failed());
    }
}

/// Puts `new_exe` in the installed place, with the old one kept beside it. The marker goes first,
/// so that a swap can never have happened without one.
pub fn swap_in(paths: &Paths, new_exe: &Path, marker: &Marker) -> Result<(), String> {
    let text = serde_json::to_vec(marker).map_err(|e| e.to_string())?;
    fs::create_dir_all(&paths.folder).map_err(|e| format!("Couldn't write the update's marker: {e}"))?;
    fs::write(paths.marker(), text).map_err(|e| format!("Couldn't write the update's marker: {e}"))?;
    replace_exe(new_exe, &paths.installed).inspect_err(|_| paths.clear_marker())
}

/// Puts the old exe back. The new one is moved rather than deleted, as it may be running.
pub fn put_old_back(paths: &Paths) -> Result<(), String> {
    let aside = aside_path(&paths.installed);
    if !aside.exists() {
        return Err("The old copy of mesh-kvm is gone, so it can't be put back.".to_string());
    }
    paths.clear_failed();
    let _ = fs::rename(&paths.installed, paths.failed());
    if paths.installed.exists() {
        let _ = fs::remove_file(&paths.installed);
    }
    fs::rename(&aside, &paths.installed).map_err(|e| format!("Couldn't put the old copy of mesh-kvm back: {e}"))
}

/// What the watcher does to the outside world.
pub trait WatchHost {
    fn start_new_service(&mut self);
    /// Stops the new service, if it's still going.
    fn stop_new_service(&mut self);
    /// Starts whatever is now in the installed place.
    fn start_restored_service(&mut self);
    fn wait(&self, time: Duration);
    fn elapsed(&self) -> Duration;
}

/// Starts the new copy and sees it through. Ends once it's healthy, or once it's undone.
pub fn watch(paths: &Paths, host: &mut dyn WatchHost) {
    let Some(marker) = paths.read_marker() else { return };
    host.start_new_service();
    while host.elapsed() < RESTORE_AFTER {
        if !paths.swap_pending() {
            return;
        }
        host.wait(LOOK_EVERY);
    }
    if !paths.swap_pending() {
        return;
    }
    host.stop_new_service();
    let note = match put_old_back(paths) {
        Ok(()) => format!("Version {} didn't start properly, so version {} was put back.", marker.to, marker.from),
        Err(why) => format!("Version {} didn't start properly. {why}", marker.to),
    };
    paths.leave_note(&note);
    paths.clear_marker();
    host.start_restored_service();
}

/// How a service that has just started should treat a marker.
#[derive(Debug, PartialEq, Eq)]
pub enum OnStart {
    Nothing,
    /// This is the new copy: it has to prove itself.
    ProveHealthy,
}

/// A marker for another version is left from a swap that ended some other way.
pub fn on_start(paths: &Paths, running: Version) -> OnStart {
    match paths.read_marker() {
        Some(marker) if marker.to == running.to_string() => OnStart::ProveHealthy,
        Some(_) => {
            paths.clear_marker();
            OnStart::Nothing
        }
        None => OnStart::Nothing,
    }
}

/// Clears the marker once the service has run for `HEALTHY_AFTER` and its API answers.
pub fn prove_healthy(paths: &Paths, started: Instant, api_answers: &dyn Fn() -> bool, wait: &dyn Fn(Duration)) {
    loop {
        if !paths.swap_pending() {
            return;
        }
        if started.elapsed() >= HEALTHY_AFTER && api_answers() {
            paths.clear_marker();
            return;
        }
        wait(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn paths(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-swap-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Paths { installed: dir.join("mesh-kvm.exe"), folder: dir.join("updates") }
    }

    fn marker() -> Marker {
        Marker { from: "0.7.0".into(), to: "0.7.1".into() }
    }

    fn swapped(paths: &Paths) {
        fs::write(&paths.installed, "old").unwrap();
        let new = paths.installed.with_file_name("new.exe");
        fs::write(&new, "new").unwrap();
        swap_in(paths, &new, &marker()).unwrap();
    }

    /// `healthy_at` is when the new copy clears the marker, in time since the swap.
    struct FakeHost {
        paths: Paths,
        now: Cell<Duration>,
        healthy_at: Option<Duration>,
        calls: Vec<&'static str>,
        /// What the installed place held when the restored service was started.
        restored: Option<String>,
    }

    impl WatchHost for FakeHost {
        fn start_new_service(&mut self) {
            self.calls.push("start new");
        }
        fn stop_new_service(&mut self) {
            self.calls.push("stop new");
        }
        fn start_restored_service(&mut self) {
            self.calls.push("start restored");
            self.restored = fs::read_to_string(&self.paths.installed).ok();
        }
        fn wait(&self, time: Duration) {
            self.now.set(self.now.get() + time);
            if self.healthy_at.is_some_and(|at| self.now.get() >= at) {
                self.paths.clear_marker();
            }
        }
        fn elapsed(&self) -> Duration {
            self.now.get()
        }
    }

    fn host(paths: &Paths, healthy_at: Option<Duration>) -> FakeHost {
        FakeHost { paths: paths.clone(), now: Cell::new(Duration::ZERO), healthy_at, calls: Vec::new(), restored: None }
    }

    #[test]
    fn a_swap_keeps_the_old_exe_and_leaves_a_marker() {
        let paths = paths("swap");
        swapped(&paths);
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "new");
        assert_eq!(fs::read_to_string(aside_path(&paths.installed)).unwrap(), "old");
        assert_eq!(paths.read_marker(), Some(marker()));
    }

    #[test]
    fn a_swap_that_fails_leaves_everything_as_it_was() {
        let paths = paths("swap-fails");
        fs::write(&paths.installed, "old").unwrap();
        assert!(swap_in(&paths, Path::new("/no/such/file"), &marker()).is_err());
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "old");
        assert!(!paths.swap_pending());
    }

    #[test]
    fn a_healthy_new_copy_is_left_to_run() {
        let paths = paths("healthy");
        swapped(&paths);
        let mut host = host(&paths, Some(Duration::from_secs(35)));
        watch(&paths, &mut host);
        assert_eq!(host.calls, vec!["start new"]);
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "new");
        assert_eq!(paths.take_note(), None);
    }

    #[test]
    fn a_new_copy_that_is_not_healthy_in_a_minute_is_replaced_by_the_old_one() {
        let paths = paths("unhealthy");
        swapped(&paths);
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert_eq!(host.calls, vec!["start new", "stop new", "start restored"]);
        assert_eq!(host.restored.as_deref(), Some("old"));
        assert_eq!(host.now.get(), RESTORE_AFTER);
        assert!(!aside_path(&paths.installed).exists());
        assert!(!paths.swap_pending());
        let note = paths.take_note().unwrap();
        assert!(note.contains("0.7.1") && note.contains("0.7.0"), "{note}");
        assert_eq!(paths.take_note(), None, "the note is given once");
    }

    #[test]
    fn with_nothing_to_watch_the_watcher_touches_nothing() {
        let paths = paths("nothing");
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert!(host.calls.is_empty());
    }

    #[test]
    fn with_the_old_exe_gone_the_note_says_so() {
        let paths = paths("old-gone");
        swapped(&paths);
        fs::remove_file(aside_path(&paths.installed)).unwrap();
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "new", "better than nothing");
        assert!(paths.take_note().unwrap().contains("gone"));
    }

    #[test]
    fn the_new_copy_proves_itself_only_after_30_seconds_and_with_its_api_up() {
        let paths = paths("prove");
        swapped(&paths);
        let v071 = Version::parse("0.7.1").unwrap();
        assert_eq!(on_start(&paths, v071), OnStart::ProveHealthy);
        let looks = Cell::new(0);
        let answers = || {
            looks.set(looks.get() + 1);
            looks.get() > 2
        };
        let started = Instant::now() - HEALTHY_AFTER;
        prove_healthy(&paths, started, &answers, &|_| ());
        assert!(!paths.swap_pending());
        assert_eq!(looks.get(), 3);
    }

    #[test]
    fn a_young_service_does_not_count_as_healthy_however_well_it_answers() {
        let paths = paths("young");
        swapped(&paths);
        let waits = Cell::new(0);
        let wait = |_| {
            waits.set(waits.get() + 1);
            assert!(paths.swap_pending(), "cleared too early");
            if waits.get() == 3 {
                paths.clear_marker();
            }
        };
        prove_healthy(&paths, Instant::now(), &|| true, &wait);
        assert_eq!(waits.get(), 3);
    }

    #[test]
    fn a_marker_for_another_version_is_stale() {
        let paths = paths("stale");
        swapped(&paths);
        assert_eq!(on_start(&paths, Version::parse("0.7.0").unwrap()), OnStart::Nothing);
        assert!(!paths.swap_pending());
    }
}
