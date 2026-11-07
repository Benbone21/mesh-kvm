//! Updating both devices from one click. The other device goes first, so the app on screen can
//! show the whole thing, and this one only follows once the other is back on the new version.

use super::trust::Version;
use std::time::Duration;

/// Long enough for Debian's Timeshift snapshot.
pub const COME_BACK_WITHIN: Duration = Duration::from_secs(5 * 60);
const LOOK_EVERY: Duration = Duration::from_secs(2);

/// What the other device says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerLook {
    pub name: String,
    pub version: Option<Version>,
    pub failed: Option<String>,
}

/// The step the banner shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    UpdatingPeer,
    WaitingForPeer,
}

/// What `update_both` does to the outside world.
pub trait Devices {
    /// `None` when it can't be reached.
    fn look_at_peer(&self) -> Option<PeerLook>;
    /// Asks the other device to install its checked download.
    fn ask_peer_to_install(&self) -> Result<(), String>;
    fn show(&self, step: Step);
    fn wait(&self, time: Duration);
}

/// Sees the other device onto `target`. On `Err`, this device must stay as it is, and the error
/// says which device failed.
pub fn update_peer_first(devices: &dyn Devices, target: Version) -> Result<(), String> {
    let look = devices.look_at_peer();
    if look.as_ref().is_some_and(|l| l.version.is_some_and(|v| v >= target)) {
        return Ok(());
    }
    let name = look.map_or_else(|| "The other device".to_string(), |l| l.name);
    devices.show(Step::UpdatingPeer);
    devices.ask_peer_to_install().map_err(|why| format!("{name} couldn't start updating, so this device stayed as it is. {why}"))?;
    let mut waited = Duration::ZERO;
    while waited < COME_BACK_WITHIN {
        devices.wait(LOOK_EVERY);
        waited += LOOK_EVERY;
        match devices.look_at_peer() {
            Some(look) if look.version.is_some_and(|v| v >= target) => return Ok(()),
            Some(PeerLook { failed: Some(why), .. }) => {
                return Err(format!("{name} couldn't update, so this device stayed as it is. {why}"));
            }
            Some(_) => {}
            None => devices.show(Step::WaitingForPeer),
        }
    }
    Err(format!("{name} didn't come back on version {target} within 5 minutes, so this device stayed as it is."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn on(version: &str) -> Option<PeerLook> {
        Some(PeerLook { name: "SACBOOKPRO".into(), version: Version::parse(version), failed: None })
    }

    /// Plays the other device's looks in turn, and keeps giving the last.
    #[derive(Default)]
    struct Fake {
        looks: RefCell<Vec<Option<PeerLook>>>,
        ask_fails: bool,
        log: RefCell<Vec<String>>,
        waited: Cell<Duration>,
    }

    impl Devices for Fake {
        fn look_at_peer(&self) -> Option<PeerLook> {
            let mut looks = self.looks.borrow_mut();
            if looks.len() > 1 {
                looks.remove(0)
            } else {
                looks[0].clone()
            }
        }

        fn ask_peer_to_install(&self) -> Result<(), String> {
            self.log.borrow_mut().push("asked".into());
            if self.ask_fails {
                return Err("It said no.".into());
            }
            Ok(())
        }

        fn show(&self, step: Step) {
            self.log.borrow_mut().push(format!("{step:?}"));
        }

        fn wait(&self, time: Duration) {
            self.waited.set(self.waited.get() + time);
        }
    }

    fn fake(looks: Vec<Option<PeerLook>>) -> Fake {
        Fake { looks: RefCell::new(looks), ..Default::default() }
    }

    #[test]
    fn the_other_device_goes_first_and_this_one_follows_once_it_is_back_on_the_new_version() {
        let devices = fake(vec![on("0.7.0"), on("0.7.0"), None, None, on("0.7.0"), on("0.7.1")]);
        assert_eq!(update_peer_first(&devices, v("0.7.1")), Ok(()));
        assert_eq!(*devices.log.borrow(), vec!["UpdatingPeer", "asked", "WaitingForPeer", "WaitingForPeer"]);
    }

    #[test]
    fn a_peer_already_on_the_version_is_left_alone() {
        let devices = fake(vec![on("0.7.1")]);
        assert_eq!(update_peer_first(&devices, v("0.7.1")), Ok(()));
        assert!(devices.log.borrow().is_empty());
    }

    #[test]
    fn a_peer_that_never_returns_stops_it_after_five_minutes_and_names_the_device() {
        let devices = fake(vec![on("0.7.0"), None]);
        let error = update_peer_first(&devices, v("0.7.1")).unwrap_err();
        assert!(error.starts_with("SACBOOKPRO didn't come back on version 0.7.1"), "{error}");
        assert!(error.contains("this device stayed"), "{error}");
        assert_eq!(devices.waited.get(), COME_BACK_WITHIN);
    }

    #[test]
    fn a_peer_that_comes_back_on_the_old_version_is_not_good_enough() {
        let devices = fake(vec![on("0.7.0")]);
        assert!(update_peer_first(&devices, v("0.7.1")).is_err());
    }

    #[test]
    fn a_peer_that_says_it_failed_stops_it_at_once_with_its_reason() {
        let failed = PeerLook { failed: Some("apt said no.".into()), ..on("0.7.0").unwrap() };
        let devices = fake(vec![on("0.7.0"), Some(failed)]);
        let error = update_peer_first(&devices, v("0.7.1")).unwrap_err();
        assert!(error.contains("apt said no.") && error.contains("SACBOOKPRO"), "{error}");
        assert!(devices.waited.get() < COME_BACK_WITHIN);
    }

    #[test]
    fn a_peer_that_refuses_at_the_start_changes_nothing_here() {
        let devices = Fake { ask_fails: true, ..fake(vec![on("0.7.0")]) };
        let error = update_peer_first(&devices, v("0.7.1")).unwrap_err();
        assert!(error.contains("It said no.") && error.contains("stayed"), "{error}");
    }
}
