//! Opening the user's download without being fooled. Each step down from the home folder is opened
//! from the last with symlinks refused, so no path is ever looked up twice. Nothing is believed
//! yet: this only keeps the wrong kind of file from being read at all.

use super::process::c_path;
use super::trust::{Version, MANIFEST_NAME, MAX_FILE, MAX_MANIFEST, SIGNATURE_NAME, SIGNATURE_SIZE};
use std::fs::File;
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

/// The way down from the user's home to the download, as `Store` lays it out.
const UPDATES: [&str; 3] = [".config", "mesh-kvm", "updates"];

/// The three files of a download, open and of the right kind.
#[derive(Debug)]
pub struct Opened {
    pub manifest: File,
    pub signature: File,
    pub package: File,
}

fn open_at(parent: i32, name: &str, flags: i32) -> Result<OwnedFd, String> {
    let name_c = c_path(name)?;
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY;
    let fd = unsafe { libc::openat(parent, name_c.as_ptr(), flags) };
    if fd < 0 {
        return Err(format!("{name} couldn't be opened: {}", std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn stat(fd: &OwnedFd) -> Result<libc::stat, String> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err("A file couldn't be looked at.".to_string());
    }
    Ok(unsafe { stat.assume_init() })
}

fn open_folder(parent: i32, name: &str, uid: u32) -> Result<OwnedFd, String> {
    let folder = open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    if stat(&folder)?.st_uid != uid {
        return Err(format!("{name} doesn't belong to the user."));
    }
    Ok(folder)
}

/// Only the user's own plain file, with no other name. `O_NONBLOCK` keeps a pipe from hanging this
/// before it can be turned down, and a second name could be someone else's file.
fn open_file(folder: &OwnedFd, name: &str, uid: u32, cap: u64) -> Result<File, String> {
    let fd = open_at(folder.as_raw_fd(), name, libc::O_RDONLY | libc::O_NONBLOCK)?;
    let stat = stat(&fd)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(format!("{name} isn't a plain file."));
    }
    if stat.st_uid != uid {
        return Err(format!("{name} doesn't belong to the user."));
    }
    if stat.st_nlink != 1 {
        return Err(format!("{name} has another name somewhere."));
    }
    if stat.st_size as u64 > cap {
        return Err(format!("{name} is too big."));
    }
    Ok(File::from(fd))
}

/// Opens the download for `version` under `home`.
pub fn open_download(home: &Path, uid: u32, version: Version, package: &str) -> Result<Opened, String> {
    let home = home.to_str().ok_or("The user's home isn't a usable path.")?;
    let mut folder = open_folder(libc::AT_FDCWD, home, uid)?;
    for name in UPDATES.into_iter().map(String::from).chain([version.to_string()]) {
        folder = open_folder(folder.as_raw_fd(), &name, uid)?;
    }
    Ok(Opened {
        manifest: open_file(&folder, MANIFEST_NAME, uid, MAX_MANIFEST)?,
        signature: open_file(&folder, SIGNATURE_NAME, uid, SIGNATURE_SIZE)?,
        package: open_file(&folder, package, uid, MAX_FILE)?,
    })
}

/// Copies up to `cap` bytes, and fails on any more: the file can grow after it was looked at.
pub fn copy_capped(source: File, target: &mut File, cap: u64) -> Result<(), String> {
    let mut source = source.take(cap + 1);
    let copied = std::io::copy(&mut source, target).map_err(|e| format!("A file couldn't be copied: {e}"))?;
    if copied > cap {
        return Err("A file grew past what's allowed.".to_string());
    }
    target.flush().map_err(|e| format!("A file couldn't be copied: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    const PACKAGE: &str = "mesh-kvm_0.7.0_amd64.deb";

    fn me() -> u32 {
        unsafe { libc::getuid() }
    }

    fn version() -> Version {
        Version::parse("0.7.0").unwrap()
    }

    /// A home with a download in it.
    fn home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("mesh-kvm-copy-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let dir = home.join(".config/mesh-kvm/updates/0.7.0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_NAME), "{}").unwrap();
        fs::write(dir.join(SIGNATURE_NAME), [0; 64]).unwrap();
        fs::write(dir.join(PACKAGE), "package").unwrap();
        home
    }

    fn open(home: &Path) -> Result<Opened, String> {
        open_download(home, me(), version(), PACKAGE)
    }

    fn download(home: &Path) -> PathBuf {
        home.join(".config/mesh-kvm/updates/0.7.0")
    }

    #[test]
    fn a_plain_download_opens() {
        let home = home("plain");
        let mut opened = open(&home).unwrap();
        let mut text = String::new();
        opened.package.read_to_string(&mut text).unwrap();
        assert_eq!(text, "package");
    }

    #[test]
    fn a_symlink_for_any_file_is_refused() {
        for name in [MANIFEST_NAME, SIGNATURE_NAME, PACKAGE] {
            let home = home(&format!("link-{name}"));
            let target = home.join("elsewhere");
            fs::write(&target, "x").unwrap();
            fs::remove_file(download(&home).join(name)).unwrap();
            symlink(&target, download(&home).join(name)).unwrap();
            assert!(open(&home).is_err(), "{name}");
        }
    }

    #[test]
    fn a_symlink_for_any_folder_on_the_way_is_refused() {
        for (name, step) in [
            ("config", ".config"),
            ("meshkvm", ".config/mesh-kvm"),
            ("updates", ".config/mesh-kvm/updates"),
            ("version", ".config/mesh-kvm/updates/0.7.0"),
        ] {
            let home = home(&format!("folder-{name}"));
            let moved = home.join("moved");
            fs::rename(home.join(step), &moved).unwrap();
            symlink(&moved, home.join(step)).unwrap();
            assert!(open(&home).is_err(), "{step}");
        }
    }

    #[test]
    fn a_home_that_is_a_symlink_is_refused() {
        let home = home("home-link");
        let link = home.with_file_name(format!("mesh-kvm-copy-{}-the-link", std::process::id()));
        let _ = fs::remove_file(&link);
        symlink(&home, &link).unwrap();
        assert!(open(&link).is_err());
        fs::remove_file(link).unwrap();
    }

    #[test]
    fn a_pipe_is_turned_down_without_waiting_for_a_writer() {
        let home = home("pipe");
        let path = download(&home).join(PACKAGE);
        fs::remove_file(&path).unwrap();
        let name = c_path(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open(&home).unwrap_err().contains("plain file"));
    }

    #[test]
    fn a_folder_in_place_of_a_file_is_refused() {
        let home = home("folder-file");
        let path = download(&home).join(PACKAGE);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(open(&home).unwrap_err().contains("plain file"));
    }

    #[test]
    fn a_file_with_a_second_name_is_refused() {
        let home = home("hard-link");
        fs::hard_link(download(&home).join(PACKAGE), home.join("other-name")).unwrap();
        assert!(open(&home).unwrap_err().contains("another name"));
    }

    #[test]
    fn something_that_belongs_to_someone_else_is_refused() {
        let home = home("owner");
        assert!(open_download(&home, me() + 1, version(), PACKAGE).unwrap_err().contains("belong"));
    }

    #[test]
    fn a_file_too_big_is_refused_by_its_size_before_a_byte_is_read() {
        let home = home("big");
        let file = fs::File::options().write(true).open(download(&home).join(PACKAGE)).unwrap();
        file.set_len(MAX_FILE + 1).unwrap();
        assert!(open(&home).unwrap_err().contains("too big"));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_signature_or_list_of_the_wrong_size_is_refused() {
        let home = home("big-sig");
        fs::write(download(&home).join(SIGNATURE_NAME), [0; 65]).unwrap();
        assert!(open(&home).is_err());
        let other = self::home("big-manifest");
        fs::write(download(&other).join(MANIFEST_NAME), vec![b' '; MAX_MANIFEST as usize + 1]).unwrap();
        assert!(open(&other).is_err());
    }

    #[test]
    fn a_missing_file_is_refused() {
        let home = home("missing");
        fs::remove_file(download(&home).join(PACKAGE)).unwrap();
        assert!(open(&home).is_err());
    }

    #[test]
    fn a_file_that_grows_past_the_cap_while_copying_stops_there() {
        let home = home("grows");
        let source = fs::File::open(download(&home).join(PACKAGE)).unwrap();
        let mut target = fs::File::create(home.join("copy")).unwrap();
        assert!(copy_capped(source, &mut target, 3).is_err());
    }
}
