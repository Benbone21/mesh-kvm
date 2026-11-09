//! Installing on Windows: the exe sits in the user's own folder, so no admin prompt is needed. It's
//! swapped while running, then a watcher running from the old exe sees the new one through.

use super::store::Ready;
use super::swap::{self, Marker, OnStart, Paths, WatchHost};
use super::store::Store;
use super::trust::{check_file, running_version, Version};
use super::{Installer, Updater};
use crate::app::install::{aside_path, installed_exe, runs_from_installed};
use crate::app::launch::{args_for_watcher, args_from_watcher, restart_service, spawn_exe, RESTARTING_ENV};
use crate::platform::detach::{self, Child};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the watcher gives the old service to end by itself before it ends it.
const OLD_SERVICE_ENDS_WITHIN: Duration = Duration::from_secs(20);

pub struct Swap;

fn paths() -> Option<Paths> {
    Some(Paths { installed: installed_exe()?, folder: crate::settings::config_dir() })
}

impl Installer for Swap {
    fn can_install(&self) -> bool {
        runs_from_installed()
    }

    fn file_name(&self, _version: Version) -> String {
        "mesh-kvm-windows-x86_64.exe".to_string()
    }

    fn install(&self, ready: &Ready) -> Result<(), String> {
        let paths = paths().ok_or("Couldn't find where mesh-kvm is installed.")?;
        crate::mute::shutdown();
        let marker = Marker { from: running_version().to_string(), to: ready.version.to_string() };
        let check = |copy: &Path| {
            let content = File::open(copy).map_err(|e| format!("Couldn't check the new version's copy: {e}"))?;
            check_file(&ready.signed, content).map_err(|why| why.to_string())
        };
        if let Err(why) = swap::swap_and_watch(&paths, &ready.file, &marker, &check, &|| start_watcher(&paths)) {
            // The mute is already released, and only a new start brings it back.
            paths.leave_note(&format!("The update couldn't be installed, so this copy keeps running. {why}"));
            return Err(restart_service());
        }
        #[cfg(feature = "test-hooks")]
        super::trust::linger_if_told();
        crate::linklog::stop("update", None);
        std::process::exit(0);
    }
}

/// Runs from the old exe, as the new one may not run at all. It's told this service's process id,
/// so it can wait for this one to be gone before it starts the new one.
fn start_watcher(paths: &Paths) -> Result<(), String> {
    let args = args_for_watcher(std::env::args().skip(1).collect(), std::process::id());
    detach::spawn(&aside_path(&paths.installed), &args, &[])
        .map(drop)
        .map_err(|e| format!("Couldn't start the update's watcher: {e}"))
}

/// True while a swap waits to prove itself, when the old exe isn't to be cleared away.
pub fn swap_pending() -> bool {
    paths().is_some_and(|paths| paths.swap_pending())
}

/// True when this is a new copy that a swap is waiting on, before the updater has even started.
pub fn swapped_to_this_version() -> bool {
    paths().and_then(|paths| paths.read_marker()).is_some_and(|marker| marker.to == running_version().to_string())
}

/// What a service does with what the last update left: shows its note, and if this is the new
/// copy, proves that it works. `proving` is set here, before the API answers anything.
pub fn after_start(updater: &Arc<Updater>) {
    let Some(paths) = paths() else { return };
    paths.clear_failed();
    if let Some(note) = paths.read_note() {
        let mut status = updater.status();
        status.stage = super::status::Stage::Failed;
        status.note = Some(note);
    }
    if swap::on_start(&paths, updater.running) == OnStart::ProveHealthy {
        updater.status().proving = true;
        let started = Instant::now();
        // Answering isn't enough: a service that has every port and is still running is.
        let answers = || super::service_is_up() && matches!(crate::webui::http_call("127.0.0.1", "GET", "/api/local", ""), Ok((200, _)));
        let updater = updater.clone();
        let spawned = std::thread::Builder::new().name("mesh-kvm-update-health".into()).spawn(move || {
            swap::prove_healthy(&paths, started, &answers, &std::thread::sleep);
            updater.status().proving = false;
        });
        drop(spawned);
    }
}

struct RealHost {
    exe: PathBuf,
    /// What the service was started with, so that it comes back the same.
    args: Vec<String>,
    /// The service that started this watcher, if one did.
    old_pid: Option<u32>,
    started: Instant,
    new_service: Option<Child>,
}

impl RealHost {
    /// Started as a replacement, so it waits for ports that are still being let go.
    fn start(&self) -> Option<Child> {
        let started = detach::spawn(&self.exe, &self.args, &[(RESTARTING_ENV, "1")]);
        if let Err(e) = &started {
            super::log(&format!("The update's watcher couldn't start {}: {e}", self.exe.display()));
        }
        started.ok()
    }
}

impl WatchHost for RealHost {
    fn wait_for_old_service(&mut self) {
        if let Some(pid) = self.old_pid {
            if detach::wait_for_exit(pid, OLD_SERVICE_ENDS_WITHIN) {
                super::log(&format!("The old service hadn't ended {} seconds after the update, so it was stopped.", OLD_SERVICE_ENDS_WITHIN.as_secs()));
            }
        }
    }

    fn start_new_service(&mut self) {
        super::log(&format!("The update's watcher is starting {}.", self.exe.display()));
        self.new_service = self.start();
    }

    fn stop_new_service(&mut self) {
        if let Some(child) = &mut self.new_service {
            child.kill();
            child.wait(Duration::from_secs(10));
        }
    }

    fn start_restored_service(&mut self) {
        super::log("The update's watcher put the old version back, and is starting it.");
        let _ = self.start();
    }

    fn reject(&mut self, version: &str) {
        if let Some(version) = Version::parse(version) {
            Store::new(super::store::folder()).reject(version);
        }
    }

    fn wait(&self, time: Duration) {
        std::thread::sleep(time);
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// `--update-watch`.
pub fn run_watcher() {
    let Some(paths) = paths() else { return };
    let told = args_from_watcher(std::env::args().skip(1).collect());
    let mut host = RealHost { exe: paths.installed.clone(), args: told.service, old_pid: told.old_pid, started: Instant::now(), new_service: None };
    swap::watch(&paths, &mut host);
    super::log(&format!("The update's watcher is done, after {} seconds.", host.started.elapsed().as_secs()));
    if told.tray {
        let _ = spawn_exe(paths.installed.clone(), &["--tray"], &[]);
    }
}
