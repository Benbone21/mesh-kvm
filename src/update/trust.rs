//! What makes a release trustworthy: a signature from the key built into the program, a version
//! newer than the one running, and files that match what was signed. HTTPS isn't what's trusted,
//! so nothing here looks at where the bytes came from.
//!
//! The root helper repeats these checks with this same file, so it stays free of everything else.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;

/// The release key, as 64 hex digits, or a placeholder that nothing can verify against.
const RELEASE_KEY: &str = include_str!("../../packaging/release-key.pub");
/// The most `release.json` can sensibly be.
pub const MAX_MANIFEST: u64 = 64 * 1024;
pub const SIGNATURE_SIZE: u64 = 64;
/// The most a release's file can be. A real one is about 15 MB.
pub const MAX_FILE: u64 = 256 * 1024 * 1024;
pub const MANIFEST_NAME: &str = "release.json";
pub const SIGNATURE_NAME: &str = "release.json.sig";

#[cfg(feature = "test-hooks")]
pub const TEST_KEY_ENV: &str = "MESH_KVM_TEST_RELEASE_KEY";
#[cfg(feature = "test-hooks")]
pub const TEST_VERSION_ENV: &str = "MESH_KVM_TEST_VERSION";
/// Ends a test exe that claims another version: the same program, as another release.
#[cfg(feature = "test-hooks")]
pub const TEST_VERSION_TAG: &[u8] = b"MESH-KVM-TEST-VERSION=";

/// A plain three-part version. Anything else, from a tag or an argument, isn't one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    /// Takes only digits and dots: no sign, no space, no leading zero, nothing after the patch.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let mut part = || {
            let digits = parts.next()?;
            let plain = (1..=9).contains(&digits.len())
                && digits.bytes().all(|b| b.is_ascii_digit())
                && (digits == "0" || !digits.starts_with('0'));
            plain.then(|| digits.parse().ok()).flatten()
        };
        let version = Self { major: part()?, minor: part()?, patch: part()? };
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The version of this program.
pub fn running_version() -> Version {
    #[cfg(feature = "test-hooks")]
    if let Some(version) = test_version() {
        return version;
    }
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the package has a plain version")
}

/// What's on the end of a test exe, if anything.
#[cfg(feature = "test-hooks")]
fn test_exe_tail() -> Option<&'static str> {
    static TAIL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let read = || {
        let exe = std::fs::read(std::env::current_exe().ok()?).ok()?;
        let tail = &exe[exe.len().saturating_sub(64)..];
        let at = tail.windows(TEST_VERSION_TAG.len()).position(|w| w == TEST_VERSION_TAG)?;
        Some(String::from_utf8_lossy(&tail[at + TEST_VERSION_TAG.len()..]).into_owned())
    };
    TAIL.get_or_init(read).as_deref()
}

#[cfg(feature = "test-hooks")]
fn test_version() -> Option<Version> {
    let tagged = test_exe_tail().and_then(|tail| Version::parse(tail.split_whitespace().next()?));
    tagged.or_else(|| Version::parse(&std::env::var(TEST_VERSION_ENV).ok()?))
}

/// A test exe can be one that starts and never gets anywhere.
#[cfg(feature = "test-hooks")]
pub fn hang_if_told() {
    while test_exe_tail().is_some_and(|tail| tail.contains("HANG")) {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// The key in `text`, or `None` for the placeholder or anything else that isn't a usable key.
pub fn parse_key(text: &str) -> Option<VerifyingKey> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    let key = VerifyingKey::from_bytes(&bytes).ok()?;
    (!key.is_weak()).then_some(key)
}

/// The key built into this program. With the placeholder there is none, and nothing is trusted.
pub fn built_in_key() -> Option<VerifyingKey> {
    #[cfg(feature = "test-hooks")]
    if let Ok(text) = std::env::var(TEST_KEY_ENV) {
        return parse_key(&text);
    }
    parse_key(RELEASE_KEY)
}

/// Why a release was turned down, worded for the person to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    /// There is no key to check anything with.
    NoKey,
    BadSignature,
    /// It's signed, but says nothing usable.
    BadManifest,
    /// A signed release that isn't newer: an old one being replayed, or simply nothing new.
    NotNewer(Version),
    /// It's signed for another version than the one it came as.
    WrongVersion { signed: Version, expected: Version },
    MissingFile(String),
    WrongSize,
    WrongHash,
    /// The file couldn't be read at all.
    Unreadable(String),
}

impl Rejected {
    /// True if this says something about the release, and not just about this moment here.
    pub fn is_about_the_release(&self) -> bool {
        !matches!(self, Self::NoKey | Self::Unreadable(_))
    }
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoKey => write!(f, "This copy of mesh-kvm has no release key, so it can't check updates."),
            Self::BadSignature => write!(f, "The update's signature is wrong, so it was thrown away."),
            Self::BadManifest => write!(f, "The update's list of files can't be read, so it was thrown away."),
            Self::NotNewer(version) => write!(f, "Version {version} isn't newer than this one."),
            Self::WrongVersion { signed, expected } => {
                write!(f, "The update says it's version {signed} but came as {expected}, so it was thrown away.")
            }
            Self::MissingFile(name) => write!(f, "The update doesn't list {name}, so it was thrown away."),
            Self::WrongSize | Self::WrongHash => write!(f, "The downloaded update doesn't match what was signed, so it was thrown away."),
            Self::Unreadable(why) => write!(f, "The update couldn't be read: {why}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SignedFile {
    pub size: u64,
    pub sha256: String,
}

#[derive(Deserialize)]
struct RawManifest {
    version: String,
    files: BTreeMap<String, SignedFile>,
}

/// A `release.json` whose signature has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: Version,
    pub files: BTreeMap<String, SignedFile>,
}

impl Manifest {
    /// Check 1: the signature, before a byte of the contents is believed.
    pub fn verify(key: Option<&VerifyingKey>, json: &[u8], signature: &[u8]) -> Result<Self, Rejected> {
        let key = key.ok_or(Rejected::NoKey)?;
        let signature = Signature::from_slice(signature).map_err(|_| Rejected::BadSignature)?;
        key.verify_strict(json, &signature).map_err(|_| Rejected::BadSignature)?;
        let raw: RawManifest = serde_json::from_slice(json).map_err(|_| Rejected::BadManifest)?;
        let version = Version::parse(&raw.version).ok_or(Rejected::BadManifest)?;
        Ok(Self { version, files: raw.files })
    }

    /// Checks 2 and 3: newer than what's running, and the version it came as.
    pub fn check_version(&self, running: Version, expected: Version) -> Result<(), Rejected> {
        if self.version <= running {
            return Err(Rejected::NotNewer(self.version));
        }
        if self.version != expected {
            return Err(Rejected::WrongVersion { signed: self.version, expected });
        }
        Ok(())
    }

    pub fn file(&self, name: &str) -> Result<&SignedFile, Rejected> {
        let file = self.files.get(name).ok_or_else(|| Rejected::MissingFile(name.to_string()))?;
        if file.size > MAX_FILE {
            return Err(Rejected::WrongSize);
        }
        Ok(file)
    }
}

/// Check 4: `content` is exactly the file that was signed. It reads one byte past the signed size
/// at most, so an endless file ends here.
pub fn check_file(signed: &SignedFile, content: impl Read) -> Result<(), Rejected> {
    let mut content = content.take(signed.size.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0;
    loop {
        let read = content.read(&mut buffer).map_err(|e| Rejected::Unreadable(e.to_string()))?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buffer[..read]);
    }
    if size != signed.size {
        return Err(Rejected::WrongSize);
    }
    let digest: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if digest != signed.sha256.to_ascii_lowercase() {
        return Err(Rejected::WrongHash);
    }
    Ok(())
}

#[cfg(test)]
pub mod testing {
    //! A key of the tests' own, and releases signed with it.

    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub fn key_hex(key: &SigningKey) -> String {
        key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn manifest_json(version: &str, name: &str, bytes: &[u8]) -> Vec<u8> {
        serde_json::json!({ "version": version, "files": { name: { "size": bytes.len(), "sha256": sha256_hex(bytes) } } })
            .to_string()
            .into_bytes()
    }

    pub fn sign(key: &SigningKey, json: &[u8]) -> Vec<u8> {
        key.sign(json).to_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_compare_as_numbers_and_not_as_text() {
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.7.1") > v("0.7.0"));
        assert_eq!(v("0.7.0"), v("0.7.0"));
        assert_eq!(v("10.20.30").to_string(), "10.20.30");
    }

    #[test]
    fn only_a_plain_version_counts_as_one() {
        for text in [
            "", "1", "1.2", "1.2.3.4", "01.2.3", "1.02.3", "1.2.03", "-1.2.3", "+1.2.3", " 1.2.3", "1.2.3 ", "1.2.3\n",
            "1.2.3-rc1", "v1.2.3", "1..3", ".1.2", "1.2.", "../../etc", "1.2.x", "\u{0661}.2.3", "1234567890.0.0",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
        assert_eq!(Version::parse("0.0.0").unwrap().to_string(), "0.0.0");
    }

    #[test]
    fn the_placeholder_is_no_key_and_nothing_verifies_without_one() {
        assert_eq!(parse_key("PLACEHOLDER-NOT-A-REAL-KEY: run packaging/make-release-key.sh before the first release"), None);
        assert_eq!(parse_key(""), None);
        assert_eq!(parse_key(&"zz".repeat(32)), None);
        let json = manifest_json("0.7.0", "a", b"a");
        let signature = sign(&signing_key(1), &json);
        assert_eq!(Manifest::verify(None, &json, &signature), Err(Rejected::NoKey));
    }

    #[test]
    fn the_key_file_in_the_tree_is_the_placeholder_or_a_real_key() {
        assert!(RELEASE_KEY.starts_with("PLACEHOLDER") || parse_key(RELEASE_KEY).is_some(), "{RELEASE_KEY}");
    }

    #[test]
    fn a_key_with_no_strength_is_refused() {
        // The identity point: every message would check out against a bad signature.
        let mut weak = "01".to_string();
        weak.push_str(&"00".repeat(31));
        assert_eq!(parse_key(&weak), None);
    }

    #[test]
    fn a_key_reads_back_from_its_hex() {
        let key = signing_key(1);
        assert_eq!(parse_key(&format!("{}\n", key_hex(&key))), Some(key.verifying_key()));
    }

    #[cfg(not(feature = "test-hooks"))]
    #[test]
    fn nothing_from_outside_changes_the_key_or_the_version() {
        std::env::set_var("MESH_KVM_TEST_RELEASE_KEY", key_hex(&signing_key(9)));
        std::env::set_var("MESH_KVM_TEST_VERSION", "99.0.0");
        assert_eq!(built_in_key(), parse_key(RELEASE_KEY));
        assert_eq!(running_version().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_good_release_passes_every_check() {
        let key = signing_key(1);
        let (name, file) = ("mesh-kvm_0.7.0_amd64.deb", b"the package".as_slice());
        let json = manifest_json("0.7.0", name, file);
        let manifest = Manifest::verify(Some(&key.verifying_key()), &json, &sign(&key, &json)).unwrap();
        manifest.check_version(v("0.6.1"), v("0.7.0")).unwrap();
        check_file(manifest.file(name).unwrap(), file).unwrap();
    }

    #[test]
    fn a_bad_signature_is_refused_before_the_contents_are_read() {
        let key = signing_key(1);
        let public = key.verifying_key();
        let json = manifest_json("0.7.0", "a", b"a");
        let good = sign(&key, &json);
        let mut flipped = good.clone();
        flipped[10] ^= 1;
        assert_eq!(Manifest::verify(Some(&public), &json, &flipped), Err(Rejected::BadSignature));
        assert_eq!(Manifest::verify(Some(&public), &json, &good[..63]), Err(Rejected::BadSignature));
        assert_eq!(Manifest::verify(Some(&public), &json, b""), Err(Rejected::BadSignature));
        let other = sign(&signing_key(2), &json);
        assert_eq!(Manifest::verify(Some(&public), &json, &other), Err(Rejected::BadSignature), "another key");
        let mut edited = json.clone();
        edited[12] ^= 1;
        assert_eq!(Manifest::verify(Some(&public), &edited, &good), Err(Rejected::BadSignature), "edited after signing");
    }

    #[test]
    fn a_signature_from_one_release_does_not_fit_another() {
        let key = signing_key(1);
        let (old, new) = (manifest_json("0.6.0", "a", b"a"), manifest_json("0.7.0", "a", b"a"));
        let swapped = sign(&key, &old);
        assert_eq!(Manifest::verify(Some(&key.verifying_key()), &new, &swapped), Err(Rejected::BadSignature));
    }

    #[test]
    fn signed_nonsense_is_still_refused() {
        let key = signing_key(1);
        for json in [&b"not json"[..], br#"{"version":"seven","files":{}}"#, br#"{"version":"1.2.3"}"#] {
            let signature = sign(&key, json);
            assert_eq!(Manifest::verify(Some(&key.verifying_key()), json, &signature), Err(Rejected::BadManifest));
        }
    }

    #[test]
    fn an_old_signed_release_replayed_is_not_newer() {
        let key = signing_key(1);
        let json = manifest_json("0.6.1", "a", b"a");
        let manifest = Manifest::verify(Some(&key.verifying_key()), &json, &sign(&key, &json)).unwrap();
        assert_eq!(manifest.check_version(v("0.6.1"), v("0.6.1")), Err(Rejected::NotNewer(v("0.6.1"))), "equal");
        assert_eq!(manifest.check_version(v("0.7.0"), v("0.6.1")), Err(Rejected::NotNewer(v("0.6.1"))), "older");
    }

    #[test]
    fn the_version_must_be_the_one_it_came_as() {
        let key = signing_key(1);
        let json = manifest_json("0.7.0", "a", b"a");
        let manifest = Manifest::verify(Some(&key.verifying_key()), &json, &sign(&key, &json)).unwrap();
        let wrong = manifest.check_version(v("0.6.1"), v("0.8.0"));
        assert_eq!(wrong, Err(Rejected::WrongVersion { signed: v("0.7.0"), expected: v("0.8.0") }));
    }

    #[test]
    fn a_file_must_match_in_size_and_hash() {
        let signed = SignedFile { size: 3, sha256: sha256_hex(b"abc") };
        check_file(&signed, &b"abc"[..]).unwrap();
        assert_eq!(check_file(&signed, &b"abd"[..]), Err(Rejected::WrongHash));
        assert_eq!(check_file(&signed, &b"ab"[..]), Err(Rejected::WrongSize));
        assert_eq!(check_file(&signed, &b"abcd"[..]), Err(Rejected::WrongSize));
        assert_eq!(check_file(&signed, std::io::repeat(b'a')), Err(Rejected::WrongSize), "an endless file ends");
    }

    #[test]
    fn a_file_the_release_does_not_list_or_that_is_too_big_is_refused() {
        let key = signing_key(1);
        let json = serde_json::json!({ "version": "0.7.0", "files": { "big": { "size": MAX_FILE + 1, "sha256": "00" } } }).to_string();
        let manifest = Manifest::verify(Some(&key.verifying_key()), json.as_bytes(), &sign(&key, json.as_bytes())).unwrap();
        assert_eq!(manifest.file("other"), Err(Rejected::MissingFile("other".into())));
        assert_eq!(manifest.file("big"), Err(Rejected::WrongSize));
    }
}
