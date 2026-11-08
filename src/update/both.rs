//! Updating both devices from one click. The other device goes first, so the app on screen can
//! show the whole thing, and this one only follows once the other is back on the new version.

use super::trust::Version;
use std::time::Duration;

/// Long enough for Debian's Timeshift snapshot.
pub const COME_BACK_WITHIN: Duration = Duration::from_secs(5 * 60);
const LOOK_EVERY: Duration = Duration::from_secs(2);
/// With no clear answer, how long the other device gets to show that it heard.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(20);
/// How long the other device has to keep answering on the new version before this one follows.
/// A new copy that answers once and then dies hasn't arrived.
pub const STEADY_FOR: Duration = Duration::from_secs(10);

/// What the other device says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerLook {
    pub name: String,
    pub version: Option<Version>,
    pub failed: Option<String>,
    /// It's on a new version that hasn't proved itself, and could still go back.
    pub proving: bool,
    /// An update is under way there.
    pub updating: bool,
}

/// How asking the other device to install went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asked {
    Accepted,
    Refused(String),
    /// No clear answer, which on bad Wi-Fi doesn't mean it wasn't heard.
    Unclear,
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
    fn ask_peer_to_install(&self) -> Asked;
    fn show(&self, step: Step);
    fn wait(&self, time: Duration);
}

/// Sees the other device onto `target`. On `Err`, this device must stay as it is, and the error
/// says which device failed.
pub fn update_peer_first(devices: &dyn Devices, target: Version) -> Result<(), String> {
    let on_target = |look: &PeerLook| look.version.is_some_and(|v| v >= target);
    let look = devices.look_at_peer();
    if look.as_ref().is_some_and(|l| on_target(l) && !l.proving) {
        return Ok(());
    }
    let already_there = look.as_ref().is_some_and(on_target);
    let name = look.map_or_else(|| "The other device".to_string(), |l| l.name);
    let ask = || match devices.ask_peer_to_install() {
        Asked::Refused(why) => Err(format!("{name} couldn't start updating, so this device stayed as it is. {why}")),
        Asked::Accepted => Ok(false),
        Asked::Unclear => Ok(true),
    };
    let mut unclear = false;
    if already_there {
        devices.show(Step::WaitingForPeer);
    } else {
        devices.show(Step::UpdatingPeer);
        unclear = ask()?;
    }
    let mut waited = Duration::ZERO;
    // When it was first seen there, in this unbroken run of looks.
    let mut arrived_at = None;
    while waited < COME_BACK_WITHIN {
        devices.wait(LOOK_EVERY);
        waited += LOOK_EVERY;
        let look = devices.look_at_peer();
        if !look.as_ref().is_some_and(|l| on_target(l) && !l.proving) {
            arrived_at = None;
        }
        match look {
            None => devices.show(Step::WaitingForPeer),
            Some(look) if on_target(&look) && !look.proving => {
                let since = *arrived_at.get_or_insert(waited);
                if waited - since >= STEADY_FOR {
                    return Ok(());
                }
            }
            // It could still go back, so it hasn't arrived yet.
            Some(look) if on_target(&look) => devices.show(Step::WaitingForPeer),
            Some(look) if look.updating => unclear = false,
            // With no clear answer yet, a failure on show may be from some earlier try.
            Some(PeerLook { failed: Some(why), .. }) if !unclear => {
                return Err(format!("{name} couldn't update, so this device stayed as it is. {why}"));
            }
            Some(_) if unclear && waited >= ASK_AGAIN_AFTER => {
                ask()?;
                unclear = false;
            }
            Some(_) => {}
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

    fn proving(version: &str) -> Option<PeerLook> {
        Some(PeerLook { proving: true, ..on(version).unwrap() })
    }

    fn updating(version: &str) -> Option<PeerLook> {
        Some(PeerLook { updating: true, ..on(version).unwrap() })
    }

    fn on(version: &str) -> Option<PeerLook> {
        Some(PeerLook { name: "SACBOOKPRO".into(), version: Version::parse(version), ..Default::default() })
    }

    /// Plays the other device's looks in turn, and keeps giving the last.
    #[derive(Default)]
    struct Fake {
        looks: RefCell<Vec<Option<PeerLook>>>,
        ask_fails: bool,
        /// Answers in turn. With none left, it's accepted.
        answers: RefCell<Vec<Asked>>,
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

        fn ask_peer_to_install(&self) -> Asked {
            self.log.borrow_mut().push("asked".into());
            if self.ask_fails {
                return Asked::Refused("It said no.".into());
            }
            let mut answers = self.answers.borrow_mut();
            if answers.is_empty() {
                Asked::Accepted
            } else {
                answers.remove(0)
            }
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
    fn a_peer_on_the_new_version_is_only_followed_once_it_has_proved_itself() {
        let devices = fake(vec![on("0.7.0"), on("0.7.0"), proving("0.7.1"), proving("0.7.1"), on("0.7.1")]);
        assert_eq!(update_peer_first(&devices, v("0.7.1")), Ok(()));
        assert_eq!(devices.waited.get(), LOOK_EVERY * 4 + STEADY_FOR);
    }

    #[test]
    fn a_peer_that_answers_on_the_new_version_and_then_dies_is_not_followed() {
        let devices = fake(vec![on("0.7.0"), on("0.7.0"), on("0.7.1"), None]);
        let error = update_peer_first(&devices, v("0.7.1")).unwrap_err();
        assert!(error.contains("didn't come back on version 0.7.1") && error.contains("stayed"), "{error}");
    }

    #[test]
    fn a_peer_that_keeps_dropping_out_on_the_new_version_is_not_followed() {
        let mut looks = vec![on("0.7.0")];
        for _ in 0..200 {
            looks.extend([on("0.7.1"), on("0.7.1"), None]);
        }
        let devices = fake(looks);
        assert!(update_peer_first(&devices, v("0.7.1")).is_err());
    }

    #[test]
    fn a_peer_must_keep_answering_on_the_new_version_for_a_while_first() {
        let devices = fake(vec![on("0.7.0"), on("0.7.1"), None, on("0.7.1")]);
        assert_eq!(update_peer_first(&devices, v("0.7.1")), Ok(()));
        assert_eq!(devices.waited.get(), LOOK_EVERY * 3 + STEADY_FOR, "the run started again after the gap");
    }

    #[test]
    fn a_peer_that_goes_back_while_proving_stops_it() {
        let back = PeerLook { failed: Some("Version 0.7.1 didn't start properly.".into()), ..on("0.7.0").unwrap() };
        let devices = fake(vec![on("0.7.0"), proving("0.7.1"), Some(back)]);
        let error = update_peer_first(&devices, v("0.7.1")).unwrap_err();
        assert!(error.contains("didn't start properly"), "{error}");
    }

    #[test]
    fn a_peer_found_still_proving_is_waited_for_and_not_asked_again() {
        let devices = fake(vec![proving("0.7.1"), proving("0.7.1"), on("0.7.1")]);
        assert_eq!(update_peer_first(&devices, v("0.7.1")), Ok(()));
        assert!(!devices.log.borrow().contains(&"asked".to_string()));
    }

    #[test]
    fn no_clear_answer_means_looking_at_the_peer_and_not_giving_up() {
        let devices = Fake { answers: RefCell::new(vec![Asked::Unclear]), ..fake(vec![on("0.7.0"), updating("0.7.0"), None, on("0.7.1")]) };
        assert_eq!(update_peer_first(&devices, v("0.7.1")), Ok(()));
        assert_eq!(devices.log.borrow().iter().filter(|l| *l == "asked").count(), 1, "it had heard");
    }

    #[test]
    fn a_peer_that_did_not_hear_is_asked_once_more() {
        let devices = Fake { answers: RefCell::new(vec![Asked::Unclear]), ..fake(vec![on("0.7.0")]) };
        assert!(update_peer_first(&devices, v("0.7.1")).is_err());
        assert_eq!(devices.log.borrow().iter().filter(|l| *l == "asked").count(), 2);
    }

    #[test]
    fn an_old_failure_does_not_count_until_the_peer_has_clearly_heard() {
        let old = PeerLook { failed: Some("From last week.".into()), ..on("0.7.0").unwrap() };
        let devices = Fake { answers: RefCell::new(vec![Asked::Unclear]), ..fake(vec![Some(old.clone()), Some(old), updating("0.7.0"), on("0.7.1")]) };
        assert_eq!(update_peer_first(&devices, v("0.7.1")), Ok(()));
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
