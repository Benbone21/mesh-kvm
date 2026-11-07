//! This device's key pair and the one peer it trusts, kept in `pairing.json`.

use blake2::{Blake2s256, Digest};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const NOISE_IK: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

const FINGERPRINT_LABEL: &[u8] = b"mesh-kvm fingerprint 1";
const FILE_VERSION: u32 = 1;

#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

// By hand, so that no log line or failed assert can print the private key.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("private", &format_args!("<hidden>"))
            .field("public", &self.public)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PeerKey {
    pub public: [u8; 32],
    pub name: String,
    pub paired_at: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pairing {
    pub identity: Identity,
    pub peer: Option<PeerKey>,
}

pub enum Loaded {
    Ready(Pairing),
    /// The file is there but can't be used. It is left as it was.
    Damaged(String),
}

#[derive(Serialize, Deserialize)]
struct FileData {
    version: u32,
    private_key: String,
    public_key: String,
    #[serde(default)]
    peer: Option<FilePeer>,
}

#[derive(Serialize, Deserialize)]
struct FilePeer {
    public_key: String,
    name: String,
    paired_at: u64,
}

pub fn pairing_path() -> PathBuf {
    crate::settings::config_dir().join("pairing.json")
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex32(text: &str) -> Result<[u8; 32], String> {
    if text.len() != 64 || !text.is_ascii() {
        return Err("a key is not 64 hex digits".to_string());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .map_err(|_| "a key has a character that is not hex".to_string())?;
    }
    Ok(out)
}

impl Pairing {
    pub fn fresh() -> Pairing {
        let params = NOISE_IK.parse().expect("fixed noise pattern");
        let pair = snow::Builder::new(params)
            .generate_keypair()
            .expect("system random source");
        let mut private = [0u8; 32];
        let mut public = [0u8; 32];
        private.copy_from_slice(&pair.private[..32]);
        public.copy_from_slice(&pair.public[..32]);
        Pairing { identity: Identity { private, public }, peer: None }
    }

    /// A missing file is a new pairing, which is not saved here.
    pub fn load_from(path: &Path) -> Loaded {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Loaded::Ready(Pairing::fresh());
            }
            Err(e) => return Loaded::Damaged(format!("can't read {}: {e}", path.display())),
        };
        match parse(&text) {
            Ok(pairing) => Loaded::Ready(pairing),
            Err(e) => Loaded::Damaged(format!("{} is not usable: {e}", path.display())),
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let data = FileData {
            version: FILE_VERSION,
            private_key: to_hex(&self.identity.private),
            public_key: to_hex(&self.identity.public),
            peer: self.peer.as_ref().map(|p| FilePeer {
                public_key: to_hex(&p.public),
                name: p.name.clone(),
                paired_at: p.paired_at,
            }),
        };
        let text = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?;
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        std::fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
        let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        tmp_name.push(".tmp");
        let tmp = dir.join(tmp_name);
        let written = write_private(&tmp, text.as_bytes())
            .and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("can't save {}: {e}", path.display()));
        }
        Ok(())
    }

    /// The same on both devices, for checking by eye: `4F2A-91C3`. None until there is a peer.
    pub fn fingerprint(&self) -> Option<String> {
        let peer = self.peer.as_ref()?;
        let (a, b) = if self.identity.public <= peer.public {
            (&self.identity.public, &peer.public)
        } else {
            (&peer.public, &self.identity.public)
        };
        let mut hash = Blake2s256::new();
        hash.update(FINGERPRINT_LABEL);
        hash.update(a);
        hash.update(b);
        let d = hash.finalize();
        Some(format!("{:02X}{:02X}-{:02X}{:02X}", d[0], d[1], d[2], d[3]))
    }
}

fn parse(text: &str) -> Result<Pairing, String> {
    let data: FileData = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if data.version != FILE_VERSION {
        return Err(format!("version {} is not known", data.version));
    }
    let peer = match data.peer {
        Some(p) => Some(PeerKey {
            public: from_hex32(&p.public_key)?,
            name: p.name,
            paired_at: p.paired_at,
        }),
        None => None,
    };
    Ok(Pairing {
        identity: Identity {
            private: from_hex32(&data.private_key)?,
            public: from_hex32(&data.public_key)?,
        },
        peer,
    })
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let mut file = options.open(path)?;
        // A leftover temporary file keeps its old mode otherwise.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()
    }
    #[cfg(not(unix))]
    {
        let mut file = options.open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mk-keys-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn debug_output_never_shows_the_private_key() {
        let pairing = paired();
        let private = &pairing.identity.private;
        for text in [format!("{:?}", pairing.identity), format!("{pairing:?}"), format!("{pairing:#?}")] {
            assert!(text.contains("public"), "{text}");
            assert!(!text.contains(&format!("{private:?}")), "{text}");
            assert!(!text.contains(&format!("{private:#?}")), "{text}");
            assert!(!text.contains(&to_hex(private)), "{text}");
        }
    }

    fn paired() -> Pairing {
        let mut p = Pairing::fresh();
        p.peer = Some(PeerKey {
            public: Pairing::fresh().identity.public,
            name: "Laptop".to_string(),
            paired_at: 1794018000,
        });
        p
    }

    #[test]
    fn pairing_round_trips() {
        let path = temp_dir("round").join("pairing.json");
        let p = paired();
        p.save_to(&path).unwrap();
        match Pairing::load_from(&path) {
            Loaded::Ready(back) => assert_eq!(back, p),
            Loaded::Damaged(e) => panic!("damaged: {e}"),
        }
        let unpaired = Pairing { peer: None, ..p };
        unpaired.save_to(&path).unwrap();
        match Pairing::load_from(&path) {
            Loaded::Ready(back) => assert_eq!(back, unpaired),
            Loaded::Damaged(e) => panic!("damaged: {e}"),
        }
    }

    #[test]
    fn a_missing_file_is_fresh() {
        let path = temp_dir("missing").join("pairing.json");
        match Pairing::load_from(&path) {
            Loaded::Ready(p) => assert!(p.peer.is_none()),
            Loaded::Damaged(e) => panic!("damaged: {e}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn a_corrupt_file_is_damaged_and_left_alone() {
        let path = temp_dir("corrupt").join("pairing.json");
        let bytes = b"{ \"version\": 1, \"private_key\": \"zz\" ";
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(Pairing::load_from(&path), Loaded::Damaged(_)));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn both_ends_show_the_same_fingerprint() {
        let a = paired();
        let b = Pairing {
            identity: Identity { private: [0; 32], public: a.peer.as_ref().unwrap().public },
            peer: Some(PeerKey {
                public: a.identity.public,
                name: "Desktop".to_string(),
                paired_at: 1,
            }),
        };
        let fp = a.fingerprint().unwrap();
        assert_eq!(fp, b.fingerprint().unwrap());
        let b_chars: Vec<char> = fp.chars().collect();
        assert_eq!(b_chars.len(), 9);
        assert_eq!(b_chars[4], '-');
        assert!(b_chars.iter().enumerate().all(|(i, c)| i == 4 || matches!(c, '0'..='9' | 'A'..='F')));
        assert!(Pairing::fresh().fingerprint().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_dir("mode").join("pairing.json");
        let p = paired();
        p.save_to(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        p.save_to(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
    }
}
