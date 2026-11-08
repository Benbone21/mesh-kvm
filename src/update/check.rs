//! One look for a new release: fetch it if there is one, check it, and say how that went. A look
//! that can't reach anything is quiet, and the next one carries on where it stopped.

use super::fetch::{Failure, Source};
use super::status::{Stage, Status};
use super::store::{now_seconds, Ready, Store};
use super::trust::{Manifest, Rejected, Version, MANIFEST_NAME, MAX_MANIFEST, SIGNATURE_NAME, SIGNATURE_SIZE};
use ed25519_dalek::VerifyingKey;
use std::fs;
use std::sync::Mutex;
use std::time::Duration;

/// The gaps between tries at a download, as Wi-Fi here is bad. Getting further starts them again.
pub const RETRY_GAPS: [Duration; 6] = [
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
];

pub struct Check<'a> {
    pub source: &'a Source,
    pub store: &'a Store,
    pub key: Option<&'a VerifyingKey>,
    pub running: Version,
    pub status: &'a Mutex<Status>,
    /// This device's file in a release.
    pub file_name: &'a dyn Fn(Version) -> String,
    /// A copy that can't install only needs to know the version exists, and leaves the folder to
    /// the copy that can. It may well have no key, so what it thinks of a release counts for nothing.
    pub installed_copy: bool,
    pub gaps: &'a [Duration],
}

impl Check<'_> {
    fn status(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Picks up a finished download, such as one from before a restart.
    pub fn look_on_disk(&self) -> Option<Ready> {
        let ready = self.store.newest_ready(self.key, self.running, self.file_name)?;
        let mut status = self.status();
        status.available = Some(ready.version);
        status.ready = true;
        Some(ready)
    }

    pub fn run(&self) {
        let Ok(tag) = self.source.latest_tag() else { return };
        if self.installed_copy {
            self.store.mark_good(now_seconds());
        }
        self.status().stale = false;
        let ready = self.look_on_disk();
        let Some(version) = tag.strip_prefix('v').and_then(Version::parse) else { return };
        if version <= self.running || self.store.is_rejected(version) || ready.is_some_and(|r| r.version >= version) {
            return;
        }
        let Some(manifest) = self.fetch_manifest(&tag, version) else { return };
        if !self.installed_copy {
            self.status().available = Some(version);
            return;
        }
        self.fetch_release(&tag, version, &manifest);
        let mut status = self.status();
        status.progress = None;
        if status.stage == Stage::Downloading {
            status.stage = Stage::Idle;
        }
    }

    /// Puts down a release that didn't pass. An old release isn't worth a word: it's just ignored.
    /// Only something wrong with the release itself is remembered.
    fn turn_down(&self, version: Version, why: Rejected) {
        if self.installed_copy && why.is_about_the_release() {
            self.store.reject(version);
        }
        super::log(&format!("Update {version} turned down: {why}"));
        if !matches!(why, Rejected::NotNewer(_)) {
            self.status().note = Some(why.to_string());
        }
    }

    fn fetch_manifest(&self, tag: &str, version: Version) -> Option<Manifest> {
        let json = self.small_file(tag, version, MANIFEST_NAME, MAX_MANIFEST)?;
        let signature = self.small_file(tag, version, SIGNATURE_NAME, SIGNATURE_SIZE)?;
        let manifest = Manifest::verify(self.key, &json, &signature).and_then(|manifest| {
            manifest.check_version(self.running, version)?;
            manifest.file(&(self.file_name)(version))?;
            Ok(manifest)
        });
        let manifest = match manifest {
            Ok(manifest) => manifest,
            Err(why) => {
                self.turn_down(version, why);
                return None;
            }
        };
        if !self.installed_copy {
            return Some(manifest);
        }
        let dir = self.store.version_dir(version);
        let saved = fs::create_dir_all(&dir)
            .and_then(|()| fs::write(dir.join(MANIFEST_NAME), json))
            .and_then(|()| fs::write(dir.join(SIGNATURE_NAME), signature));
        if let Err(e) = saved {
            self.disk_note(&e.to_string());
            return None;
        }
        Some(manifest)
    }

    fn small_file(&self, tag: &str, version: Version, name: &str, cap: u64) -> Option<Vec<u8>> {
        match self.source.small_file(tag, name, cap) {
            Ok(bytes) => Some(bytes),
            Err(Failure::TooMuch) => {
                self.turn_down(version, Rejected::BadManifest);
                None
            }
            Err(_) => None,
        }
    }

    fn disk_note(&self, why: &str) {
        self.status().note = Some(format!("The update couldn't be saved, so it will be tried again later: {why}"));
    }

    fn fetch_release(&self, tag: &str, version: Version, manifest: &Manifest) {
        let name = (self.file_name)(version);
        let Ok(signed) = manifest.file(&name) else { return };
        {
            let mut status = self.status();
            if !status.ready {
                status.available = Some(version);
                status.progress = Some(0);
                if status.stage == Stage::Idle {
                    status.stage = Stage::Downloading;
                }
            }
        }
        let show = |have: u64| {
            let mut status = self.status();
            if !status.ready {
                status.progress = Some((have * 100 / signed.size.max(1)).min(100) as u8);
            }
        };
        // A bad byte on the way shouldn't cost a whole version, so it gets a second go from nothing.
        let mut struck = false;
        loop {
            let why = match self.fetch_and_verify(tag, version, &name, signed.size, &show) {
                Fetched::Good => return,
                Fetched::Stopped => return,
                Fetched::Bad(why) => why,
            };
            if struck || !matches!(why, Rejected::WrongHash | Rejected::WrongSize) {
                return self.turn_down(version, why);
            }
            struck = true;
            self.store.remove_download(version, &name);
        }
    }

    fn fetch_and_verify(&self, tag: &str, version: Version, name: &str, size: u64, show: &dyn Fn(u64)) -> Fetched {
        let part = self.store.part_path(version, name);
        let mut gaps = self.gaps.iter();
        loop {
            let before = fs::metadata(&part).map_or(0, |m| m.len());
            match self.source.download(tag, name, &part, size, show) {
                Ok(()) => break,
                Err(Failure::Network(_)) => {}
                Err(Failure::TooMuch) => return Fetched::Bad(Rejected::WrongSize),
                Err(Failure::Disk(why)) => {
                    self.disk_note(&why);
                    return Fetched::Stopped;
                }
            }
            if fs::metadata(&part).map_or(0, |m| m.len()) > before {
                gaps = self.gaps.iter();
            }
            match gaps.next() {
                Some(gap) => std::thread::sleep(*gap),
                None => return Fetched::Stopped,
            }
        }
        if let Err(e) = fs::rename(&part, self.store.version_dir(version).join(name)) {
            self.disk_note(&e.to_string());
            return Fetched::Stopped;
        }
        match self.store.verify(self.key, self.running, version, name) {
            Ok(_) => {
                self.store.remove_older_than(version);
                let mut status = self.status();
                status.available = Some(version);
                status.ready = true;
                status.note = None;
                Fetched::Good
            }
            Err(why) => Fetched::Bad(why),
        }
    }
}

/// How one go at a release's file ended.
enum Fetched {
    Good,
    /// It can't go on for now, and the next look carries on.
    Stopped,
    Bad(Rejected),
}

#[cfg(test)]
mod tests {
    use super::super::fetch::fake::{serve, Behaviour};
    use super::super::fetch::Server;
    use super::super::trust::testing::*;
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    const FILE: &str = "update.bin";

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    struct World {
        behaviour: Arc<Mutex<Behaviour>>,
        source: Source,
        store: Store,
        dir: PathBuf,
        status: Mutex<Status>,
        key: VerifyingKey,
        installed_copy: bool,
        no_key: bool,
    }

    impl World {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("mesh-kvm-check-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            let behaviour = Arc::new(Mutex::new(Behaviour::default()));
            let source = Source::new(Server::local(&serve(behaviour.clone())));
            let key = signing_key(1).verifying_key();
            Self { behaviour, source, store: Store::new(dir.clone()), dir, status: Mutex::default(), key, installed_copy: true, no_key: false }
        }

        /// A release with a good signature on the server.
        fn publish(&self, version: &str, bytes: &[u8]) {
            let json = manifest_json(version, FILE, bytes);
            self.publish_as(version, json.clone(), sign(&signing_key(1), &json), bytes);
        }

        fn publish_as(&self, tag_version: &str, json: Vec<u8>, signature: Vec<u8>, bytes: &[u8]) {
            let mut b = self.behaviour.lock().unwrap();
            b.tag = Some(format!("v{tag_version}"));
            b.files = [
                (MANIFEST_NAME.to_string(), json),
                (SIGNATURE_NAME.to_string(), signature),
                (FILE.to_string(), bytes.to_vec()),
            ]
            .into();
        }

        fn look(&self, gaps: &[Duration]) {
            Check {
                source: &self.source,
                store: &self.store,
                key: (!self.no_key).then_some(&self.key),
                running: v("0.6.1"),
                status: &self.status,
                file_name: &|_| FILE.to_string(),
                installed_copy: self.installed_copy,
                gaps,
            }
            .run()
        }

        fn status(&self) -> Status {
            self.status.lock().unwrap().clone()
        }

        fn requests(&self) -> usize {
            self.behaviour.lock().unwrap().seen.len()
        }
    }

    #[test]
    fn a_new_release_ends_up_downloaded_and_checked() {
        let world = World::new("good");
        world.publish("0.7.0", b"the new version");
        world.look(&[]);
        let status = world.status();
        assert_eq!((status.available, status.ready, status.stage, status.note), (Some(v("0.7.0")), true, Stage::Idle, None));
        assert_eq!(fs::read(world.dir.join("0.7.0").join(FILE)).unwrap(), b"the new version");
    }

    #[test]
    fn a_release_already_here_is_not_fetched_again() {
        let world = World::new("again");
        world.publish("0.7.0", b"x");
        world.look(&[]);
        let before = world.requests();
        world.look(&[]);
        assert_eq!(world.requests(), before + 1, "only the look at the latest tag");
    }

    #[test]
    fn nothing_newer_means_nothing_fetched() {
        let world = World::new("same");
        world.publish("0.6.1", b"x");
        world.look(&[]);
        assert_eq!(world.requests(), 1);
        assert_eq!(world.status(), Status::default());
    }

    #[test]
    fn a_failed_check_is_quiet() {
        let world = World::new("quiet");
        world.look(&[]);
        assert_eq!(world.status(), Status::default());
    }

    #[test]
    fn a_tag_that_is_not_a_version_is_ignored() {
        let world = World::new("tag");
        world.behaviour.lock().unwrap().tag = Some("nightly".into());
        world.look(&[]);
        assert_eq!(world.status(), Status::default());
    }

    #[test]
    fn a_bad_signature_deletes_the_release_says_so_and_never_tries_it_again() {
        let world = World::new("bad-sig");
        let json = manifest_json("0.7.0", FILE, b"x");
        world.publish_as("0.7.0", json.clone(), sign(&signing_key(2), &json), b"x");
        world.look(&[]);
        let status = world.status();
        assert!(status.note.unwrap().contains("signature"));
        assert_eq!((status.available, status.ready), (None, false));
        assert!(!world.dir.join("0.7.0").exists());
        let before = world.requests();
        world.look(&[]);
        assert_eq!(world.requests(), before + 1, "that version is left alone");
    }

    #[test]
    fn a_good_release_after_a_bad_one_clears_the_note() {
        let world = World::new("recovers");
        let json = manifest_json("0.7.0", FILE, b"x");
        world.publish_as("0.7.0", json.clone(), sign(&signing_key(2), &json), b"x");
        world.look(&[]);
        assert!(world.status().note.is_some());
        world.publish("0.7.1", b"fixed");
        world.look(&[]);
        assert_eq!((world.status().ready, world.status().note), (true, None));
    }

    #[test]
    fn one_damaged_download_is_fetched_again_from_nothing_before_it_counts() {
        let world = World::new("second-go");
        world.publish("0.7.0", b"the real one");
        world.behaviour.lock().unwrap().damage_next = 1;
        world.look(&[]);
        let status = world.status();
        assert_eq!((status.ready, status.note), (true, None));
        assert!(!world.store.is_rejected(v("0.7.0")));
        let fetched = world.behaviour.lock().unwrap().seen.iter().filter(|r| r.contains(FILE)).count();
        assert_eq!(fetched, 2);
    }

    #[test]
    fn a_copy_with_no_key_writes_nothing_others_share() {
        let mut world = World::new("no-key");
        world.installed_copy = false;
        world.no_key = true;
        world.publish("0.7.0", b"x");
        world.look(&[]);
        assert!(world.status().note.unwrap().contains("no release key"));
        assert!(!world.store.is_rejected(v("0.7.0")));
        assert!(!world.dir.join("check.json").exists(), "not even the time of the check");
        assert!(!world.dir.join("0.7.0").exists());
    }

    #[test]
    fn a_copy_that_can_not_install_never_rejects_or_deletes_for_the_installed_one() {
        let mut world = World::new("dev-copy");
        world.installed_copy = false;
        let json = manifest_json("0.7.0", FILE, b"x");
        world.publish_as("0.7.0", json.clone(), sign(&signing_key(2), &json), b"x");
        fs::create_dir_all(world.dir.join("0.7.0")).unwrap();
        fs::write(world.dir.join("0.7.0").join(FILE), b"the installed copy's download").unwrap();
        world.look(&[]);
        assert!(world.status().note.is_some(), "it still says");
        assert!(!world.store.is_rejected(v("0.7.0")));
        assert!(world.dir.join("0.7.0").join(FILE).exists());
    }

    #[test]
    fn a_tampered_file_deletes_the_release_and_says_so() {
        let world = World::new("tampered");
        let json = manifest_json("0.7.0", FILE, b"the real one");
        world.publish_as("0.7.0", json.clone(), sign(&signing_key(1), &json), b"the fake one");
        world.look(&[]);
        let status = world.status();
        assert!(status.note.unwrap().contains("doesn't match"));
        assert!(!status.ready);
        assert!(!world.dir.join("0.7.0").exists());
    }

    #[test]
    fn a_file_longer_than_signed_stops_there() {
        let world = World::new("long");
        let json = manifest_json("0.7.0", FILE, b"short");
        world.publish_as("0.7.0", json.clone(), sign(&signing_key(1), &json), &[7; 5000]);
        world.look(&[]);
        assert!(world.status().note.unwrap().contains("doesn't match"));
        assert!(!world.dir.join("0.7.0").exists());
    }

    #[test]
    fn an_old_signed_release_under_a_new_tag_is_refused() {
        let world = World::new("old-under-new-tag");
        let json = manifest_json("0.7.0", FILE, b"x");
        world.publish_as("0.8.0", json.clone(), sign(&signing_key(1), &json), b"x");
        world.look(&[]);
        assert!(world.status().note.unwrap().contains("came as 0.8.0"));
    }

    #[test]
    fn a_replayed_old_release_is_ignored_without_a_word() {
        let world = World::new("replay");
        let json = manifest_json("0.6.0", FILE, b"x");
        world.publish_as("0.7.0", json.clone(), sign(&signing_key(1), &json), b"x");
        world.look(&[]);
        assert_eq!(world.status(), Status::default());
    }

    #[test]
    fn a_drop_in_the_download_carries_on_and_a_later_look_finishes_it() {
        let world = World::new("drop");
        let bytes = vec![3; 50_000];
        world.publish("0.7.0", &bytes);
        world.behaviour.lock().unwrap().cut_after = vec![20_000, 20_000, 0];
        // No gaps left to wait through, so the look gives up with most of it here.
        world.look(&[Duration::ZERO]);
        let status = world.status();
        assert_eq!((status.available, status.ready), (Some(v("0.7.0")), false), "nothing is offered until it's complete");
        assert_eq!(fs::metadata(world.dir.join("0.7.0").join(format!("{FILE}.part"))).unwrap().len(), 40_000);
        world.look(&[]);
        assert!(world.status().ready);
        assert_eq!(fs::read(world.dir.join("0.7.0").join(FILE)).unwrap(), bytes);
    }

    #[test]
    fn older_downloads_stay_on_offer_until_a_newer_one_is_complete() {
        let world = World::new("newer");
        world.publish("0.7.0", b"old");
        world.look(&[]);
        world.publish("0.8.0", &[1; 5000]);
        world.behaviour.lock().unwrap().cut_after = vec![100];
        world.look(&[]);
        let status = world.status();
        assert_eq!((status.available, status.ready), (Some(v("0.7.0")), true), "it's still the one ready");
        assert!(world.dir.join("0.7.0").exists());
        world.look(&[]);
        assert_eq!((world.status().available, world.status().ready), (Some(v("0.8.0")), true));
        assert!(!world.dir.join("0.7.0").exists());
    }

    #[test]
    fn a_full_disk_is_said_and_leaves_no_partial_file() {
        let world = World::new("disk");
        world.publish("0.7.0", &[1; 5000]);
        fs::create_dir_all(&world.dir).unwrap();
        // A folder in the file's place makes writing it fail, as a disk can't be filled here.
        fs::create_dir_all(world.dir.join("0.7.0").join(format!("{FILE}.part"))).unwrap();
        world.look(&[]);
        let status = world.status();
        assert!(status.note.unwrap().contains("couldn't be saved"));
        assert!(!status.ready);
    }

    #[test]
    fn a_copy_that_can_not_install_only_learns_the_version() {
        let mut world = World::new("view");
        world.installed_copy = false;
        world.publish("0.7.0", b"x");
        world.look(&[]);
        let status = world.status();
        assert_eq!((status.available, status.ready), (Some(v("0.7.0")), false));
        assert!(!world.dir.join("0.7.0").join(FILE).exists());
        assert!(!world.behaviour.lock().unwrap().seen.iter().any(|r| r.contains(FILE)));
    }
}
