//! The folder only root can touch: where a download is copied, checked and installed from, and
//! where the last packages stay as a way back.

use super::copy::{copy_capped, Opened};
use super::trust::{check_file, Manifest, Rejected, Version, MANIFEST_NAME, MAX_FILE, MAX_MANIFEST, SIGNATURE_NAME, SIGNATURE_SIZE};
use ed25519_dalek::VerifyingKey;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const STAGING: &str = "staging";
const KEPT: &str = "kept";
const LOCK: &str = "lock";
/// The one installed now, and the one before it, which is the way back if a new version turns out
/// bad. The one installed now is the way back if `apt` leaves a package half done.
const KEEP: usize = 2;
const APT: &str = "/usr/bin/apt-get";
const DPKG_DEB: &str = "/usr/bin/dpkg-deb";
const SYSTEM_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
/// How long to wait for anything else using `apt`, such as a scheduled upgrade.
const APT_LOCK_SECONDS: &str = "DPkg::Lock::Timeout=120";

pub struct Cache {
    root: PathBuf,
}

/// Holds the lock while it lives. It ends with the process, however that ends.
pub struct Lock(#[allow(dead_code)] File);

fn make_private(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

impl Cache {
    /// Opens the folder, making it if it's missing. It has to be a real folder of `owner`'s that
    /// nobody else can write to.
    pub fn open(root: &Path, owner: u32) -> Result<Self, String> {
        if fs::symlink_metadata(root).is_err() {
            make_private(root).map_err(|e| format!("Couldn't make the update folder: {e}"))?;
        }
        let meta = fs::symlink_metadata(root).map_err(|e| format!("Couldn't look at the update folder: {e}"))?;
        if !meta.file_type().is_dir() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return Err("The update folder isn't safe to use.".to_string());
        }
        Ok(Self { root: root.to_path_buf() })
    }

    /// One update at a time, however many are started at once.
    pub fn lock(&self) -> Result<Lock, String> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join(LOCK))
            .map_err(|e| format!("Couldn't take the update lock: {e}"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("Another update is running.".to_string());
        }
        Ok(Lock(file))
    }

    fn staging(&self) -> PathBuf {
        self.root.join(STAGING)
    }

    pub fn staged(&self, name: &str) -> PathBuf {
        self.staging().join(name)
    }

    pub fn clear_staging(&self) {
        let _ = fs::remove_dir_all(self.staging());
    }

    fn copy_in(&self, source: File, name: &str, cap: u64) -> Result<(), String> {
        let mut target = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.staged(name))
            .map_err(|e| format!("Couldn't copy the update: {e}"))?;
        copy_capped(source, &mut target, cap)
    }

    /// Starts from an empty staging folder, so nothing left by an earlier run can count.
    pub fn fill_staging(&self, opened: Opened, package: &str) -> Result<(), String> {
        self.clear_staging();
        make_private(&self.staging()).map_err(|e| format!("Couldn't make the update folder: {e}"))?;
        self.copy_in(opened.manifest, MANIFEST_NAME, MAX_MANIFEST)?;
        self.copy_in(opened.signature, SIGNATURE_NAME, SIGNATURE_SIZE)?;
        self.copy_in(opened.package, package, MAX_FILE)
    }

    /// Every check, on the copies. The user can't reach these, so what passes here stays as it is.
    pub fn check_staging(&self, key: Option<&VerifyingKey>, running: Version, version: Version, package: &str) -> Result<(), Rejected> {
        let read = |name: &str| fs::read(self.staged(name)).map_err(|e| Rejected::Unreadable(e.to_string()));
        let manifest = Manifest::verify(key, &read(MANIFEST_NAME)?, &read(SIGNATURE_NAME)?)?;
        manifest.check_version(running, version)?;
        let signed = manifest.file(package)?;
        let content = File::open(self.staged(package)).map_err(|e| Rejected::Unreadable(e.to_string()))?;
        check_file(signed, content)
    }

    fn kept(&self) -> PathBuf {
        self.root.join(KEPT)
    }

    /// The kept packages, newest first.
    fn kept_packages(&self) -> Vec<(Version, PathBuf)> {
        let Ok(entries) = fs::read_dir(self.kept()) else { return Vec::new() };
        let mut packages: Vec<(Version, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let version = name.to_str()?.strip_prefix("mesh-kvm_")?.strip_suffix("_amd64.deb")?;
                Some((Version::parse(version)?, entry.path()))
            })
            .collect();
        packages.sort();
        packages.reverse();
        packages
    }

    /// Moves the package just installed out of staging, and lets go of older ones.
    pub fn keep(&self, package: &str) {
        let _ = make_private(&self.kept());
        let _ = fs::rename(self.staged(package), self.kept().join(package));
        for (_, old) in self.kept_packages().into_iter().skip(KEEP) {
            let _ = fs::remove_file(old);
        }
    }

    /// The command that puts the last good package back, if there is one.
    pub fn way_back(&self) -> Option<String> {
        let (_, newest) = self.kept_packages().into_iter().next()?;
        Some(format!("sudo apt-get install --reinstall --allow-downgrades {}", newest.display()))
    }
}

/// Makes sure the package says it's this program at this version. The signature already covers
/// that, so this only stops a release put together wrongly from installing as something else.
pub fn check_package(package: &Path, version: Version) -> Result<(), String> {
    let output = Command::new(DPKG_DEB)
        .arg("-f")
        .arg(package)
        .args(["Package", "Version"])
        .env_clear()
        .env("PATH", SYSTEM_PATH)
        .env("LC_ALL", "C.UTF-8")
        .current_dir("/")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("dpkg-deb couldn't start: {e}"))?;
    if output.status.success() && fields_match(&String::from_utf8_lossy(&output.stdout), version) {
        return Ok(());
    }
    Err(format!("That package isn't mesh-kvm {version}."))
}

fn fields_match(said: &str, version: Version) -> bool {
    let field = |name: &str| said.lines().find_map(|l| l.strip_prefix(name)).map(str::trim);
    field("Package:") == Some("mesh-kvm") && field("Version:") == Some(version.to_string().as_str())
}

/// Installs `package` with nothing of the caller's around it. Going through `apt` keeps the
/// Timeshift hook running, and `--no-remove` means a package can't take anything else away.
/// `Err` is apt's last word.
pub fn run_apt(package: &Path) -> Result<(), String> {
    let output = Command::new(APT)
        .args(["install", "-y", "--no-remove"])
        // Keep a changed config file without asking, as nobody is there to answer.
        .args(["-o", "Dpkg::Options::=--force-confdef", "-o", "Dpkg::Options::=--force-confold", "-o", APT_LOCK_SECONDS])
        .arg(package)
        .env_clear()
        .env("PATH", SYSTEM_PATH)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("LC_ALL", "C.UTF-8")
        .env("HOME", "/root")
        .current_dir("/")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("apt couldn't start: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let (out, err) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    Err(last_word(&format!("{out}\n{err}")))
}

/// What to show of `apt`'s output: its last error if it gave one, as warnings come after it too.
fn last_word(said: &str) -> String {
    let lines: Vec<&str> = said.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let error = lines.iter().rev().find(|l| l.starts_with("E:"));
    error.or(lines.last()).map_or_else(|| "apt failed and said nothing.".to_string(), |l| l.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{symlink, PermissionsExt};

    const PACKAGE: &str = "mesh-kvm_0.7.0_amd64.deb";

    fn me() -> u32 {
        unsafe { libc::getuid() }
    }

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-cache-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn manifest(version: &str, bytes: &[u8]) -> Vec<u8> {
        let digest: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
        serde_json::json!({ "version": version, "files": { PACKAGE: { "size": bytes.len(), "sha256": digest } } }).to_string().into_bytes()
    }

    fn stage(cache: &Cache, json: &[u8], signature: &[u8], package: &[u8]) {
        cache.clear_staging();
        make_private(&cache.staging()).unwrap();
        fs::write(cache.staged(MANIFEST_NAME), json).unwrap();
        fs::write(cache.staged(SIGNATURE_NAME), signature).unwrap();
        fs::write(cache.staged(PACKAGE), package).unwrap();
    }

    #[test]
    fn the_folder_is_made_private_when_missing() {
        let root = root("new");
        Cache::open(&root, me()).unwrap();
        assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn a_symlink_or_a_folder_others_can_write_to_is_refused() {
        let real = root("real");
        fs::create_dir_all(&real).unwrap();
        let link = root("link");
        symlink(&real, &link).unwrap();
        assert!(Cache::open(&link, me()).is_err(), "a symlink");
        fs::remove_file(&link).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Cache::open(&real, me()).is_err(), "writable by everyone");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(Cache::open(&real, me() + 1).is_err(), "someone else's");
        let file = root("file");
        fs::write(&file, "").unwrap();
        assert!(Cache::open(&file, me()).is_err(), "a file");
    }

    #[test]
    fn only_one_update_runs_at_a_time() {
        let cache = Cache::open(&root("lock"), me()).unwrap();
        let held = cache.lock().unwrap();
        assert!(cache.lock().is_err());
        drop(held);
        cache.lock().unwrap();
    }

    #[test]
    fn a_stale_file_left_in_staging_is_gone_before_the_copy() {
        let cache = Cache::open(&root("stale"), me()).unwrap();
        stage(&cache, b"old", b"old", b"old");
        fs::write(cache.staged("left-behind"), "x").unwrap();
        let source = cache.root.join("source");
        fs::write(&source, "new").unwrap();
        let open = || File::open(&source).unwrap();
        let opened = Opened { manifest: open(), signature: open(), package: open() };
        cache.fill_staging(opened, PACKAGE).unwrap();
        assert!(!cache.staged("left-behind").exists());
        assert_eq!(fs::read(cache.staged(PACKAGE)).unwrap(), b"new");
    }

    #[test]
    fn only_this_package_at_this_version_passes() {
        let version = v("0.7.0");
        assert!(fields_match("Package: mesh-kvm\nVersion: 0.7.0\n", version));
        for bad in [
            "Package: other\nVersion: 0.7.0\n",
            "Package: mesh-kvm\nVersion: 0.7.1\n",
            "Package: mesh-kvm\n",
            "Version: 0.7.0\n",
            "",
            "Package: mesh-kvm-extra\nVersion: 0.7.0\n",
            "Package: mesh-kvm\nVersion: 0.7.0-1\n",
        ] {
            assert!(!fields_match(bad, version), "{bad:?}");
        }
    }

    #[test]
    fn what_passes_the_checks_is_the_copy_and_not_the_users_file() {
        // The copy is what's looked at, so changing the source afterwards changes nothing.
        let key = SigningKey::from_bytes(&[1; 32]);
        let cache = Cache::open(&root("copy"), me()).unwrap();
        let source = cache.root.join("users-file");
        fs::write(&source, "the package").unwrap();
        let json = manifest("0.7.0", b"the package");
        make_private(&cache.staging()).unwrap();
        cache.copy_in(File::open(&source).unwrap(), PACKAGE, 100).unwrap();
        fs::write(cache.staged(MANIFEST_NAME), &json).unwrap();
        fs::write(cache.staged(SIGNATURE_NAME), key.sign(&json).to_bytes()).unwrap();
        fs::write(&source, "swapped in afterwards").unwrap();
        cache.check_staging(Some(&key.verifying_key()), v("0.6.1"), v("0.7.0"), PACKAGE).unwrap();
        assert_eq!(fs::read(cache.staged(PACKAGE)).unwrap(), b"the package");
    }

    #[test]
    fn the_copy_will_not_follow_something_planted_where_it_writes() {
        let cache = Cache::open(&root("planted"), me()).unwrap();
        make_private(&cache.staging()).unwrap();
        let victim = cache.root.join("victim");
        fs::write(&victim, "safe").unwrap();
        symlink(&victim, cache.staged(PACKAGE)).unwrap();
        let source = cache.root.join("source");
        fs::write(&source, "bad").unwrap();
        assert!(cache.copy_in(File::open(&source).unwrap(), PACKAGE, 100).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"safe");
    }

    #[test]
    fn every_check_runs_on_the_copies() {
        let key = SigningKey::from_bytes(&[1; 32]);
        let public = key.verifying_key();
        let cache = Cache::open(&root("checks"), me()).unwrap();
        let json = manifest("0.7.0", b"good");
        let signature = key.sign(&json).to_bytes();
        let check = |running, version| cache.check_staging(Some(&public), v(running), v(version), PACKAGE);

        stage(&cache, &json, &signature, b"good");
        assert_eq!(check("0.6.1", "0.7.0"), Ok(()));
        assert_eq!(check("0.7.0", "0.7.0"), Err(Rejected::NotNewer(v("0.7.0"))), "same version");
        assert_eq!(check("0.8.0", "0.7.0"), Err(Rejected::NotNewer(v("0.7.0"))), "a downgrade");
        assert!(matches!(check("0.6.1", "0.7.1"), Err(Rejected::WrongVersion { .. })));
        assert_eq!(cache.check_staging(None, v("0.6.1"), v("0.7.0"), PACKAGE), Err(Rejected::NoKey));

        stage(&cache, &json, &signature, b"evil");
        assert_eq!(check("0.6.1", "0.7.0"), Err(Rejected::WrongHash), "a tampered package");
        stage(&cache, &manifest("0.7.0", b"evil"), &signature, b"evil");
        assert_eq!(check("0.6.1", "0.7.0"), Err(Rejected::BadSignature), "a tampered list");
        let other = SigningKey::from_bytes(&[2; 32]).sign(&json).to_bytes();
        stage(&cache, &json, &other, b"good");
        assert_eq!(check("0.6.1", "0.7.0"), Err(Rejected::BadSignature), "another key");
    }

    fn keep(cache: &Cache, version: &str) {
        make_private(&cache.staging()).ok();
        let name = format!("mesh-kvm_{version}_amd64.deb");
        fs::write(cache.staged(&name), version).unwrap();
        cache.keep(&name);
    }

    #[test]
    fn the_last_two_packages_stay_as_the_way_back() {
        let cache = Cache::open(&root("keep"), me()).unwrap();
        assert_eq!(cache.way_back(), None);
        for version in ["0.7.0", "0.7.1", "0.10.0"] {
            keep(&cache, version);
        }
        let kept: Vec<String> = cache.kept_packages().iter().map(|(v, _)| v.to_string()).collect();
        assert_eq!(kept, ["0.10.0", "0.7.1"]);
        let way_back = cache.way_back().unwrap();
        assert!(way_back.starts_with("sudo apt-get install") && way_back.ends_with("mesh-kvm_0.10.0_amd64.deb"), "{way_back}");
    }

    #[test]
    fn apts_last_error_is_what_shows_even_with_warnings_after_it() {
        let said = "Reading package lists...\nE: Unable to correct problems\nW: Download is performed unsandboxed as root\n";
        assert_eq!(last_word(said), "E: Unable to correct problems");
        assert_eq!(last_word("one\ntwo\n\n"), "two");
        assert_eq!(last_word(""), "apt failed and said nothing.");
    }
}
