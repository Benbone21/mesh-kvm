//! Finding, checking and installing new releases. The service runs all of this, so it works with
//! the app window closed. The app only shows `status_json` and sends the actions in
//! `handle_action`.

pub mod both;
mod check;
pub mod fetch;
#[cfg(target_os = "linux")]
mod linux;
mod peer;
pub mod status;
pub mod store;
pub mod swap;
pub mod trust;
#[cfg(target_os = "windows")]
pub mod windows;

use both::{Devices, PeerLook, Step};
use check::Check;
use ed25519_dalek::VerifyingKey;
use fetch::{Server, Source};
use peer::{PeerApi, PeerLink};
use serde_json::Value;
use status::{Stage, Status, STALE_AFTER};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use store::{now_seconds, Ready, Store};
use trust::{running_version, Rejected, Version};

/// When the first check runs after the service starts, and how often after that.
const FIRST_CHECK: Duration = Duration::from_secs(2 * 60);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How long an update waits for a screen mode trial to end. A trial takes seconds, or longer if
/// the screen won't go back, in which case this isn't the moment to restart anything.
const TRIAL_WAIT: Duration = Duration::from_secs(2 * 60);
const TRIAL_LOOK: Duration = Duration::from_millis(500);

const NOTHING_READY: &str = r#"{"ok":false,"error":"No update is ready on this device."}"#;
const ALREADY_UPDATING: &str = r#"{"ok":false,"error":"An update is already running."}"#;
const ONLY_FROM_HERE: &str = r#"{"ok":false,"error":"Updating both devices can only be started from this device."}"#;

/// Keeps a line for the log. Tests have no business in the real one.
fn log(note: &str) {
    #[cfg(not(test))]
    crate::settings::log_note(note);
    #[cfg(test)]
    let _ = note;
}

/// What installing takes on this operating system.
pub trait Installer: Send + Sync {
    /// True on the installed copy. Any other copy only says that an update exists.
    fn can_install(&self) -> bool;
    /// This device's file in a release.
    fn file_name(&self, version: Version) -> String;
    /// Installs `ready`. `Err` is worded for the person to read. A success ends this process, so
    /// `Ok` only comes back from a test.
    fn install(&self, ready: &Ready) -> Result<(), String>;
}

fn installer() -> Arc<dyn Installer> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(linux::Deb)
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(windows::Swap)
    }
}

pub struct Updater {
    status: Mutex<Status>,
    store: Store,
    key: Option<VerifyingKey>,
    running: Version,
    installer: Arc<dyn Installer>,
    /// True while a screen mode trial runs.
    trial_running: Box<dyn Fn() -> bool + Send + Sync>,
    /// The other device, when there is one.
    peer: Arc<dyn PeerLink>,
    /// Only one update at a time.
    busy: AtomicBool,
    wake: Mutex<Option<Sender<()>>>,
}

impl Updater {
    fn new(
        store: Store,
        key: Option<VerifyingKey>,
        running: Version,
        installer: Arc<dyn Installer>,
        trial_running: Box<dyn Fn() -> bool + Send + Sync>,
        peer: Arc<dyn PeerLink>,
    ) -> Self {
        Self { status: Mutex::default(), store, key, running, installer, trial_running, peer, busy: AtomicBool::new(false), wake: Mutex::default() }
    }

    fn status(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn check<'a>(&'a self, source: &'a Source, file_name: &'a dyn Fn(Version) -> String) -> Check<'a> {
        Check {
            source,
            store: &self.store,
            key: self.key.as_ref(),
            running: self.running,
            status: &self.status,
            file_name,
            fetch_file: self.installer.can_install(),
            gaps: &check::RETRY_GAPS,
        }
    }

    /// Runs checks until the service ends: by the clock, and when told to.
    fn run_checks(&self, source: &Source, wake: &Receiver<()>) {
        let installer = self.installer.clone();
        let file_name = move |version| installer.file_name(version);
        let check = self.check(source, &file_name);
        self.store.remove_installed(self.running);
        check.look_on_disk();
        let mut wait = FIRST_CHECK;
        loop {
            match wake.recv_timeout(wait) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            check.run();
            self.note_if_late(now_seconds());
            wait = CHECK_EVERY;
            while wake.try_recv().is_ok() {}
        }
    }

    fn note_if_late(&self, now: u64) {
        self.status().stale = now.saturating_sub(self.store.last_good(now)) > STALE_AFTER.as_secs();
    }

    /// `update` in `/api/local`.
    fn json(&self) -> Value {
        self.status().json(self.running, self.installer.can_install())
    }

    fn check_now(&self) -> String {
        if let Some(wake) = self.wake.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = wake.send(());
        }
        crate::settings::OK_RESPONSE.to_string()
    }

    /// Takes the one update slot, if there's something to install.
    fn claim(&self, first: Stage) -> Result<(), &'static str> {
        let mut status = self.status();
        if !status.ready || !self.installer.can_install() {
            return Err(NOTHING_READY);
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(ALREADY_UPDATING);
        }
        status.stage = first;
        status.note = None;
        Ok(())
    }

    fn fail(&self, note: String) {
        log(&format!("Update failed: {note}"));
        let mut status = self.status();
        status.stage = Stage::Failed;
        status.note = Some(note);
        drop(status);
        self.busy.store(false, Ordering::SeqCst);
    }

    /// Holds an update back until any screen mode trial is over, as it can't be answered or put
    /// back by a service that's restarting.
    fn wait_for_trial(&self) -> Result<(), String> {
        let started = Instant::now();
        while (self.trial_running)() {
            self.status().stage = Stage::WaitingForTrial;
            if started.elapsed() >= TRIAL_WAIT {
                return Err("A screen change hasn't finished, so nothing was updated. Try again once it has.".to_string());
            }
            std::thread::sleep(TRIAL_LOOK);
        }
        Ok(())
    }

    fn file_for(&self, version: Version) -> String {
        self.installer.file_name(version)
    }

    fn newest(&self) -> Option<Version> {
        self.status().available
    }

    /// Checks the download once more and installs it.
    fn install_here(&self) {
        if let Err(note) = self.wait_for_trial().and_then(|()| self.verified_and_installed()) {
            self.fail(note);
        }
    }

    fn verified_and_installed(&self) -> Result<(), String> {
        self.status().stage = Stage::Installing;
        let version = self.newest().ok_or("No update is ready on this device.")?;
        let ready = self.store.verify(self.key.as_ref(), self.running, version, &self.file_for(version));
        let ready = ready.map_err(|why: Rejected| {
            self.store.reject(version);
            let mut status = self.status();
            status.ready = false;
            status.available = None;
            why.to_string()
        })?;
        self.installer.install(&ready)
    }

    /// The whole of "Update both devices".
    fn update_both_now(&self) {
        let Some(target) = self.newest() else { return self.fail("No update is ready on this device.".to_string()) };
        let done = self.wait_for_trial().and_then(|()| both::update_peer_first(self, target));
        match done {
            Ok(()) => self.install_here(),
            Err(note) => self.fail(note),
        }
    }

    /// Answers an action this module owns, or `None` for one it doesn't. The action names no file
    /// or version: all anyone can ask for is the download that has already passed its checks.
    fn answer(self: &Arc<Self>, action: &str, from_app_here: bool) -> Option<String> {
        let start = |first: Stage, work: fn(&Self)| {
            if let Err(refused) = self.claim(first) {
                return refused.to_string();
            }
            let me = self.clone();
            let spawned = std::thread::Builder::new().name("mesh-kvm-update".into()).spawn(move || work(&me));
            if spawned.is_err() {
                self.fail("The update couldn't start.".to_string());
            }
            crate::settings::OK_RESPONSE.to_string()
        };
        match action {
            "check_update" => Some(self.check_now()),
            "install_update" => Some(start(Stage::Installing, Self::install_here)),
            "update_both" if !from_app_here => Some(ONLY_FROM_HERE.to_string()),
            "update_both" => Some(start(Stage::UpdatingPeer, Self::update_both_now)),
            _ => None,
        }
    }
}

impl Devices for Updater {
    fn look_at_peer(&self) -> Option<PeerLook> {
        self.peer.look()
    }

    fn ask_peer_to_install(&self) -> Result<(), String> {
        self.peer.install()
    }

    fn show(&self, step: Step) {
        self.status().stage = match step {
            Step::UpdatingPeer => Stage::UpdatingPeer,
            Step::WaitingForPeer => Stage::WaitingForPeer,
        };
    }

    fn wait(&self, time: Duration) {
        std::thread::sleep(time);
    }
}

static UPDATER: OnceLock<Arc<Updater>> = OnceLock::new();

/// Starts checking for updates.
pub fn start(trial_running: Box<dyn Fn() -> bool + Send + Sync>) {
    let updater = Arc::new(Updater::new(
        Store::new(store::folder()),
        trust::built_in_key(),
        running_version(),
        installer(),
        trial_running,
        Arc::new(PeerApi),
    ));
    #[cfg(target_os = "windows")]
    windows::after_start(&updater);
    let (wake, woken) = mpsc::channel();
    *updater.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(wake);
    let _ = UPDATER.set(updater.clone());
    updater.note_if_late(now_seconds());
    let spawned = std::thread::Builder::new().name("mesh-kvm-update-check".into()).spawn(move || {
        updater.run_checks(&Source::new(Server::github()), &woken);
    });
    if let Err(e) = spawned {
        log(&format!("Couldn't start checking for updates: {e}"));
    }
}

/// `update` in `/api/local`. Before `start`, only the version is known.
pub fn status_json() -> Value {
    match UPDATER.get() {
        Some(updater) => updater.json(),
        None => Status::default().json(running_version(), false),
    }
}

/// See `Updater::answer`.
pub fn handle_action(action: &str, from_app_here: bool) -> Option<String> {
    UPDATER.get()?.answer(action, from_app_here)
}

#[cfg(test)]
mod tests {
    use super::trust::testing::*;
    use super::trust::{MANIFEST_NAME, SIGNATURE_NAME};
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    const FILE: &str = "update.bin";

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[derive(Default)]
    struct FakeInstaller {
        not_installed_copy: bool,
        fails: Option<String>,
        installed: Mutex<Vec<Version>>,
    }

    impl Installer for FakeInstaller {
        fn can_install(&self) -> bool {
            !self.not_installed_copy
        }
        fn file_name(&self, _version: Version) -> String {
            FILE.to_string()
        }
        fn install(&self, ready: &Ready) -> Result<(), String> {
            self.installed.lock().unwrap().push(ready.version);
            self.fails.clone().map_or(Ok(()), Err)
        }
    }

    #[derive(Default)]
    struct FakePeer {
        asked: Mutex<u32>,
    }

    impl PeerLink for FakePeer {
        fn look(&self) -> Option<PeerLook> {
            let asked = *self.asked.lock().unwrap() > 0;
            Some(PeerLook { name: "Laptop".into(), version: Some(v(if asked { "0.7.0" } else { "0.6.1" })), failed: None })
        }
        fn install(&self) -> Result<(), String> {
            *self.asked.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-updater-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn updater(name: &str, installer: FakeInstaller, trial: Box<dyn Fn() -> bool + Send + Sync>) -> (Arc<Updater>, Arc<FakeInstaller>, Arc<FakePeer>) {
        let (installer, peer) = (Arc::new(installer), Arc::new(FakePeer::default()));
        let store = Store::new(folder(name));
        let key = signing_key(1);
        let dir = store.version_dir(v("0.7.0"));
        fs::create_dir_all(&dir).unwrap();
        let json = manifest_json("0.7.0", FILE, b"new");
        fs::write(dir.join(SIGNATURE_NAME), sign(&key, &json)).unwrap();
        fs::write(dir.join(MANIFEST_NAME), json).unwrap();
        fs::write(dir.join(FILE), b"new").unwrap();
        let updater = Updater::new(store, Some(key.verifying_key()), v("0.6.1"), installer.clone(), trial, peer.clone());
        let mut status = updater.status();
        status.available = Some(v("0.7.0"));
        status.ready = true;
        drop(status);
        (Arc::new(updater), installer, peer)
    }

    fn no_trial() -> Box<dyn Fn() -> bool + Send + Sync> {
        Box::new(|| false)
    }

    fn settle(updater: &Updater, installer: &FakeInstaller) {
        for _ in 0..200 {
            if !installer.installed.lock().unwrap().is_empty() || updater.status().stage == Stage::Failed {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn asking_to_install_installs_the_checked_download() {
        let (updater, installer, _) = updater("install", FakeInstaller::default(), no_trial());
        assert_eq!(updater.answer("install_update", false).unwrap(), crate::settings::OK_RESPONSE);
        settle(&updater, &installer);
        assert_eq!(*installer.installed.lock().unwrap(), vec![v("0.7.0")]);
    }

    #[test]
    fn with_nothing_ready_there_is_nothing_to_install() {
        let (updater, installer, _) = updater("none-ready", FakeInstaller::default(), no_trial());
        updater.status().ready = false;
        assert_eq!(updater.answer("install_update", true).unwrap(), NOTHING_READY);
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_copy_that_is_not_the_installed_one_never_installs() {
        let (updater, installer, _) = updater("copy", FakeInstaller { not_installed_copy: true, ..Default::default() }, no_trial());
        assert_eq!(updater.answer("install_update", true).unwrap(), NOTHING_READY);
        assert_eq!(updater.answer("update_both", true).unwrap(), NOTHING_READY);
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_download_that_changed_since_it_was_checked_is_not_installed() {
        let (updater, installer, _) = updater("changed", FakeInstaller::default(), no_trial());
        fs::write(updater.store.version_dir(v("0.7.0")).join(FILE), b"swapped").unwrap();
        updater.answer("install_update", false);
        settle(&updater, &installer);
        assert!(installer.installed.lock().unwrap().is_empty());
        let status = updater.status().clone();
        assert_eq!((status.stage, status.ready), (Stage::Failed, false));
        assert!(status.note.unwrap().contains("doesn't match"));
    }

    #[test]
    fn a_failed_install_says_why_and_can_be_tried_again() {
        let (updater, installer, _) = updater("fails", FakeInstaller { fails: Some("apt said no.".into()), ..Default::default() }, no_trial());
        updater.answer("install_update", false);
        settle(&updater, &installer);
        settle_stage(&updater, Stage::Failed);
        assert_eq!(updater.status().note.as_deref(), Some("apt said no."));
        assert_eq!(updater.answer("install_update", false).unwrap(), crate::settings::OK_RESPONSE);
    }

    fn settle_stage(updater: &Updater, stage: Stage) {
        for _ in 0..200 {
            if updater.status().stage == stage {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("never reached {stage:?}");
    }

    #[test]
    fn a_second_update_can_not_start_during_the_first() {
        let (updater, _, _) = updater("twice", FakeInstaller::default(), Box::new(|| true));
        assert_eq!(updater.answer("install_update", false).unwrap(), crate::settings::OK_RESPONSE);
        assert_eq!(updater.answer("install_update", false).unwrap(), ALREADY_UPDATING);
        assert_eq!(updater.answer("update_both", true).unwrap(), ALREADY_UPDATING);
    }

    #[test]
    fn an_update_clicked_during_a_mode_trial_waits_for_it_to_end() {
        let trial = Arc::new(AtomicBool::new(true));
        let running = trial.clone();
        let (updater, installer, _) = updater("trial", FakeInstaller::default(), Box::new(move || running.load(Ordering::SeqCst)));
        updater.answer("install_update", false);
        settle_stage(&updater, Stage::WaitingForTrial);
        assert!(installer.installed.lock().unwrap().is_empty());
        trial.store(false, Ordering::SeqCst);
        settle(&updater, &installer);
        assert_eq!(installer.installed.lock().unwrap().len(), 1);
    }

    #[test]
    fn only_the_app_here_can_start_updating_both() {
        let (updater, installer, peer) = updater("both-refused", FakeInstaller::default(), no_trial());
        assert_eq!(updater.answer("update_both", false).unwrap(), ONLY_FROM_HERE);
        assert_eq!(*peer.asked.lock().unwrap(), 0);
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn update_both_does_the_other_device_first_and_then_this_one() {
        let (updater, installer, peer) = updater("both", FakeInstaller::default(), no_trial());
        updater.answer("update_both", true);
        for _ in 0..400 {
            if !installer.installed.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            if *peer.asked.lock().unwrap() > 0 {
                // The wait between looks at the peer is real here.
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        assert_eq!(*peer.asked.lock().unwrap(), 1);
        assert_eq!(*installer.installed.lock().unwrap(), vec![v("0.7.0")]);
    }

    #[test]
    fn extra_fields_in_the_request_mean_nothing() {
        // The action has no way to name a file or a version: `answer` only ever sees its name.
        let (updater, _, _) = updater("names", FakeInstaller::default(), no_trial());
        assert_eq!(updater.answer("set_something_else", true), None);
    }

    #[test]
    fn three_days_with_no_good_check_shows_in_the_note() {
        let (updater, _, _) = updater("late", FakeInstaller::default(), no_trial());
        updater.store.mark_good(1000);
        updater.note_if_late(1000 + STALE_AFTER.as_secs());
        assert!(!updater.status().stale);
        updater.note_if_late(1001 + STALE_AFTER.as_secs());
        assert!(updater.status().stale);
    }
}
