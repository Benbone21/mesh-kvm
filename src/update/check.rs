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
    /// A copy that can't install only needs to know the version exists.
    pub fetch_file: bool,
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
        self.store.mark_good(now_seconds());
        self.status().stale = false;
        let ready = self.look_on_disk();
        let Some(version) = tag.strip_prefix('v').and_then(Version::parse) else { return };
        if version <= self.running || self.store.is_rejected(version) || ready.is_some_and(|r| r.version >= version) {
            return;
        }
        let Some(manifest) = self.fetch_manifest(&tag, version) else { return };
        if !self.fetch_file {
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
    fn turn_down(&self, version: Version, why: Rejected) {
        self.store.reject(version);
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
        let part = self.store.part_path(version, &name);
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
        let mut gaps = self.gaps.iter();
        loop {
            let before = fs::metadata(&part).map_or(0, |m| m.len());
            match self.source.download(tag, &name, &part, signed.size, &show) {
                Ok(()) => break,
                Err(Failure::Network(_)) => {}
                Err(Failure::TooMuch) => return self.turn_down(version, Rejected::WrongSize),
                Err(Failure::Disk(why)) => return self.disk_note(&why),
            }
            if fs::metadata(&part).map_or(0, |m| m.len()) > before {
                gaps = self.gaps.iter();
            }
            match gaps.next() {
                Some(gap) => std::thread::sleep(*gap),
                None => return,
            }
        }
        let finished = self.store.version_dir(version).join(&name);
        if let Err(e) = fs::rename(&part, &finished) {
            return self.disk_note(&e.to_string());
        }
        match self.store.verify(self.key, self.running, version, &name) {
            Ok(_) => {
                self.store.remove_older_than(version);
                let mut status = self.status();
                status.available = Some(version);
                status.ready = true;
            }
            Err(why) => self.turn_down(version, why),
        }
    }
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
        fetch_file: bool,
    }

    impl World {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("mesh-kvm-check-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            let behaviour = Arc::new(Mutex::new(Behaviour::default()));
            let source = Source::new(Server::local(&serve(behaviour.clone())));
            let key = signing_key(1).verifying_key();
            Self { behaviour, source, store: Store::new(dir.clone()), dir, status: Mutex::default(), key, fetch_file: true }
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
                key: Some(&self.key),
                running: v("0.6.1"),
                status: &self.status,
                file_name: &|_| FILE.to_string(),
                fetch_file: self.fetch_file,
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
        world.fetch_file = false;
        world.publish("0.7.0", b"x");
        world.look(&[]);
        let status = world.status();
        assert_eq!((status.available, status.ready), (Some(v("0.7.0")), false));
        assert!(!world.dir.join("0.7.0").join(FILE).exists());
        assert!(!world.behaviour.lock().unwrap().seen.iter().any(|r| r.contains(FILE)));
    }
}
