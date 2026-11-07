//! The `updates` folder: one folder per version, holding what was downloaded for it, and a small
//! record of when a check last worked and which versions were turned down.

use super::trust::{check_file, Manifest, Rejected, Version, MANIFEST_NAME, MAX_MANIFEST, SIGNATURE_NAME, SIGNATURE_SIZE};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const RECORD_NAME: &str = "check.json";
/// A download still on its way, beside where the finished file goes.
const PART_SUFFIX: &str = ".part";

/// `updates` in the config folder, unless a test says otherwise.
pub fn folder() -> PathBuf {
    crate::settings::config_dir().join("updates")
}

pub fn now_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A download that has passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub version: Version,
    pub file: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Record {
    /// Seconds since 1970 of the last check that got an answer.
    last_good: u64,
    /// Versions not to try again.
    rejected: Vec<String>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn version_dir(&self, version: Version) -> PathBuf {
        self.dir.join(version.to_string())
    }

    pub fn part_path(&self, version: Version, name: &str) -> PathBuf {
        self.version_dir(version).join(format!("{name}{PART_SUFFIX}"))
    }

    fn read_record(&self) -> Record {
        fs::read(self.dir.join(RECORD_NAME)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    fn write_record(&self, record: &Record) {
        let _ = fs::create_dir_all(&self.dir);
        if let Ok(text) = serde_json::to_vec(record) {
            let _ = fs::write(self.dir.join(RECORD_NAME), text);
        }
    }

    /// When a check last got an answer. The first look counts as one, as nothing has gone wrong
    /// yet, so a new install doesn't start out three days late.
    pub fn last_good(&self, now: u64) -> u64 {
        let mut record = self.read_record();
        if record.last_good == 0 {
            record.last_good = now;
            self.write_record(&record);
        }
        record.last_good
    }

    pub fn mark_good(&self, now: u64) {
        let mut record = self.read_record();
        record.last_good = now;
        self.write_record(&record);
    }

    pub fn is_rejected(&self, version: Version) -> bool {
        self.read_record().rejected.contains(&version.to_string())
    }

    /// Throws away what was fetched for `version`, and keeps it from being fetched again.
    pub fn reject(&self, version: Version) {
        let _ = fs::remove_dir_all(self.version_dir(version));
        let mut record = self.read_record();
        record.rejected.push(version.to_string());
        self.write_record(&record);
    }

    fn versions(&self) -> Vec<Version> {
        let Ok(entries) = fs::read_dir(&self.dir) else { return Vec::new() };
        let mut versions: Vec<Version> = entries.flatten().filter_map(|e| Version::parse(e.file_name().to_str()?)).collect();
        versions.sort();
        versions
    }

    /// Checks the download for `version` from the start: every check in the design, on what is on
    /// disk now.
    pub fn verify(&self, key: Option<&VerifyingKey>, running: Version, version: Version, name: &str) -> Result<Ready, Rejected> {
        let dir = self.version_dir(version);
        let manifest = read_capped(&dir.join(MANIFEST_NAME), MAX_MANIFEST)?;
        let signature = read_capped(&dir.join(SIGNATURE_NAME), SIGNATURE_SIZE)?;
        let manifest = Manifest::verify(key, &manifest, &signature)?;
        manifest.check_version(running, version)?;
        let signed = manifest.file(name)?;
        let file = dir.join(name);
        let content = File::open(&file).map_err(|e| Rejected::Unreadable(e.to_string()))?;
        check_file(signed, content)?;
        Ok(Ready { version, file })
    }

    /// The newest finished download that still passes. Run at the start, so that a download
    /// survives a restart and a damaged one doesn't.
    pub fn newest_ready(&self, key: Option<&VerifyingKey>, running: Version, name: impl Fn(Version) -> String) -> Option<Ready> {
        self.versions().into_iter().rev().filter(|v| *v > running).find_map(|v| self.verify(key, running, v, &name(v)).ok())
    }

    /// Deletes every download older than `keep`, or as old, along with what's left of them.
    pub fn remove_older_than(&self, keep: Version) {
        for version in self.versions().into_iter().filter(|v| *v < keep) {
            let _ = fs::remove_dir_all(self.version_dir(version));
        }
    }

    /// Deletes what's no use once `running` runs: everything up to and including it.
    pub fn remove_installed(&self, running: Version) {
        for version in self.versions().into_iter().filter(|v| *v <= running) {
            let _ = fs::remove_dir_all(self.version_dir(version));
        }
    }
}

fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, Rejected> {
    let file = File::open(path).map_err(|e| Rejected::Unreadable(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).map_err(|e| Rejected::Unreadable(e.to_string()))?;
    if bytes.len() as u64 > cap {
        return Err(Rejected::BadManifest);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::super::trust::testing::*;
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-store-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Store::new(dir)
    }

    const NAME: &str = "file.bin";

    fn place(store: &Store, version: &str, bytes: &[u8]) {
        let key = signing_key(1);
        let dir = store.version_dir(v(version));
        fs::create_dir_all(&dir).unwrap();
        let json = manifest_json(version, NAME, bytes);
        fs::write(dir.join(SIGNATURE_NAME), sign(&key, &json)).unwrap();
        fs::write(dir.join(MANIFEST_NAME), json).unwrap();
        fs::write(dir.join(NAME), bytes).unwrap();
    }

    #[test]
    fn a_finished_download_survives_a_restart() {
        let store = store("survives");
        place(&store, "0.7.0", b"new");
        let key = signing_key(1).verifying_key();
        let ready = store.newest_ready(Some(&key), v("0.6.1"), |_| NAME.into()).unwrap();
        assert_eq!(ready, Ready { version: v("0.7.0"), file: store.version_dir(v("0.7.0")).join(NAME) });
    }

    #[test]
    fn a_damaged_or_replayed_download_is_not_ready() {
        let store = store("damaged");
        let key = signing_key(1).verifying_key();
        place(&store, "0.7.0", b"new");
        fs::write(store.version_dir(v("0.7.0")).join(NAME), b"bad").unwrap();
        assert_eq!(store.newest_ready(Some(&key), v("0.6.1"), |_| NAME.into()), None, "changed on disk");
        place(&store, "0.6.0", b"old");
        assert_eq!(store.newest_ready(Some(&key), v("0.6.1"), |_| NAME.into()), None, "older than what runs");
        assert_eq!(store.verify(Some(&key), v("0.6.1"), v("0.6.0"), NAME), Err(Rejected::NotNewer(v("0.6.0"))));
    }

    #[test]
    fn the_folder_a_release_sits_in_must_be_its_version() {
        let store = store("moved");
        let key = signing_key(1).verifying_key();
        place(&store, "0.7.0", b"new");
        fs::rename(store.version_dir(v("0.7.0")), store.version_dir(v("0.8.0"))).unwrap();
        assert_eq!(
            store.verify(Some(&key), v("0.6.1"), v("0.8.0"), NAME),
            Err(Rejected::WrongVersion { signed: v("0.7.0"), expected: v("0.8.0") })
        );
    }

    #[test]
    fn the_newest_good_download_wins() {
        let store = store("newest");
        let key = signing_key(1).verifying_key();
        place(&store, "0.7.0", b"a");
        place(&store, "0.8.0", b"b");
        assert_eq!(store.newest_ready(Some(&key), v("0.6.1"), |_| NAME.into()).unwrap().version, v("0.8.0"));
        fs::remove_file(store.version_dir(v("0.8.0")).join(NAME)).unwrap();
        assert_eq!(store.newest_ready(Some(&key), v("0.6.1"), |_| NAME.into()).unwrap().version, v("0.7.0"));
    }

    #[test]
    fn older_downloads_go_once_a_newer_one_is_complete() {
        let store = store("prune");
        place(&store, "0.7.0", b"a");
        place(&store, "0.8.0", b"b");
        store.remove_older_than(v("0.8.0"));
        assert!(!store.version_dir(v("0.7.0")).exists());
        assert!(store.version_dir(v("0.8.0")).exists());
        store.remove_installed(v("0.8.0"));
        assert!(!store.version_dir(v("0.8.0")).exists());
    }

    #[test]
    fn a_rejected_version_is_deleted_and_remembered() {
        let store = store("rejected");
        place(&store, "0.7.0", b"a");
        assert!(!store.is_rejected(v("0.7.0")));
        store.reject(v("0.7.0"));
        assert!(store.is_rejected(v("0.7.0")));
        assert!(!store.version_dir(v("0.7.0")).exists());
        assert!(Store::new(store.dir.clone()).is_rejected(v("0.7.0")), "it lasts past a restart");
    }

    #[test]
    fn the_first_look_counts_as_a_good_check_and_later_ones_keep_the_last_good_time() {
        let store = store("record");
        assert_eq!(store.last_good(1000), 1000);
        assert_eq!(store.last_good(9000), 1000);
        store.mark_good(5000);
        assert_eq!(store.last_good(9000), 5000);
    }
}
