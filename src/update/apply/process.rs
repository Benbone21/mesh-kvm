//! Making this process safe to be: whatever the caller set up is undone first.

use std::ffi::{CStr, CString, OsStr};
use std::path::PathBuf;

/// `pkexec` starts this with whatever limits the caller chose. A tiny file size limit would cut
/// packages short halfway through writing them to `/usr`, so these go back to what's normal.
const OPEN_FILES: libc::rlim_t = 4096;

/// Everything here runs before anything else, and none of it can fail in a way worth stopping for.
pub fn harden() {
    unsafe {
        // A closed 0, 1 or 2 would be handed to the next file opened here, which would then be
        // written to by anything that prints.
        for fd in 0..3 {
            if libc::fcntl(fd, libc::F_GETFD) == -1 {
                libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            }
        }
        // Nothing the caller had open reaches `apt`, or the package's scripts.
        libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
        libc::umask(0o077);
        libc::setgroups(0, std::ptr::null());
        libc::chdir(c"/".as_ptr());
        // No core file of a root process in a place the user picked.
        let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &none);
        let open_files = libc::rlimit { rlim_cur: OPEN_FILES, rlim_max: OPEN_FILES };
        libc::setrlimit(libc::RLIMIT_NOFILE, &open_files);
        let unlimited = libc::rlimit { rlim_cur: libc::RLIM_INFINITY, rlim_max: libc::RLIM_INFINITY };
        for limit in [libc::RLIMIT_FSIZE, libc::RLIMIT_CPU, libc::RLIMIT_AS, libc::RLIMIT_DATA, libc::RLIMIT_NPROC] {
            libc::setrlimit(limit, &unlimited);
        }
    }
}

/// `pkexec` has cleared most of this already. Nothing here depends on that.
pub fn clear_environment() {
    unsafe { libc::clearenv() };
}

/// What `apt` should start with, once the part that handles the user's files is over.
pub fn ready_for_child() {
    unsafe { libc::umask(0o022) };
}

/// Ends this process with a signal if it's still going after `seconds`. 0 turns that off.
pub fn set_alarm(seconds: u32) {
    unsafe { libc::alarm(seconds) };
}

pub fn is_root() -> bool {
    unsafe { libc::getuid() == 0 && libc::geteuid() == 0 }
}

/// The user who ran `pkexec`, from what it set. Root itself isn't one: mesh-kvm doesn't run as root.
pub fn invoking_uid(value: Option<&OsStr>) -> Result<u32, String> {
    let value = value.ok_or("It has to run through pkexec.")?;
    let text = value.to_str().filter(|t| (1..=10).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_digit()));
    let uid = text.and_then(|t| t.parse::<u32>().ok()).ok_or("PKEXEC_UID isn't a user.")?;
    if uid == 0 || uid == u32::MAX {
        return Err("PKEXEC_UID isn't a user who can update mesh-kvm.".to_string());
    }
    Ok(uid)
}

pub struct User {
    pub uid: u32,
    pub gid: u32,
    /// From the system's list of users, and never from `HOME`.
    pub home: PathBuf,
}

pub fn look_up(uid: u32) -> Result<User, String> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let status = unsafe { libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found) };
    if status != 0 || found.is_null() || entry.pw_dir.is_null() {
        return Err("That user doesn't exist.".to_string());
    }
    let home = unsafe { CStr::from_ptr(entry.pw_dir) };
    let home = PathBuf::from(home.to_str().map_err(|_| "That user's home isn't a usable path.")?);
    if !home.is_absolute() {
        return Err("That user's home isn't a usable path.".to_string());
    }
    Ok(User { uid, gid: entry.pw_gid, home })
}

/// While this lives, files are opened with the user's rights and not root's. Only the effective
/// ids change: the real ones stay root, so the user can't signal or trace this process meanwhile.
pub struct AsUser;

impl AsUser {
    pub fn enter(user: &User) -> Result<Self, String> {
        let done = unsafe { libc::setegid(user.gid) == 0 && libc::seteuid(user.uid) == 0 };
        if !done {
            return Err("Couldn't take on the user's rights.".to_string());
        }
        Ok(Self)
    }
}

impl Drop for AsUser {
    fn drop(&mut self) {
        // Staying as the user only makes the next step fail, which is safe.
        unsafe {
            libc::seteuid(0);
            libc::setegid(0);
        }
    }
}

/// A path for the C library, which has no use for one with a nul in it.
pub fn c_path(text: &str) -> Result<CString, String> {
    CString::new(text).map_err(|_| "That name can't be used.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(text: &str) -> Result<u32, String> {
        invoking_uid(Some(OsStr::new(text)))
    }

    #[test]
    fn the_user_is_a_plain_number_and_never_root() {
        assert_eq!(uid("1000"), Ok(1000));
        for bad in ["", "0", "00", "-1", "+1000", " 1000", "1000 ", "1000x", "0x3e8", "1e3", "4294967295", "4294967296", "99999999999"] {
            assert!(uid(bad).is_err(), "{bad:?}");
        }
        assert!(invoking_uid(None).is_err(), "not started by pkexec");
    }

    #[test]
    fn this_user_is_found_with_a_home_and_a_made_up_one_is_not() {
        let me = unsafe { libc::getuid() };
        assert!(look_up(me).unwrap().home.is_absolute());
        assert!(look_up(3_999_999_999).is_err());
    }
}
