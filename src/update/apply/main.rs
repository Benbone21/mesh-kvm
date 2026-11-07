//! `apply-update`: installs a checked `.deb` as root, for the logged-in user, with no password.
//! That makes it the one way from the user's account to root, so it believes nothing it's handed.
//!
//! The only input is a version. The user comes from `PKEXEC_UID`, and everything else is a fixed
//! path or was built into this program. The download is copied into a folder only root can touch,
//! and only those copies are checked and installed, so nothing can change after it's checked.
//! Whatever the user controls is read with the user's own rights.

#[cfg(target_os = "linux")]
mod cache;
#[cfg(target_os = "linux")]
mod copy;
#[cfg(target_os = "linux")]
mod process;
#[cfg(target_os = "linux")]
#[allow(dead_code)] // The service uses the rest.
#[path = "../trust.rs"]
mod trust;

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use super::cache::{self, Cache};
    use super::copy;
    use super::process::{self, AsUser, User};
    use super::trust::{self, Version};
    use ed25519_dalek::VerifyingKey;
    use std::ffi::OsString;
    use std::io::Write;
    use std::path::Path;
    use std::process::ExitCode;

    const CACHE: &str = "/var/cache/mesh-kvm";
    /// Copying is seconds of work. A file that hangs, such as on a mount the user made, ends here.
    const COPY_LIMIT_SECONDS: u32 = 120;

    /// How a run ended, other than well.
    pub enum Stop {
        Refused(String),
        AptFailed { said: String, way_back: Option<String> },
    }

    fn refused<T>(why: impl Into<String>) -> Result<T, Stop> {
        Err(Stop::Refused(why.into()))
    }

    /// The version asked for: one argument, strictly a version.
    fn version_argument(args: &[OsString]) -> Result<Version, Stop> {
        let [only] = args else { return refused("It takes the version and nothing else.") };
        match only.to_str().and_then(Version::parse) {
            Some(version) => Ok(version),
            None => refused("That isn't a version."),
        }
    }

    pub fn package_name(version: Version) -> String {
        format!("mesh-kvm_{version}_amd64.deb")
    }

    /// Copies the download in, with the user's rights to read it and root's to write the copy.
    fn bring_in(user: &User, version: Version, cache: &Cache) -> Result<(), Stop> {
        let package = package_name(version);
        let opened = {
            let _as_user = AsUser::enter(user).map_err(Stop::Refused)?;
            copy::open_download(&user.home, user.uid, version, &package).map_err(Stop::Refused)?
        };
        cache.fill_staging(opened, &package).map_err(Stop::Refused)
    }

    fn run(args: &[OsString], pkexec_uid: Option<OsString>, key: Option<VerifyingKey>, running: Version) -> Result<(), Stop> {
        if !process::is_root() {
            return refused("It has to run as root, through pkexec.");
        }
        let version = version_argument(args)?;
        let uid = process::invoking_uid(pkexec_uid.as_deref()).map_err(Stop::Refused)?;
        let user = process::look_up(uid).map_err(Stop::Refused)?;
        let cache = Cache::open(Path::new(CACHE), 0).map_err(Stop::Refused)?;
        let _lock = cache.lock().map_err(Stop::Refused)?;

        process::set_alarm(COPY_LIMIT_SECONDS);
        let brought = bring_in(&user, version, &cache);
        process::set_alarm(0);
        if let Err(stop) = brought {
            cache.clear_staging();
            return Err(stop);
        }
        let package = package_name(version);
        if let Err(why) = cache.check_staging(key.as_ref(), running, version, &package) {
            cache.clear_staging();
            return refused(why.to_string());
        }
        if let Err(why) = cache::check_package(&cache.staged(&package), version) {
            cache.clear_staging();
            return refused(why);
        }
        process::ready_for_child();
        match cache::run_apt(&cache.staged(&package)) {
            Ok(()) => {
                cache.keep(&package);
                cache.clear_staging();
                Ok(())
            }
            Err(said) => {
                cache.clear_staging();
                Err(Stop::AptFailed { said, way_back: cache.way_back() })
            }
        }
    }

    fn say(line: &str) {
        let _ = writeln!(std::io::stdout(), "{line}");
    }

    pub fn main() -> ExitCode {
        process::harden();
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        let pkexec_uid = std::env::var_os("PKEXEC_UID");
        let (key, running) = (trust::built_in_key(), trust::running_version());
        process::clear_environment();
        match run(&args, pkexec_uid, key, running) {
            Ok(()) => {
                say("done");
                ExitCode::SUCCESS
            }
            Err(Stop::Refused(why)) => {
                say(&format!("refused: {why}"));
                ExitCode::from(2)
            }
            Err(Stop::AptFailed { said, way_back }) => {
                say(&format!("apt: {said}"));
                if let Some(way_back) = way_back {
                    say(&format!("back: {way_back}"));
                }
                ExitCode::from(3)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn args(list: &[&str]) -> Vec<OsString> {
            list.iter().map(OsString::from).collect()
        }

        #[test]
        fn the_only_argument_is_a_plain_version() {
            assert_eq!(version_argument(&args(&["0.7.0"])).ok(), Version::parse("0.7.0"));
            for bad in [
                &[][..],
                &["0.7.0", "0.7.1"],
                &["../../etc/shadow"],
                &["0.7.0/../../.."],
                &["0.7.0\n"],
                &["-o", "APT::Update::Pre-Invoke=sh"],
                &["--help"],
                &[""],
                &["0.7.0 "],
                &["0.07.0"],
            ] {
                assert!(version_argument(&args(bad)).is_err(), "{bad:?}");
            }
        }

        #[test]
        fn an_argument_that_is_not_text_is_refused() {
            use std::os::unix::ffi::OsStringExt;
            assert!(version_argument(&[OsString::from_vec(vec![0xff, b'1'])]).is_err());
        }
    }
}
