//! Installing on Windows: the exe sits in the user's own folder, so no admin prompt is needed. It's
//! swapped while running, then a watcher running from the old exe sees the new one through.

use super::store::Ready;
use super::swap::{self, Marker, OnStart, Paths, WatchHost};
use super::store::Store;
use super::trust::{check_file, running_version, Version};
use super::{Installer, Updater};
use crate::app::install::{aside_path, installed_exe};
use crate::app::launch::{args_for_watcher, args_from_watcher, restart_service, RESTARTING_ENV};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Swap;

fn paths() -> Option<Paths> {
    Some(Paths { installed: installed_exe()?, folder: crate::settings::config_dir() })
}

/// True when this exe is the installed one, however the two are spelt.
fn runs_from_installed() -> bool {
    let (Ok(exe), Some(installed)) = (std::env::current_exe(), installed_exe()) else { return false };
    matches!((exe.canonicalize(), installed.canonicalize()), (Ok(a), Ok(b)) if a == b)
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
        std::process::exit(0);
    }
}

/// Runs from the old exe, as the new one may not run at all.
fn start_watcher(paths: &Paths) -> Result<(), String> {
    Command::new(aside_path(&paths.installed))
        .args(args_for_watcher(std::env::args().skip(1).collect()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("Couldn't start the update's watcher: {e}"))
}

/// True while a swap waits to prove itself, when the old exe isn't to be cleared away.
pub fn swap_pending() -> bool {
    paths().is_some_and(|paths| paths.swap_pending())
}

/// What a service does with what the last update left: shows its note, and if this is the new
/// copy, proves that it works.
pub fn after_start(updater: &Arc<Updater>) {
    let Some(paths) = paths() else { return };
    paths.clear_failed();
    if let Some(note) = paths.read_note() {
        let mut status = updater.status();
        status.stage = super::status::Stage::Failed;
        status.note = Some(note);
    }
    if swap::on_start(&paths, updater.running) == OnStart::ProveHealthy {
        let started = Instant::now();
        let answers = || matches!(crate::webui::http_call("127.0.0.1", "GET", "/api/local", ""), Ok((200, _)));
        let spawned = std::thread::Builder::new().name("mesh-kvm-update-health".into()).spawn(move || {
            swap::prove_healthy(&paths, started, &answers, &std::thread::sleep);
        });
        drop(spawned);
    }
}

struct RealHost {
    exe: PathBuf,
    /// What the service was started with, so that it comes back the same.
    args: Vec<String>,
    started: Instant,
    new_service: Option<Child>,
}

impl RealHost {
    fn start(&self) -> Option<Child> {
        Command::new(&self.exe)
            .args(&self.args)
            .env(RESTARTING_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }
}

impl WatchHost for RealHost {
    fn start_new_service(&mut self) {
        self.new_service = self.start();
    }

    fn stop_new_service(&mut self) {
        if let Some(child) = &mut self.new_service {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn start_restored_service(&mut self) {
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
    let (args, tray) = args_from_watcher(std::env::args().skip(1).collect());
    let mut host = RealHost { exe: paths.installed.clone(), args, started: Instant::now(), new_service: None };
    swap::watch(&paths, &mut host);
    if tray {
        let _ = Command::new(&paths.installed).arg("--tray").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}
