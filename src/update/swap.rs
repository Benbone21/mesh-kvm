//! The Windows swap, and the watcher that undoes it. A new exe that won't run must not leave the
//! laptop without mesh-kvm, so a marker stays until the new copy proves itself, and a small
//! watcher running from the old exe puts it back if that doesn't happen in time.
//!
//! None of this needs Windows to run, so it's tested everywhere.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))] // Only the Windows build swaps.

use super::trust::Version;
use crate::app::install::aside_path;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the new copy has to run before it counts as healthy.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(30);
/// Starts the watcher, in place of a service.
pub const WATCH_FLAG: &str = "--update-watch";
/// How long after it's started it has to get there. That covers a wait of up to 20 seconds for
/// the old service's ports, and the 30 seconds after that.
pub const RESTORE_AFTER: Duration = Duration::from_secs(90);
const LOOK_EVERY: Duration = Duration::from_millis(500);
/// What the login script runs the old exe as.
pub const RECOVERY_EXE: &str = "mesh-kvm-recover.exe";
pub const MARKER_NAME: &str = "update-pending.json";
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

    /// What a failed update left for the app. It stays until the next swap.
    pub fn read_note(&self) -> Option<String> {
        fs::read_to_string(self.note()).ok()
    }

    fn clear_note(&self) {
        let _ = fs::remove_file(self.note());
    }

    /// Where an old exe still in use goes when a newer update needs its place.
    fn stale(&self) -> PathBuf {
        let mut name = self.installed.as_os_str().to_owned();
        name.push(".old.stale");
        PathBuf::from(name)
    }

    pub fn staged(&self) -> PathBuf {
        let mut name = self.installed.as_os_str().to_owned();
        name.push(".new");
        PathBuf::from(name)
    }

    /// A failed exe is left behind if it couldn't be stopped, a copy if a swap was cut short, and
    /// an old exe that was still running when a later update needed its place. Any later start
    /// clears them.
    pub fn clear_failed(&self) {
        let _ = fs::remove_file(self.failed());
        let _ = fs::remove_file(self.staged());
        let _ = fs::remove_file(self.stale());
        let _ = fs::remove_file(self.folder.join(RECOVERY_EXE));
    }
}

/// Copies `new_exe` beside the installed exe and makes sure it has reached the disk.
fn stage(paths: &Paths, new_exe: &Path) -> std::io::Result<()> {
    let mut source = File::open(new_exe)?;
    let mut staged = File::create(paths.staged())?;
    std::io::copy(&mut source, &mut staged)?;
    staged.sync_all()
}

/// Written whole or not at all, as a crash mustn't leave half a marker.
fn write_marker(paths: &Paths, marker: &Marker) -> Result<(), String> {
    let failed = |e: &dyn std::fmt::Display| format!("Couldn't write the update's marker: {e}");
    let text = serde_json::to_vec(marker).map_err(|e| failed(&e))?;
    fs::create_dir_all(&paths.folder).map_err(|e| failed(&e))?;
    let fresh = paths.marker().with_extension("tmp");
    fs::write(&fresh, text).map_err(|e| failed(&e))?;
    fs::rename(&fresh, paths.marker()).map_err(|e| {
        let _ = fs::remove_file(&fresh);
        failed(&e)
    })
}

/// Puts `new_exe` in the installed place, with the old one kept beside it. The slow, fallible part
/// happens beside the installed exe, which is only touched by two renames once the copy has passed
/// `check`. The marker comes first, so a crash between the renames can be recovered from. On `Err`
/// the installed exe is the one that was there before, and nothing is left behind.
pub fn swap_in(paths: &Paths, new_exe: &Path, marker: &Marker, check: &dyn Fn(&Path) -> Result<(), String>) -> Result<(), String> {
    let drop_staged = || {
        let _ = fs::remove_file(paths.staged());
    };
    stage(paths, new_exe).map_err(|e| format!("Couldn't copy the new version into place: {e}")).inspect_err(|_| drop_staged())?;
    check(&paths.staged()).inspect_err(|_| drop_staged())?;
    write_marker(paths, marker).inspect_err(|_| drop_staged())?;
    let aside = aside_path(&paths.installed);
    if let Err(e) = move_aside(paths, &aside) {
        drop_staged();
        paths.clear_marker();
        return Err(format!("Couldn't move the running copy aside: {e}"));
    }
    if let Err(e) = fs::rename(paths.staged(), &paths.installed) {
        let _ = fs::rename(&aside, &paths.installed);
        drop_staged();
        paths.clear_marker();
        return Err(format!("Couldn't put the new version in place: {e}"));
    }
    paths.clear_note();
    Ok(())
}

/// How long the move aside keeps trying when it's refused for a moment.
const ASIDE_TRIES: u32 = 10;
const ASIDE_GAP: Duration = Duration::from_millis(500);

/// Moves the installed exe aside. An old exe already there from the last update can still be in
/// use, as the watcher from that update runs from it, and Windows won't replace a file that's
/// running. It will rename one, so that one is moved out of the way first.
fn move_aside(paths: &Paths, aside: &Path) -> std::io::Result<()> {
    if aside.is_file() && fs::remove_file(aside).is_err() {
        let _ = fs::remove_file(paths.stale());
        let _ = fs::rename(aside, paths.stale());
    }
    let mut tries = 1;
    loop {
        match fs::rename(&paths.installed, aside) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && tries < ASIDE_TRIES => {
                tries += 1;
                std::thread::sleep(ASIDE_GAP);
            }
            other => return other,
        }
    }
}

/// Swaps, and starts the watcher. The old exe only comes back if the swap really happened: after a
/// swap that failed, what's beside the installed exe is an older version still.
pub fn swap_and_watch(
    paths: &Paths,
    new_exe: &Path,
    marker: &Marker,
    check: &dyn Fn(&Path) -> Result<(), String>,
    start_watcher: &dyn Fn() -> Result<(), String>,
) -> Result<(), String> {
    swap_in(paths, new_exe, marker, check)?;
    start_watcher().inspect_err(|_| {
        let _ = put_old_back(paths);
        paths.clear_marker();
    })
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
    /// Waits for the service that started the watcher to have ended, and ends it if it takes too
    /// long, as it holds the ports a new service needs. At login there is none.
    fn wait_for_old_service(&mut self);
    fn start_new_service(&mut self);
    /// Stops the new service, if it's still going.
    fn stop_new_service(&mut self);
    /// Starts whatever is now in the installed place.
    fn start_restored_service(&mut self);
    /// Keeps `version` from being fetched or installed again.
    fn reject(&mut self, version: &str);
    fn wait(&self, time: Duration);
    fn elapsed(&self) -> Duration;
}

/// Starts the new copy and sees it through. It ends with a service running, whatever it found:
/// it also runs at login when a marker is there, as nothing else would then be watching.
pub fn watch(paths: &Paths, host: &mut dyn WatchHost) {
    host.wait_for_old_service();
    if !paths.swap_pending() {
        return host.start_new_service();
    }
    let marker = paths.read_marker();
    if !paths.installed.exists() {
        return restore(paths, host, marker.as_ref(), false);
    }
    host.start_new_service();
    let started = host.elapsed();
    // Only this swap's marker counts. A later update's is for its own watcher, even if this one
    // has been slow to see that its swap proved itself.
    let still_ours = || paths.swap_pending() && paths.read_marker() == marker;
    while host.elapsed().saturating_sub(started) < RESTORE_AFTER {
        if !still_ours() {
            return;
        }
        host.wait(LOOK_EVERY);
    }
    if still_ours() {
        host.stop_new_service();
        restore(paths, host, marker.as_ref(), true);
    }
}

/// Puts the old exe back and says why. `blame` is for a new version that had its chance.
fn restore(paths: &Paths, host: &mut dyn WatchHost, marker: Option<&Marker>, blame: bool) {
    let (to, from) = marker.map_or(("The new version", "the old one"), |m| (m.to.as_str(), m.from.as_str()));
    let restored = put_old_back(paths);
    let note = match (&restored, blame) {
        (Ok(()), false) => format!("The update to {to} was interrupted, so {from} was put back. It can be tried again."),
        (Ok(()), true) => format!("Version {to} didn't start properly, so version {from} was put back. It won't be offered again."),
        (Err(why), _) => format!("Version {to} didn't start properly. {why}"),
    };
    if let (Ok(()), true, Some(marker)) = (&restored, blame, marker) {
        host.reject(&marker.to);
    }
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
        swap_in(paths, &new, &marker(), &|_| Ok(())).unwrap();
    }

    /// `healthy_at` is when the new copy clears the marker, in time since the swap.
    struct FakeHost {
        paths: Paths,
        now: Cell<Duration>,
        healthy_at: Option<Duration>,
        calls: Vec<&'static str>,
        /// What the installed place held when the restored service was started.
        restored: Option<String>,
        rejected: Option<String>,
        old_ends_after: Duration,
        /// When another update swaps in after this one proved itself.
        next_update_at: Option<Duration>,
    }

    impl WatchHost for FakeHost {
        fn wait_for_old_service(&mut self) {
            self.calls.push("wait for old");
            // Ending the old one takes a while, and the new one's time starts after.
            self.now.set(self.now.get() + self.old_ends_after);
        }
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
        fn reject(&mut self, version: &str) {
            self.rejected = Some(version.to_string());
        }
        fn wait(&self, time: Duration) {
            self.now.set(self.now.get() + time);
            if self.healthy_at.is_some_and(|at| self.now.get() >= at) {
                self.paths.clear_marker();
            }
            if self.next_update_at.is_some_and(|at| self.now.get() >= at) {
                write_marker(&self.paths, &Marker { from: "0.7.1".into(), to: "0.7.2".into() }).unwrap();
            }
        }
        fn elapsed(&self) -> Duration {
            self.now.get()
        }
    }

    fn host(paths: &Paths, healthy_at: Option<Duration>) -> FakeHost {
        FakeHost { paths: paths.clone(), now: Cell::new(Duration::ZERO), healthy_at, calls: Vec::new(), restored: None, rejected: None, old_ends_after: Duration::ZERO, next_update_at: None }
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
        assert!(swap_in(&paths, Path::new("/no/such/file"), &marker(), &|_| Ok(())).is_err());
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "old");
        assert!(!paths.swap_pending());
    }

    fn new_exe(paths: &Paths) -> PathBuf {
        let new = paths.installed.with_file_name("downloaded.exe");
        fs::write(&new, "new").unwrap();
        new
    }

    #[test]
    fn the_new_exe_is_checked_in_place_before_anything_moves() {
        let paths = paths("staged");
        fs::write(&paths.installed, "old").unwrap();
        let new = new_exe(&paths);
        let seen = std::cell::RefCell::new(None);
        let check = |staged: &Path| {
            *seen.borrow_mut() = Some((staged.to_path_buf(), fs::read_to_string(staged).unwrap(), fs::read_to_string(&paths.installed).unwrap()));
            Ok(())
        };
        swap_in(&paths, &new, &marker(), &check).unwrap();
        let (staged, staged_text, installed_then) = seen.into_inner().expect("it was checked");
        assert_eq!(staged, paths.staged(), "beside the installed exe");
        assert_eq!((staged_text.as_str(), installed_then.as_str()), ("new", "old"));
        assert!(!paths.staged().exists(), "it's the installed one now");
    }

    #[test]
    fn a_copy_that_fails_its_check_changes_nothing() {
        let paths = paths("bad-copy");
        fs::write(&paths.installed, "old").unwrap();
        fs::write(aside_path(&paths.installed), "older").unwrap();
        let new = new_exe(&paths);
        assert!(swap_in(&paths, &new, &marker(), &|_| Err("doesn't match".into())).is_err());
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "old");
        assert_eq!(fs::read_to_string(aside_path(&paths.installed)).unwrap(), "older");
        assert!(!paths.staged().exists() && !paths.swap_pending());
    }

    #[test]
    fn a_marker_that_can_not_be_written_leaves_the_installed_exe_alone() {
        let paths = paths("no-marker");
        fs::write(&paths.installed, "current").unwrap();
        fs::write(aside_path(&paths.installed), "older").unwrap();
        let new = new_exe(&paths);
        fs::create_dir_all(paths.marker()).unwrap();
        let result = swap_and_watch(&paths, &new, &marker(), &|_| Ok(()), &|| panic!("nothing to watch"));
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "current", "no silent downgrade");
        assert_eq!(fs::read_to_string(aside_path(&paths.installed)).unwrap(), "older");
        assert!(!paths.staged().exists());
    }

    #[test]
    fn an_old_exe_that_is_locked_never_comes_back_over_the_current_one() {
        let paths = paths("locked");
        fs::write(&paths.installed, "current").unwrap();
        // Something in the way that can't be replaced, as a running exe can't.
        let aside = aside_path(&paths.installed);
        fs::create_dir_all(aside.join("held")).unwrap();
        let new = new_exe(&paths);
        let result = swap_and_watch(&paths, &new, &marker(), &|_| Ok(()), &|| panic!("nothing to watch"));
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "current");
        assert!(!paths.swap_pending() && !paths.staged().exists());
    }

    #[test]
    fn a_watcher_that_will_not_start_puts_the_old_exe_back() {
        let paths = paths("no-watcher");
        fs::write(&paths.installed, "old").unwrap();
        let new = new_exe(&paths);
        assert!(swap_and_watch(&paths, &new, &marker(), &|_| Ok(()), &|| Err("no".into())).is_err());
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "old");
        assert!(!paths.swap_pending());
    }

    #[test]
    fn an_old_exe_from_the_last_update_is_replaced_by_this_one() {
        let paths = paths("second");
        swapped(&paths);
        paths.clear_marker();
        let newer = paths.installed.with_file_name("newer.exe");
        fs::write(&newer, "newer").unwrap();
        swap_in(&paths, &newer, &marker(), &|_| Ok(())).unwrap();
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "newer");
        assert_eq!(fs::read_to_string(aside_path(&paths.installed)).unwrap(), "new");
    }

    #[test]
    fn a_crash_between_the_renames_goes_straight_back_with_no_blame() {
        let paths = paths("crash");
        swapped(&paths);
        fs::rename(&paths.installed, paths.staged()).unwrap();
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert_eq!(host.calls, vec!["wait for old", "start restored"], "no waiting for an exe that isn't there");
        assert_eq!(host.restored.as_deref(), Some("old"));
        assert_eq!(host.rejected, None, "the new version did nothing wrong");
        assert!(paths.read_note().unwrap().contains("interrupted"));
    }

    #[test]
    fn what_a_recovery_left_behind_is_cleared_at_a_later_start() {
        let paths = paths("leftovers");
        fs::create_dir_all(&paths.folder).unwrap();
        for file in [paths.failed(), paths.staged(), paths.stale(), paths.folder.join(RECOVERY_EXE)] {
            fs::write(&file, "x").unwrap();
            paths.clear_failed();
            assert!(!file.exists(), "{}", file.display());
        }
    }

    #[test]
    fn a_login_with_no_marker_still_ends_with_a_service() {
        let paths = paths("login");
        fs::write(&paths.installed, "new").unwrap();
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert_eq!(host.calls, vec!["wait for old", "start new"]);
    }

    #[test]
    fn a_half_written_marker_still_gets_the_new_copy_watched() {
        let paths = paths("half");
        swapped(&paths);
        fs::write(paths.marker(), "{\"fr").unwrap();
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert_eq!(host.restored.as_deref(), Some("old"));
    }

    #[test]
    fn a_version_that_failed_is_turned_down_and_the_note_stays() {
        let paths = paths("reject");
        swapped(&paths);
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert_eq!(host.rejected.as_deref(), Some("0.7.1"));
        let note = paths.read_note().unwrap();
        assert!(note.contains("won't be offered again"), "{note}");
        assert_eq!(paths.read_note().as_deref(), Some(note.as_str()), "it lasts past a restart");
    }

    #[test]
    fn a_healthy_new_copy_is_left_to_run() {
        let paths = paths("healthy");
        swapped(&paths);
        let mut host = host(&paths, Some(Duration::from_secs(35)));
        watch(&paths, &mut host);
        assert_eq!(host.calls, vec!["wait for old", "start new"]);
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "new");
        assert_eq!(paths.read_note(), None);
    }

    #[test]
    fn a_new_copy_that_is_not_healthy_in_time_is_replaced_by_the_old_one() {
        let paths = paths("unhealthy");
        swapped(&paths);
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert_eq!(host.calls, vec!["wait for old", "start new", "stop new", "start restored"]);
        assert_eq!(host.restored.as_deref(), Some("old"));
        assert_eq!(host.now.get(), RESTORE_AFTER);
        assert!(!aside_path(&paths.installed).exists());
        assert!(!paths.swap_pending());
        let note = paths.read_note().unwrap();
        assert!(note.contains("0.7.1") && note.contains("0.7.0"), "{note}");
    }

    #[test]
    fn the_new_service_starts_only_once_the_old_one_has_ended() {
        let paths = paths("old-first");
        swapped(&paths);
        let mut host = host(&paths, Some(Duration::from_secs(20 + 35)));
        host.old_ends_after = Duration::from_secs(20);
        watch(&paths, &mut host);
        assert_eq!(host.calls, vec!["wait for old", "start new"], "the slow old one doesn't count against the new");
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "new");
    }

    #[test]
    fn the_new_copy_gets_its_whole_time_after_a_slow_old_one() {
        let paths = paths("old-slow");
        swapped(&paths);
        let mut host = host(&paths, None);
        host.old_ends_after = Duration::from_secs(20);
        watch(&paths, &mut host);
        assert_eq!(host.now.get(), Duration::from_secs(20) + RESTORE_AFTER);
    }

    #[test]
    fn a_slow_watcher_leaves_the_next_update_alone() {
        let paths = paths("next");
        swapped(&paths);
        let mut host = host(&paths, Some(Duration::from_secs(30)));
        // It never got to look while its own swap had no marker.
        host.healthy_at = None;
        host.next_update_at = Some(Duration::from_secs(30));
        watch(&paths, &mut host);
        assert_eq!(host.calls, vec!["wait for old", "start new"]);
        assert_eq!(host.rejected, None);
        assert_eq!(paths.read_marker().unwrap().to, "0.7.2", "the next update's marker is its own");
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "new");
    }

    #[test]
    fn with_the_old_exe_gone_the_note_says_so() {
        let paths = paths("old-gone");
        swapped(&paths);
        fs::remove_file(aside_path(&paths.installed)).unwrap();
        let mut host = host(&paths, None);
        watch(&paths, &mut host);
        assert_eq!(fs::read_to_string(&paths.installed).unwrap(), "new", "better than nothing");
        assert!(paths.read_note().unwrap().contains("gone"));
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
