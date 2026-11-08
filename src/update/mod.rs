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

use both::{Asked, Devices, PeerLook, Step};
use check::Check;
use ed25519_dalek::VerifyingKey;
use fetch::{Server, Source};
use peer::{PeerApi, PeerLink};
use serde_json::Value;
use status::{Preflight, Stage, Status, STALE_AFTER};
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

/// The shortest gap between updates the other device can ask for.
const PEER_INSTALL_GAP: Duration = Duration::from_secs(60);

const NOTHING_READY: &str = r#"{"ok":false,"error":"No update is ready on this device."}"#;
const ALREADY_UPDATING: &str = r#"{"ok":false,"error":"An update is already running."}"#;
const TURNED_DOWN: &str = r#"{"ok":false,"error":"That version was turned down on this device."}"#;
const STILL_PROVING: &str = r#"{"ok":false,"error":"This version is still proving itself."}"#;
const TOO_SOON: &str = r#"{"ok":false,"error":"The other device asked very recently. Try again in a minute."}"#;
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
    /// Whether an install would be let through, without installing anything.
    fn preflight(&self) -> Preflight {
        Preflight::Allowed
    }
    /// The command that installs `ready` by hand, if that's a thing here.
    fn by_hand(&self, _ready: &Ready) -> Option<String> {
        None
    }
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
    /// When the other device last started an update here.
    peer_installed_at: Mutex<Option<Instant>>,
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
        Self { status: Mutex::default(), store, key, running, installer, trial_running, peer, busy: AtomicBool::new(false), wake: Mutex::default(), peer_installed_at: Mutex::default() }
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
            installed_copy: self.installer.can_install(),
            gaps: &check::RETRY_GAPS,
        }
    }

    /// Runs checks until the service ends: by the clock, and when told to.
    fn run_checks(&self, source: &Source, wake: &Receiver<()>) {
        let installer = self.installer.clone();
        let file_name = move |version| installer.file_name(version);
        let check = self.check(source, &file_name);
        if self.installer.can_install() {
            self.store.remove_installed(self.running);
        }
        self.look_at_permission(check.look_on_disk().as_ref());
        let mut wait = FIRST_CHECK;
        loop {
            match wake.recv_timeout(wait) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            check.run();
            self.look_at_permission(check.look_on_disk().as_ref());
            self.note_if_late(now_seconds());
            wait = CHECK_EVERY;
            while wake.try_recv().is_ok() {}
        }
    }

    /// Finds out early whether this device will let the ready download install, so the app can
    /// say so before anyone clicks.
    fn look_at_permission(&self, ready: Option<&Ready>) {
        let can = self.installer.can_install();
        let preflight = ready.filter(|_| can).map(|_| self.installer.preflight());
        let by_hand = ready.filter(|_| can).and_then(|ready| self.installer.by_hand(ready));
        let mut status = self.status();
        status.preflight = preflight;
        status.by_hand = by_hand;
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
        if status.available.is_some_and(|version| self.store.is_rejected(version)) {
            return Err(TURNED_DOWN);
        }
        if status.proving {
            return Err(STILL_PROVING);
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(ALREADY_UPDATING);
        }
        status.stage = first;
        status.note = None;
        status.failed_on_peer = false;
        Ok(())
    }

    /// True if the other device may start an update now, which counts as it doing so. A rollback
    /// or a failed install shouldn't turn into a loop run from over there.
    fn peer_may_install(&self, now: Instant) -> bool {
        let mut last = self.peer_installed_at.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| now.saturating_duration_since(at) < PEER_INSTALL_GAP) {
            return false;
        }
        *last = Some(now);
        true
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
        self.installer.install(&ready.map_err(|why| self.after_bad_look(version, why))?)
    }

    /// A download that was fine and no longer is. One damaged on disk is fetched again, and only
    /// something wrong with the release itself counts against it.
    fn after_bad_look(&self, version: Version, why: Rejected) -> String {
        if !why.is_about_the_release() {
            return why.to_string();
        }
        let damaged = matches!(why, Rejected::WrongHash | Rejected::WrongSize);
        if damaged {
            self.store.remove_download(version, &self.file_for(version));
        } else {
            self.store.reject(version);
        }
        let mut status = self.status();
        status.ready = false;
        status.available = None;
        drop(status);
        if damaged {
            self.check_now();
            return "The download was damaged, so it's being fetched again.".to_string();
        }
        why.to_string()
    }

    /// The whole of "Update both devices".
    fn update_both_now(&self) {
        let Some(target) = self.newest() else { return self.fail("No update is ready on this device.".to_string()) };
        if let Err(note) = self.wait_for_trial() {
            return self.fail(note);
        }
        match both::update_peer_first(self, target) {
            Ok(()) => self.install_here(),
            Err(note) => {
                self.status().failed_on_peer = true;
                self.fail(note);
            }
        }
    }

    /// Answers an action this module owns, or `None` for one it doesn't. The action names no file
    /// or version: all anyone can ask for is the download that has already passed its checks.
    fn answer(self: &Arc<Self>, request: &Value, from_app_here: bool) -> Option<String> {
        let action = request["action"].as_str()?;
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
            "install_update" if !from_app_here && !self.peer_may_install(Instant::now()) => Some(TOO_SOON.to_string()),
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

    fn ask_peer_to_install(&self) -> Asked {
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

/// Set once the service holds every port it needs and runs, and never again once it's failing.
/// Until then a new copy hasn't shown it works, however well its API answers.
static SERVICE_UP: AtomicBool = AtomicBool::new(false);
static SERVICE_FAILING: AtomicBool = AtomicBool::new(false);

/// The service has everything it needs and is running.
pub fn note_service_up() {
    if !SERVICE_FAILING.load(Ordering::SeqCst) {
        SERVICE_UP.store(true, Ordering::SeqCst);
    }
}

/// The service is stopping because something went wrong, which may take a while.
pub fn note_service_failing() {
    SERVICE_FAILING.store(true, Ordering::SeqCst);
    SERVICE_UP.store(false, Ordering::SeqCst);
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn service_is_up() -> bool {
    SERVICE_UP.load(Ordering::SeqCst)
}

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

/// `update` in `/api/local`. Before `start`, only the version is known, and whether a swap is
/// waiting on this version, so a new copy never says it has proved itself when it hasn't.
pub fn status_json() -> Value {
    match UPDATER.get() {
        Some(updater) => updater.json(),
        None => Status { proving: proving_before_start(), ..Default::default() }.json(running_version(), false),
    }
}

fn proving_before_start() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::swapped_to_this_version()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// See `Updater::answer`.
pub fn handle_action(request: &Value, from_app_here: bool) -> Option<String> {
    UPDATER.get()?.answer(request, from_app_here)
}

#[cfg(test)]
mod tests {
    use super::trust::testing::*;
    use super::trust::{MANIFEST_NAME, SIGNATURE_NAME};
    use super::*;
    use serde_json::json;
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
        files: Mutex<Vec<PathBuf>>,
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
            self.files.lock().unwrap().push(ready.file.clone());
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
            Some(PeerLook { name: "Laptop".into(), version: Some(v(if asked { "0.7.0" } else { "0.6.1" })), ..Default::default() })
        }
        fn install(&self) -> Asked {
            *self.asked.lock().unwrap() += 1;
            Asked::Accepted
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
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), false).unwrap(), crate::settings::OK_RESPONSE);
        settle(&updater, &installer);
        assert_eq!(*installer.installed.lock().unwrap(), vec![v("0.7.0")]);
    }

    #[test]
    fn with_nothing_ready_there_is_nothing_to_install() {
        let (updater, installer, _) = updater("none-ready", FakeInstaller::default(), no_trial());
        updater.status().ready = false;
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), true).unwrap(), NOTHING_READY);
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_copy_that_is_not_the_installed_one_never_installs() {
        let (updater, installer, _) = updater("copy", FakeInstaller { not_installed_copy: true, ..Default::default() }, no_trial());
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), true).unwrap(), NOTHING_READY);
        assert_eq!(updater.answer(&json!({ "action": "update_both" }), true).unwrap(), NOTHING_READY);
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_download_that_changed_since_it_was_checked_is_not_installed() {
        let (updater, installer, _) = updater("changed", FakeInstaller::default(), no_trial());
        fs::write(updater.store.version_dir(v("0.7.0")).join(FILE), b"swapped").unwrap();
        updater.answer(&json!({ "action": "install_update" }), false);
        settle(&updater, &installer);
        assert!(installer.installed.lock().unwrap().is_empty());
        let status = updater.status().clone();
        assert_eq!((status.stage, status.ready), (Stage::Failed, false));
        assert!(status.note.unwrap().contains("fetched again"));
        assert!(!updater.store.is_rejected(v("0.7.0")), "one bad file doesn't cost the version");
        assert!(!updater.store.version_dir(v("0.7.0")).join(FILE).exists());
    }

    #[test]
    fn a_file_that_can_not_be_read_for_a_moment_costs_nothing() {
        let (updater, installer, _) = updater("unreadable", FakeInstaller::default(), no_trial());
        fs::remove_file(updater.store.version_dir(v("0.7.0")).join(FILE)).unwrap();
        updater.answer(&json!({ "action": "install_update" }), false);
        settle(&updater, &installer);
        assert_eq!(updater.status().stage, Stage::Failed);
        assert!(!updater.store.is_rejected(v("0.7.0")));
        assert!(updater.status().ready, "the next look sorts it out");
    }

    #[test]
    fn a_failed_install_says_why_and_can_be_tried_again() {
        let (updater, installer, _) = updater("fails", FakeInstaller { fails: Some("apt said no.".into()), ..Default::default() }, no_trial());
        updater.answer(&json!({ "action": "install_update" }), false);
        settle(&updater, &installer);
        settle_stage(&updater, Stage::Failed);
        assert_eq!(updater.status().note.as_deref(), Some("apt said no."));
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), true).unwrap(), crate::settings::OK_RESPONSE);
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
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), false).unwrap(), crate::settings::OK_RESPONSE);
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), true).unwrap(), ALREADY_UPDATING);
        assert_eq!(updater.answer(&json!({ "action": "update_both" }), true).unwrap(), ALREADY_UPDATING);
    }

    #[test]
    fn an_update_clicked_during_a_mode_trial_waits_for_it_to_end() {
        let trial = Arc::new(AtomicBool::new(true));
        let running = trial.clone();
        let (updater, installer, _) = updater("trial", FakeInstaller::default(), Box::new(move || running.load(Ordering::SeqCst)));
        updater.answer(&json!({ "action": "install_update" }), false);
        settle_stage(&updater, Stage::WaitingForTrial);
        assert!(installer.installed.lock().unwrap().is_empty());
        trial.store(false, Ordering::SeqCst);
        settle(&updater, &installer);
        assert_eq!(installer.installed.lock().unwrap().len(), 1);
    }

    #[test]
    fn only_the_app_here_can_start_updating_both() {
        let (updater, installer, peer) = updater("both-refused", FakeInstaller::default(), no_trial());
        assert_eq!(updater.answer(&json!({ "action": "update_both" }), false).unwrap(), ONLY_FROM_HERE);
        assert_eq!(*peer.asked.lock().unwrap(), 0);
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn update_both_does_the_other_device_first_and_then_this_one() {
        let (updater, installer, peer) = updater("both", FakeInstaller::default(), no_trial());
        updater.answer(&json!({ "action": "update_both" }), true);
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
    fn a_request_can_not_name_a_file_or_a_version() {
        let (updater, installer, _) = updater("names", FakeInstaller::default(), no_trial());
        let dir = updater.store.version_dir(v("0.7.0"));
        let request = json!({ "action": "install_update", "version": "9.9.9", "file": "/etc/shadow", "ready": { "file": "/etc/shadow" } });
        assert_eq!(updater.answer(&request, false).unwrap(), crate::settings::OK_RESPONSE);
        settle(&updater, &installer);
        assert_eq!(*installer.installed.lock().unwrap(), vec![v("0.7.0")]);
        assert_eq!(*installer.files.lock().unwrap(), vec![dir.join(FILE)]);
    }

    #[test]
    fn a_request_that_is_not_for_updates_is_left_alone() {
        let (updater, _, _) = updater("others", FakeInstaller::default(), no_trial());
        assert_eq!(updater.answer(&json!({ "action": "set_something_else" }), true), None);
        assert_eq!(updater.answer(&json!({}), true), None);
    }

    #[test]
    fn the_preflight_is_reported_with_a_download_ready_and_not_without() {
        struct Blocked;
        impl Installer for Blocked {
            fn can_install(&self) -> bool {
                true
            }
            fn file_name(&self, _version: Version) -> String {
                FILE.to_string()
            }
            fn install(&self, _ready: &Ready) -> Result<(), String> {
                Ok(())
            }
            fn preflight(&self) -> Preflight {
                Preflight::Denied
            }
            fn by_hand(&self, ready: &Ready) -> Option<String> {
                Some(format!("sudo apt-get install {}", ready.file.display()))
            }
        }
        let (updater, _, _) = updater("preflight", FakeInstaller::default(), no_trial());
        let updater = Updater { installer: Arc::new(Blocked), ..Arc::into_inner(updater).unwrap() };
        let ready = updater.store.verify(updater.key.as_ref(), v("0.6.1"), v("0.7.0"), FILE).unwrap();
        updater.look_at_permission(Some(&ready));
        assert_eq!(updater.json()["preflight"], "denied");
        assert!(updater.json()["by_hand"].as_str().unwrap().ends_with(FILE));
        updater.look_at_permission(None);
        assert_eq!(updater.json()["preflight"], Value::Null);
    }

    #[test]
    fn a_version_rolled_back_can_not_be_asked_for_again() {
        let (updater, installer, _) = updater("rolled-back", FakeInstaller::default(), no_trial());
        updater.store.reject(v("0.7.0"));
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), false).unwrap(), TURNED_DOWN);
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), true).unwrap(), TURNED_DOWN);
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn nothing_new_installs_while_this_version_is_still_proving_itself() {
        let (updater, installer, _) = updater("proving", FakeInstaller::default(), no_trial());
        updater.status().proving = true;
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), false).unwrap(), STILL_PROVING);
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn the_peer_can_only_ask_so_often_and_the_app_here_is_not_held_back() {
        let (updater, _, _) = updater("rate", FakeInstaller { fails: Some("No.".into()), ..Default::default() }, no_trial());
        let now = Instant::now();
        assert!(updater.peer_may_install(now));
        assert!(!updater.peer_may_install(now + Duration::from_secs(59)));
        assert!(updater.peer_may_install(now + PEER_INSTALL_GAP));
    }

    #[test]
    fn a_second_request_from_the_peer_is_refused_with_a_reason() {
        let (updater, installer, _) = updater("rate-real", FakeInstaller { fails: Some("No.".into()), ..Default::default() }, no_trial());
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), false).unwrap(), crate::settings::OK_RESPONSE);
        settle(&updater, &installer);
        settle_stage(&updater, Stage::Failed);
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), false).unwrap(), TOO_SOON);
        assert_eq!(updater.answer(&json!({ "action": "install_update" }), true).unwrap(), crate::settings::OK_RESPONSE, "the person here can try again");
    }

    #[test]
    fn a_peer_that_failed_is_marked_as_the_one_that_failed() {
        struct Refuses;
        impl PeerLink for Refuses {
            fn look(&self) -> Option<PeerLook> {
                Some(PeerLook { name: "Laptop".into(), version: Some(v("0.6.1")), ..Default::default() })
            }
            fn install(&self) -> Asked {
                Asked::Refused("No.".into())
            }
        }
        let (updater, _, _) = updater("peer-fail", FakeInstaller::default(), no_trial());
        let updater = Arc::new(Updater { peer: Arc::new(Refuses), ..Arc::into_inner(updater).unwrap() });
        updater.answer(&json!({ "action": "update_both" }), true);
        settle_stage(&updater, Stage::Failed);
        assert_eq!(updater.json()["failed_on"], "peer");
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
