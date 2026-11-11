//! When Wi-Fi streaming mode should be on, worked out from plain inputs, with the Wi-Fi itself
//! behind a trait so that the decisions can be tested without a wireless card.

use std::time::{Duration, Instant};

/// Streaming mode stays on this long after the link goes down, so that a short drop doesn't let
/// the scans back in just as the devices find each other again.
pub const HOLD: Duration = Duration::from_secs(30);
/// Windows turns streaming mode off by itself whenever the interface disconnects, a roam included,
/// and says nothing. Setting it again now and then puts it back without waiting for a change to be
/// seen.
pub const REAPPLY: Duration = Duration::from_secs(60);

/// One `key=value` pair after the line's word.
pub type Pairs = Vec<(&'static str, String)>;

/// What turning streaming mode on did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enabled {
    /// Connected wireless interfaces now in streaming mode.
    pub ifaces: usize,
    /// Background scans were turned off on every one of them too.
    pub bgscan_off: bool,
}

/// The Wi-Fi, as far as this needs it.
pub trait Wlan {
    /// Puts every connected wireless interface in streaming mode. Calling it again does it again.
    fn enable(&mut self) -> Result<Enabled, String>;
    /// Undoes whatever `enable` did. Safe to call when it did nothing.
    fn disable(&mut self);
}

/// What the service knows each time round.
#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub now: Instant,
    pub link_up: bool,
    /// The Low-latency Wi-Fi setting.
    pub setting: bool,
    /// Counts that go up when the link log sees the network change, or the device wake from sleep.
    pub net_changes: u64,
    pub sleeps: u64,
}

#[derive(Default)]
pub struct WifiPolicy {
    last_up: Option<Instant>,
    on: bool,
    /// When streaming mode was last asked for, and the counts then.
    tried: Option<(Instant, u64, u64)>,
    last_said: Option<Pairs>,
}

impl WifiPolicy {
    /// Does what `look` calls for. A `wifi_mode` line comes back when the state changed.
    pub fn step(&mut self, look: Look, wlan: &mut dyn Wlan) -> Option<Pairs> {
        if look.link_up {
            self.last_up = Some(look.now);
        }
        let held = self.last_up.is_some_and(|at| look.now.saturating_duration_since(at) < HOLD);
        if !(look.setting && held) {
            self.tried = None;
            if !self.on {
                return None;
            }
            wlan.disable();
            self.on = false;
            let reason = if look.setting { "link_down" } else { "setting" };
            return self.say(vec![("streaming", "off".into()), ("reason", reason.into())]);
        }
        let due = match self.tried {
            None => true,
            Some((at, net, sleeps)) => {
                net != look.net_changes || sleeps != look.sleeps || look.now.saturating_duration_since(at) >= REAPPLY
            }
        };
        if !due {
            return None;
        }
        self.tried = Some((look.now, look.net_changes, look.sleeps));
        match wlan.enable() {
            Ok(done) => {
                self.on = true;
                let bgscan = if done.bgscan_off { "off" } else { "on" };
                self.say(vec![("streaming", "on".into()), ("ifaces", done.ifaces.to_string()), ("bgscan", bgscan.into())])
            }
            Err(error) => {
                // Nothing is left half done: whatever a partial try set is undone.
                wlan.disable();
                self.on = false;
                self.say(vec![("streaming", "unavailable".into()), ("error", error)])
            }
        }
    }

    /// The service is stopping: streaming mode goes off.
    pub fn stop(&mut self, wlan: &mut dyn Wlan) -> Option<Pairs> {
        self.tried = None;
        if !self.on {
            return None;
        }
        wlan.disable();
        self.on = false;
        self.say(vec![("streaming", "off".into()), ("reason", "stop".into())])
    }

    /// The line, unless it's the one said last: a failure that keeps happening is logged once.
    fn say(&mut self, pairs: Pairs) -> Option<Pairs> {
        if self.last_said.as_ref() == Some(&pairs) {
            return None;
        }
        self.last_said = Some(pairs.clone());
        Some(pairs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeWlan {
        result: Option<Result<Enabled, String>>,
        enables: u32,
        disables: u32,
    }

    impl FakeWlan {
        fn working(ifaces: usize) -> FakeWlan {
            FakeWlan { result: Some(Ok(Enabled { ifaces, bgscan_off: true })), ..Default::default() }
        }
        fn failing(error: &str) -> FakeWlan {
            FakeWlan { result: Some(Err(error.into())), ..Default::default() }
        }
    }

    impl Wlan for FakeWlan {
        fn enable(&mut self) -> Result<Enabled, String> {
            self.enables += 1;
            self.result.clone().unwrap()
        }
        fn disable(&mut self) {
            self.disables += 1;
        }
    }

    struct Run {
        start: Instant,
        policy: WifiPolicy,
        net: u64,
        sleeps: u64,
    }

    impl Run {
        fn new() -> Run {
            Run { start: Instant::now(), policy: WifiPolicy::default(), net: 0, sleeps: 0 }
        }
        fn at(&mut self, secs: u64, link_up: bool, setting: bool, wlan: &mut FakeWlan) -> Option<String> {
            let look = Look { now: self.start + Duration::from_secs(secs), link_up, setting, net_changes: self.net, sleeps: self.sleeps };
            self.policy.step(look, wlan).map(|pairs| text(&pairs))
        }
    }

    fn text(pairs: &Pairs) -> String {
        pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn nothing_happens_before_the_link_first_comes_up() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        assert_eq!(run.at(0, false, true, &mut wlan), None);
        assert_eq!(run.at(5, false, true, &mut wlan), None);
        assert_eq!((wlan.enables, wlan.disables), (0, 0));
    }

    #[test]
    fn on_with_the_link_and_off_after_it_has_been_down_for_30_seconds() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        assert_eq!(run.at(0, true, true, &mut wlan).as_deref(), Some("streaming=on ifaces=1 bgscan=off"));
        assert_eq!(run.at(1, true, true, &mut wlan), None, "nothing new to say or do");
        assert_eq!(wlan.enables, 1);
        // Down from 2 s: still on at 31 s, which is 29 s after the link was last up.
        assert_eq!(run.at(2, false, true, &mut wlan), None);
        assert_eq!(run.at(30, false, true, &mut wlan), None);
        assert_eq!(wlan.disables, 0);
        assert_eq!(run.at(31, false, true, &mut wlan).as_deref(), Some("streaming=off reason=link_down"));
        assert_eq!(wlan.disables, 1);
        assert_eq!(run.at(40, false, true, &mut wlan), None, "off is said once");
        assert_eq!(run.at(41, true, true, &mut wlan).as_deref(), Some("streaming=on ifaces=1 bgscan=off"));
        assert_eq!(wlan.enables, 2);
    }

    #[test]
    fn a_short_drop_keeps_it_on() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        run.at(0, true, true, &mut wlan);
        assert_eq!(run.at(10, false, true, &mut wlan), None);
        assert_eq!(run.at(20, true, true, &mut wlan), None);
        assert_eq!(run.at(45, false, true, &mut wlan), None, "the hold starts again from the last time it was up");
        assert_eq!((wlan.enables, wlan.disables), (1, 0));
    }

    #[test]
    fn the_setting_turns_it_off_at_once_and_back_on() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        run.at(0, true, true, &mut wlan);
        assert_eq!(run.at(1, true, false, &mut wlan).as_deref(), Some("streaming=off reason=setting"));
        assert_eq!(run.at(2, true, false, &mut wlan), None);
        assert_eq!(wlan.enables, 1, "off means nothing is asked of the Wi-Fi");
        assert_eq!(run.at(3, true, true, &mut wlan).as_deref(), Some("streaming=on ifaces=1 bgscan=off"));
    }

    #[test]
    fn setting_off_from_the_start_never_touches_the_wifi() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        for secs in 0..100 {
            assert_eq!(run.at(secs, true, false, &mut wlan), None);
        }
        assert_eq!((wlan.enables, wlan.disables), (0, 0));
    }

    #[test]
    fn a_network_change_or_a_sleep_sets_it_again_without_a_new_line() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        run.at(0, true, true, &mut wlan);
        run.net += 1;
        assert_eq!(run.at(1, true, true, &mut wlan), None, "the same state, so nothing to log");
        assert_eq!(wlan.enables, 2);
        run.sleeps += 1;
        run.at(2, true, true, &mut wlan);
        assert_eq!(wlan.enables, 3);
        run.at(3, true, true, &mut wlan);
        assert_eq!(wlan.enables, 3, "nothing changed since");
    }

    #[test]
    fn it_is_set_again_every_minute_while_on() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        run.at(0, true, true, &mut wlan);
        run.at(59, true, true, &mut wlan);
        assert_eq!(wlan.enables, 1);
        run.at(60, true, true, &mut wlan);
        assert_eq!(wlan.enables, 2);
    }

    #[test]
    fn an_interface_change_that_brings_more_interfaces_is_logged() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        run.at(0, true, true, &mut wlan);
        wlan.result = Some(Ok(Enabled { ifaces: 2, bgscan_off: false }));
        run.net += 1;
        assert_eq!(run.at(1, true, true, &mut wlan).as_deref(), Some("streaming=on ifaces=2 bgscan=on"));
    }

    #[test]
    fn a_failure_is_logged_once_and_tried_again_on_a_change() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::failing("no connected wireless interface"));
        assert_eq!(run.at(0, true, true, &mut wlan).as_deref(), Some("streaming=unavailable error=no connected wireless interface"));
        assert_eq!(wlan.disables, 1, "a failed try is cleaned up");
        assert_eq!(run.at(1, true, true, &mut wlan), None);
        assert_eq!(wlan.enables, 1, "not tried again until something changes");
        assert_eq!(run.at(61, true, true, &mut wlan), None, "tried again a minute on, and not logged again");
        assert_eq!(wlan.enables, 2);
        // The link drops and comes back: tried again, still not logged again.
        run.at(100, false, true, &mut wlan);
        assert_eq!(run.at(200, true, true, &mut wlan), None);
        assert_eq!(wlan.enables, 3);
        assert_eq!(run.at(210, true, true, &mut wlan), None);
        // The Wi-Fi connects.
        wlan.result = Some(Ok(Enabled { ifaces: 1, bgscan_off: true }));
        run.net += 1;
        assert_eq!(run.at(211, true, true, &mut wlan).as_deref(), Some("streaming=on ifaces=1 bgscan=off"));
        // And loses it again: the error is news once more.
        wlan.result = Some(Err("no connected wireless interface".into()));
        run.net += 1;
        assert_eq!(run.at(212, true, true, &mut wlan).as_deref(), Some("streaming=unavailable error=no connected wireless interface"));
    }

    #[test]
    fn going_down_after_a_failure_has_nothing_to_undo_or_say() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::failing("access denied"));
        run.at(0, true, true, &mut wlan);
        let disables = wlan.disables;
        assert_eq!(run.at(40, false, true, &mut wlan), None);
        assert_eq!(run.at(41, false, false, &mut wlan), None);
        assert_eq!(wlan.disables, disables);
    }

    #[test]
    fn stopping_turns_it_off_once() {
        let (mut run, mut wlan) = (Run::new(), FakeWlan::working(1));
        run.at(0, true, true, &mut wlan);
        assert_eq!(run.policy.stop(&mut wlan).map(|p| text(&p)).as_deref(), Some("streaming=off reason=stop"));
        assert_eq!(run.policy.stop(&mut wlan), None);
        assert_eq!(wlan.disables, 1);
        let mut idle = WifiPolicy::default();
        assert_eq!(idle.stop(&mut wlan), None, "never on, nothing to stop");
    }
}
