#![allow(dead_code)]
//! Pure focus router: decides which device is active and where input goes.
//!
//! No I/O, no clock, no platform code. Time enters only through `now_ms` carried by
//! `PeerAlive` and `Tick` events; the router keeps the newest value it has seen.
//!
//! Coordinates (`x`, `y` in `PeerFocus`, `EdgeCrossed`, `SendFocus`, `WarpCursor`) are
//! opaque to the router: they are passed through unchanged. The caller owns any
//! coordinate conversion. `ManualSwitch` has no coordinates, so its `SendFocus`
//! carries `KEEP_CURSOR` for both, and a `PeerFocus` with `KEEP_CURSOR` emits no warp.

use std::collections::HashSet;

pub type DeviceId = u8;
pub const LINK_TIMEOUT_MS: u64 = 1350;
pub const SWITCH_RACE_MS: u64 = 200;
/// Coordinate sentinel in `SendFocus`/`PeerFocus`: the receiver must not move its cursor.
pub const KEEP_CURSOR: i32 = i32::MIN;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sharing {
    pub mouse: bool,
    pub keyboard: bool,
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub me: DeviceId,
    pub peer: DeviceId,
    pub main: DeviceId,
    pub sharing: Sharing,
    pub pointer_speed: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    LocalMove { dx: i32, dy: i32 },
    LocalButton { button: u8, pressed: bool },
    LocalWheel { dx: i32, dy: i32 },
    LocalKey { scancode: u16, pressed: bool },
    PeerMove { dx: i32, dy: i32 },
    PeerButton { button: u8, pressed: bool },
    PeerWheel { dx: i32, dy: i32 },
    PeerKey { scancode: u16, pressed: bool },
    PeerAlive { now_ms: u64 },
    PeerFocus { target: DeviceId, x: i32, y: i32 },
    PeerPanic,
    Tick { now_ms: u64 },
    ManualSwitch,
    Panic,
    SetCaptive(bool),
    EdgeCrossed { x: i32, y: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    ForwardMove { dx: i32, dy: i32 },
    ForwardButton { button: u8, pressed: bool },
    ForwardWheel { dx: i32, dy: i32 },
    ForwardKey { scancode: u16, pressed: bool },
    InjectMove { dx: i32, dy: i32 },
    InjectButton { button: u8, pressed: bool },
    InjectWheel { dx: i32, dy: i32 },
    InjectKey { scancode: u16, pressed: bool },
    SendFocus { target: DeviceId, x: i32, y: i32 },
    SendPanic,
    WarpCursor { x: i32, y: i32 },
    ReleaseHeld,
    SetBlocking { mouse: bool, keyboard: bool },
}

#[derive(Debug, Default, PartialEq)]
pub struct Routed {
    /// The backend must stop this local event from reaching the local OS.
    pub swallow: bool,
    pub actions: Vec<Action>,
}

pub struct Router {
    cfg: RouterConfig,
    active: DeviceId,
    online: bool,
    panicked: bool,
    captive: bool,
    /// Newest `now_ms` seen from any event.
    now: u64,
    last_alive: u64,
    /// Our own most recent switch: (time, target we claimed).
    last_switch: Option<(u64, DeviceId)>,
    /// Last `SetBlocking` state emitted.
    blocking: (bool, bool),
    fwd_keys: HashSet<u16>,
    /// Keys pressed while input was local; their repeats and release stay local.
    local_keys: HashSet<u16>,
    fwd_buttons: HashSet<u8>,
    carry_x: f64,
    carry_y: f64,
}

impl Router {
    pub fn new(cfg: RouterConfig) -> Self {
        let active = cfg.me;
        Router {
            cfg,
            active,
            online: false,
            panicked: false,
            captive: false,
            now: 0,
            last_alive: 0,
            last_switch: None,
            blocking: (false, false),
            fwd_keys: HashSet::new(),
            local_keys: HashSet::new(),
            fwd_buttons: HashSet::new(),
            carry_x: 0.0,
            carry_y: 0.0,
        }
    }

    pub fn active(&self) -> DeviceId {
        self.active
    }
    pub fn online(&self) -> bool {
        self.online
    }
    pub fn panicked(&self) -> bool {
        self.panicked
    }
    pub fn captive(&self) -> bool {
        self.captive
    }
    pub fn set_pointer_speed(&mut self, speed: f32) {
        self.cfg.pointer_speed = speed;
    }

    pub fn handle(&mut self, ev: Event) -> Routed {
        let mut out = Routed::default();
        let me = self.cfg.me;
        let peer = self.cfg.peer;
        match ev {
            Event::LocalMove { dx, dy } => {
                if self.forwarding(self.cfg.sharing.mouse) {
                    out.swallow = true;
                    let (fx, fy) = self.scale(dx, dy);
                    if fx != 0 || fy != 0 {
                        out.actions.push(Action::ForwardMove { dx: fx, dy: fy });
                    }
                }
            }
            Event::LocalWheel { dx, dy } => {
                if self.forwarding(self.cfg.sharing.mouse) {
                    out.swallow = true;
                    out.actions.push(Action::ForwardWheel { dx, dy });
                }
            }
            Event::LocalButton { button, pressed } => {
                let route = if pressed {
                    let f = self.forwarding(self.cfg.sharing.mouse);
                    if f {
                        self.fwd_buttons.insert(button);
                    }
                    f
                } else {
                    self.fwd_buttons.remove(&button)
                };
                if route {
                    out.swallow = true;
                    out.actions.push(Action::ForwardButton { button, pressed });
                }
            }
            Event::LocalKey { scancode, pressed } => {
                let route = if pressed {
                    if self.fwd_keys.contains(&scancode) {
                        true // repeat of a forwarded key
                    } else if self.local_keys.contains(&scancode) {
                        false // repeat of a locally held key
                    } else if self.forwarding(self.cfg.sharing.keyboard) {
                        self.fwd_keys.insert(scancode);
                        true
                    } else {
                        self.local_keys.insert(scancode);
                        false
                    }
                } else if self.fwd_keys.remove(&scancode) {
                    true
                } else {
                    self.local_keys.remove(&scancode);
                    false
                };
                if route {
                    out.swallow = true;
                    out.actions.push(Action::ForwardKey { scancode, pressed });
                }
            }
            Event::PeerMove { dx, dy } => {
                if self.injecting() {
                    out.actions.push(Action::InjectMove { dx, dy });
                }
            }
            Event::PeerWheel { dx, dy } => {
                if self.injecting() {
                    out.actions.push(Action::InjectWheel { dx, dy });
                }
            }
            Event::PeerButton { button, pressed } => {
                if self.injecting() {
                    out.actions.push(Action::InjectButton { button, pressed });
                }
            }
            Event::PeerKey { scancode, pressed } => {
                if self.injecting() {
                    out.actions.push(Action::InjectKey { scancode, pressed });
                }
            }
            Event::PeerAlive { now_ms } => {
                // A gap longer than the timeout is a loss followed by a fresh link-up.
                if self.online && now_ms.saturating_sub(self.last_alive) > LINK_TIMEOUT_MS {
                    self.link_lost(&mut out.actions);
                }
                self.now = self.now.max(now_ms);
                self.last_alive = self.last_alive.max(now_ms);
                if !self.online {
                    self.online = true;
                    if !self.panicked {
                        let main = self.cfg.main;
                        self.set_active(main, &mut out.actions);
                    }
                }
            }
            Event::Tick { now_ms } => {
                self.now = self.now.max(now_ms);
                if self.online && self.now.saturating_sub(self.last_alive) > LINK_TIMEOUT_MS {
                    self.link_lost(&mut out.actions);
                }
            }
            Event::PeerFocus { target, x, y } => {
                let known = target == me || target == peer;
                if self.online && known && !(self.conflicts(target) && me == self.cfg.main) {
                    self.panicked = false;
                    self.last_switch = None;
                    self.set_active(target, &mut out.actions);
                    if target == me && !(x == KEEP_CURSOR && y == KEEP_CURSOR) {
                        out.actions.push(Action::WarpCursor { x, y });
                    }
                }
            }
            Event::EdgeCrossed { x, y } => {
                if self.online
                    && !self.panicked
                    && !self.captive
                    && self.cfg.sharing.mouse
                    && self.active == me
                {
                    self.own_switch(peer, x, y, &mut out.actions);
                }
            }
            Event::ManualSwitch => {
                self.panicked = false;
                if self.online {
                    let target = if self.active == me { peer } else { me };
                    self.own_switch(target, KEEP_CURSOR, KEEP_CURSOR, &mut out.actions);
                }
            }
            Event::SetCaptive(c) => self.captive = c,
            Event::Panic => {
                self.enter_panic(&mut out.actions);
                out.actions.push(Action::SendPanic);
            }
            Event::PeerPanic => self.enter_panic(&mut out.actions),
        }
        self.sync_blocking(&mut out.actions);
        out
    }

    /// Local input of this kind goes to the peer.
    fn forwarding(&self, shared: bool) -> bool {
        shared && self.online && !self.panicked && self.active != self.cfg.me
    }

    /// Peer input is injected here. The peer decides what to forward, so this device's
    /// own sharing flags play no part.
    fn injecting(&self) -> bool {
        self.online && !self.panicked && self.active == self.cfg.me
    }

    fn scale(&mut self, dx: i32, dy: i32) -> (i32, i32) {
        let s = self.cfg.pointer_speed as f64;
        self.carry_x += dx as f64 * s;
        self.carry_y += dy as f64 * s;
        let fx = self.carry_x.trunc();
        let fy = self.carry_y.trunc();
        self.carry_x -= fx;
        self.carry_y -= fy;
        (fx as i32, fy as i32)
    }

    /// Peer claims `target` while we made a different claim of our own within the race window.
    fn conflicts(&self, target: DeviceId) -> bool {
        match self.last_switch {
            Some((t, mine)) => {
                mine != target && self.now.saturating_sub(t) <= SWITCH_RACE_MS
            }
            None => false,
        }
    }

    fn set_active(&mut self, target: DeviceId, actions: &mut Vec<Action>) {
        if target == self.active {
            return;
        }
        if self.active == self.cfg.me {
            // Losing focus: whatever the peer injected here must be let go.
            actions.push(Action::ReleaseHeld);
        }
        self.active = target;
        self.carry_x = 0.0;
        self.carry_y = 0.0;
    }

    fn own_switch(&mut self, target: DeviceId, x: i32, y: i32, actions: &mut Vec<Action>) {
        self.set_active(target, actions);
        self.last_switch = Some((self.now, target));
        actions.push(Action::SendFocus { target, x, y });
    }

    fn link_lost(&mut self, actions: &mut Vec<Action>) {
        self.online = false;
        self.active = self.cfg.me;
        self.last_switch = None;
        self.reset_held();
        actions.push(Action::ReleaseHeld);
    }

    fn enter_panic(&mut self, actions: &mut Vec<Action>) {
        self.panicked = true;
        self.active = self.cfg.me;
        self.last_switch = None;
        self.reset_held();
        actions.push(Action::ReleaseHeld);
    }

    fn reset_held(&mut self) {
        self.fwd_keys.clear();
        self.fwd_buttons.clear();
        self.carry_x = 0.0;
        self.carry_y = 0.0;
    }

    fn sync_blocking(&mut self, actions: &mut Vec<Action>) {
        let remote = self.online && !self.panicked && self.active != self.cfg.me;
        let want = (remote && self.cfg.sharing.mouse, remote && self.cfg.sharing.keyboard);
        if want != self.blocking {
            self.blocking = want;
            actions.push(Action::SetBlocking { mouse: want.0, keyboard: want.1 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(me: DeviceId) -> RouterConfig {
        RouterConfig {
            me,
            peer: 1 - me,
            main: 0,
            sharing: Sharing { mouse: true, keyboard: true },
            pointer_speed: 1.0,
        }
    }
    fn router(me: DeviceId) -> Router {
        Router::new(cfg(me))
    }
    fn online(r: &mut Router) -> Routed {
        r.handle(Event::PeerAlive { now_ms: 1000 })
    }
    fn key(r: &mut Router, scancode: u16, pressed: bool) -> Routed {
        r.handle(Event::LocalKey { scancode, pressed })
    }
    /// Router 0 (main) online and forwarding to device 1.
    fn forwarding_router() -> Router {
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::EdgeCrossed { x: 5, y: 6 });
        assert_eq!(r.active(), 1);
        // Move past the race window so later peer claims are not treated as conflicts.
        r.handle(Event::Tick { now_ms: 1000 + SWITCH_RACE_MS + 1 });
        r
    }

    #[test]
    fn starts_local_and_offline() {
        let mut r = router(0);
        assert_eq!(r.active(), 0);
        assert!(!r.online());
        let out = key(&mut r, 30, true);
        assert!(!out.swallow);
        assert!(out.actions.is_empty());
    }

    #[test]
    fn link_up_makes_main_device_active() {
        let mut r = router(1);
        let out = online(&mut r);
        assert!(r.online());
        assert_eq!(r.active(), 0);
        assert!(out.actions.contains(&Action::SetBlocking { mouse: true, keyboard: true }));
    }

    #[test]
    fn link_up_on_main_stays_unblocked() {
        let mut r = router(0);
        let out = online(&mut r);
        assert_eq!(r.active(), 0);
        assert!(out.actions.is_empty());
    }

    #[test]
    fn shared_input_forwards_to_remote_active() {
        let mut r = router(1);
        online(&mut r);
        let k = key(&mut r, 30, true);
        assert!(k.swallow);
        assert_eq!(k.actions, vec![Action::ForwardKey { scancode: 30, pressed: true }]);
        let m = r.handle(Event::LocalMove { dx: 3, dy: -2 });
        assert!(m.swallow);
        assert_eq!(m.actions, vec![Action::ForwardMove { dx: 3, dy: -2 }]);
        let w = r.handle(Event::LocalWheel { dx: 0, dy: 1 });
        assert!(w.swallow);
        assert_eq!(w.actions, vec![Action::ForwardWheel { dx: 0, dy: 1 }]);
    }

    #[test]
    fn unshared_input_stays_local() {
        let mut c = cfg(1);
        c.sharing = Sharing { mouse: true, keyboard: false };
        let mut r = Router::new(c);
        online(&mut r);
        let k = key(&mut r, 30, true);
        assert!(!k.swallow);
        assert!(k.actions.is_empty());
        assert_eq!(r.active(), 0);
        assert!(r.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);

        let mut c = cfg(1);
        c.sharing = Sharing { mouse: false, keyboard: true };
        let mut r = Router::new(c);
        let out = online(&mut r);
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: true }));
        let m = r.handle(Event::LocalMove { dx: 1, dy: 1 });
        assert!(!m.swallow);
        assert!(m.actions.is_empty());
        let b = r.handle(Event::LocalButton { button: 1, pressed: true });
        assert!(!b.swallow);
        assert!(b.actions.is_empty());
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn typing_never_changes_active() {
        let mut r = forwarding_router();
        for sc in [1u16, 2, 3, 56, 29] {
            for pressed in [true, false] {
                let out = key(&mut r, sc, pressed);
                assert!(!out.actions.iter().any(|a| matches!(a, Action::SendFocus { .. })));
            }
        }
        assert_eq!(r.active(), 1);
        let mut r = router(0);
        online(&mut r);
        let out = key(&mut r, 30, true);
        assert!(out.actions.is_empty());
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn peer_input_injects_only_when_active() {
        let mut r = router(0);
        online(&mut r);
        let out = r.handle(Event::PeerKey { scancode: 30, pressed: true });
        assert_eq!(out.actions, vec![Action::InjectKey { scancode: 30, pressed: true }]);
        assert_eq!(
            r.handle(Event::PeerMove { dx: 2, dy: 3 }).actions,
            vec![Action::InjectMove { dx: 2, dy: 3 }]
        );
        assert_eq!(
            r.handle(Event::PeerButton { button: 1, pressed: true }).actions,
            vec![Action::InjectButton { button: 1, pressed: true }]
        );
        assert_eq!(
            r.handle(Event::PeerWheel { dx: 0, dy: 1 }).actions,
            vec![Action::InjectWheel { dx: 0, dy: 1 }]
        );
        r.handle(Event::EdgeCrossed { x: 0, y: 0 });
        assert!(r.handle(Event::PeerKey { scancode: 30, pressed: false }).actions.is_empty());
        assert!(r.handle(Event::PeerMove { dx: 1, dy: 1 }).actions.is_empty());
    }

    #[test]
    fn key_up_follows_key_down() {
        let mut r = forwarding_router();
        assert!(key(&mut r, 30, true).swallow);
        r.handle(Event::PeerFocus { target: 0, x: 10, y: 10 });
        assert_eq!(r.active(), 0);
        let up = key(&mut r, 30, false);
        assert!(up.swallow);
        assert_eq!(up.actions, vec![Action::ForwardKey { scancode: 30, pressed: false }]);
    }

    #[test]
    fn locally_pressed_key_releases_locally() {
        let mut r = router(0);
        online(&mut r);
        assert!(!key(&mut r, 30, true).swallow);
        r.handle(Event::EdgeCrossed { x: 0, y: 0 });
        assert_eq!(r.active(), 1);
        let up = key(&mut r, 30, false);
        assert!(!up.swallow);
        assert!(up.actions.is_empty());
    }

    #[test]
    fn button_up_follows_button_down() {
        let mut r = forwarding_router();
        let down = r.handle(Event::LocalButton { button: 1, pressed: true });
        assert!(down.swallow);
        r.handle(Event::PeerFocus { target: 0, x: 0, y: 0 });
        let up = r.handle(Event::LocalButton { button: 1, pressed: false });
        assert!(up.swallow);
        assert_eq!(up.actions, vec![Action::ForwardButton { button: 1, pressed: false }]);

        let mut r = router(0);
        online(&mut r);
        assert!(!r.handle(Event::LocalButton { button: 1, pressed: true }).swallow);
        r.handle(Event::EdgeCrossed { x: 0, y: 0 });
        let up = r.handle(Event::LocalButton { button: 1, pressed: false });
        assert!(!up.swallow);
        assert!(up.actions.is_empty());
    }

    #[test]
    fn switch_releases_held_on_losing_device() {
        let mut r = router(0);
        online(&mut r);
        let out = r.handle(Event::EdgeCrossed { x: 7, y: 9 });
        assert!(out.actions.contains(&Action::ReleaseHeld));
        assert!(out.actions.contains(&Action::SendFocus { target: 1, x: 7, y: 9 }));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: true, keyboard: true }));
        // Losing focus to the peer's claim also releases.
        let mut r = router(0);
        online(&mut r);
        let out = r.handle(Event::PeerFocus { target: 1, x: 0, y: 0 });
        assert_eq!(r.active(), 1);
        assert!(out.actions.contains(&Action::ReleaseHeld));
    }

    #[test]
    fn peer_focus_warps_with_coordinates_unchanged() {
        let mut r = forwarding_router();
        let out = r.handle(Event::PeerFocus { target: 0, x: -3, y: 1079 });
        assert_eq!(r.active(), 0);
        assert!(out.actions.contains(&Action::WarpCursor { x: -3, y: 1079 }));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
    }

    #[test]
    fn link_loss_restores_local_control() {
        let mut r = forwarding_router();
        let out = r.handle(Event::Tick { now_ms: 1000 + 1351 });
        assert!(!r.online());
        assert_eq!(r.active(), 0);
        assert!(out.actions.contains(&Action::ReleaseHeld));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
        assert!(!key(&mut r, 30, true).swallow);
        assert!(r.handle(Event::PeerKey { scancode: 1, pressed: true }).actions.is_empty());
    }

    #[test]
    fn link_loss_on_non_main_becomes_active_for_itself() {
        let mut r = router(1);
        online(&mut r);
        r.handle(Event::Tick { now_ms: 1000 + 1351 });
        assert_eq!(r.active(), 1);
        assert!(!key(&mut r, 30, true).swallow);
    }

    #[test]
    fn link_survives_within_timeout() {
        let mut r = forwarding_router();
        let out = r.handle(Event::Tick { now_ms: 1000 + 1350 });
        assert!(r.online());
        assert!(out.actions.is_empty());
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn alive_refreshes_timeout() {
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::PeerAlive { now_ms: 2000 });
        r.handle(Event::Tick { now_ms: 2000 + 1350 });
        assert!(r.online());
        r.handle(Event::Tick { now_ms: 2000 + 1351 });
        assert!(!r.online());
    }

    #[test]
    fn panic_stops_forwarding_until_manual_switch() {
        let mut r = forwarding_router();
        let out = r.handle(Event::Panic);
        assert!(out.actions.contains(&Action::SendPanic));
        assert!(out.actions.contains(&Action::ReleaseHeld));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
        assert!(r.panicked());
        assert_eq!(r.active(), 0);
        assert!(!key(&mut r, 30, true).swallow);
        // Edge crossing and peer injection are ignored while panicked.
        assert!(r.handle(Event::EdgeCrossed { x: 0, y: 0 }).actions.is_empty());
        assert!(r.handle(Event::PeerKey { scancode: 1, pressed: true }).actions.is_empty());
        let out = r.handle(Event::ManualSwitch);
        assert!(!r.panicked());
        // Online: switches as normal.
        assert_eq!(r.active(), 1);
        assert!(out.actions.contains(&Action::SendFocus { target: 1, x: KEEP_CURSOR, y: KEEP_CURSOR }));
    }

    #[test]
    fn panic_cleared_by_peer_focus() {
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::Panic);
        let out = r.handle(Event::PeerFocus { target: 0, x: 4, y: 5 });
        assert!(!r.panicked());
        assert!(out.actions.contains(&Action::WarpCursor { x: 4, y: 5 }));
        assert_eq!(
            r.handle(Event::PeerKey { scancode: 1, pressed: true }).actions,
            vec![Action::InjectKey { scancode: 1, pressed: true }]
        );
    }

    #[test]
    fn panic_works_offline() {
        let mut r = router(0);
        let out = r.handle(Event::Panic);
        assert!(r.panicked());
        assert!(!out.actions.iter().any(|a| matches!(a, Action::SetBlocking { .. })));
        assert!(!key(&mut r, 30, true).swallow);
    }

    #[test]
    fn peer_panic_releases_input() {
        let mut r = forwarding_router();
        let out = r.handle(Event::PeerPanic);
        assert!(r.panicked());
        assert_eq!(r.active(), 0);
        assert!(!out.actions.contains(&Action::SendPanic));
        assert!(out.actions.contains(&Action::ReleaseHeld));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
        assert!(!key(&mut r, 30, true).swallow);
    }

    #[test]
    fn captive_blocks_edge_crossing() {
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::SetCaptive(true));
        assert!(r.captive());
        let out = r.handle(Event::EdgeCrossed { x: 0, y: 0 });
        assert!(!out.actions.iter().any(|a| matches!(a, Action::SendFocus { .. })));
        assert_eq!(r.active(), 0);
        r.handle(Event::SetCaptive(false));
        r.handle(Event::EdgeCrossed { x: 0, y: 0 });
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn manual_switch_needs_link() {
        let mut r = router(0);
        let out = r.handle(Event::ManualSwitch);
        assert_eq!(r.active(), 0);
        assert!(out.actions.is_empty());
    }

    #[test]
    fn manual_switch_toggles_when_online() {
        let mut r = router(1);
        online(&mut r);
        assert_eq!(r.active(), 0);
        let out = r.handle(Event::ManualSwitch);
        assert_eq!(r.active(), 1);
        assert!(out.actions.contains(&Action::SendFocus { target: 1, x: KEEP_CURSOR, y: KEEP_CURSOR }));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
    }

    fn focus_of(out: &Routed) -> Option<Event> {
        out.actions.iter().find_map(|a| match a {
            Action::SendFocus { target, x, y } => Some(Event::PeerFocus { target: *target, x: *x, y: *y }),
            _ => None,
        })
    }

    #[test]
    fn conflicting_switches_settle_on_main_claim() {
        let mut main = router(0);
        let mut other = router(1);
        online(&mut main);
        online(&mut other);
        // Views disagree: `other` earlier accepted a claim for device 1 that `main` never saw.
        other.handle(Event::PeerFocus { target: 1, x: 0, y: 0 });
        assert_eq!((main.active(), other.active()), (0, 1));
        // Both switch within the race window with opposite targets.
        let from_main = main.handle(Event::EdgeCrossed { x: 1, y: 2 });
        let from_other = other.handle(Event::ManualSwitch);
        assert_eq!((main.active(), other.active()), (1, 0));
        main.handle(focus_of(&from_other).unwrap());
        other.handle(focus_of(&from_main).unwrap());
        assert_eq!(main.active(), 1);
        assert_eq!(other.active(), 1);
    }

    #[test]
    fn claim_outside_race_window_is_accepted_by_main() {
        let mut main = router(0);
        online(&mut main);
        main.handle(Event::EdgeCrossed { x: 0, y: 0 });
        main.handle(Event::Tick { now_ms: 1000 + SWITCH_RACE_MS + 1 });
        main.handle(Event::PeerFocus { target: 0, x: 0, y: 0 });
        assert_eq!(main.active(), 0);
    }

    #[test]
    fn claim_at_race_boundary_still_conflicts() {
        let mut main = router(0);
        online(&mut main);
        main.handle(Event::EdgeCrossed { x: 0, y: 0 });
        main.handle(Event::Tick { now_ms: 1000 + SWITCH_RACE_MS });
        main.handle(Event::PeerFocus { target: 0, x: 0, y: 0 });
        assert_eq!(main.active(), 1);
    }

    #[test]
    fn pointer_speed_scales_with_carry() {
        let mut c = cfg(1);
        c.pointer_speed = 0.5;
        let mut r = Router::new(c);
        online(&mut r);
        let mut total = 0;
        for _ in 0..2 {
            let out = r.handle(Event::LocalMove { dx: 1, dy: 0 });
            assert!(out.swallow);
            for a in out.actions {
                if let Action::ForwardMove { dx, .. } = a {
                    total += dx;
                }
            }
        }
        assert_eq!(total, 1);
        r.set_pointer_speed(2.0);
        let out = r.handle(Event::LocalMove { dx: 1, dy: 1 });
        assert_eq!(out.actions, vec![Action::ForwardMove { dx: 2, dy: 2 }]);
    }

    #[test]
    fn peer_input_injects_even_if_local_sharing_is_off() {
        let mut c = cfg(0);
        c.sharing = Sharing { mouse: false, keyboard: false };
        let mut r = Router::new(c);
        online(&mut r);
        assert_eq!(
            r.handle(Event::PeerKey { scancode: 30, pressed: true }).actions,
            vec![Action::InjectKey { scancode: 30, pressed: true }]
        );
        assert_eq!(
            r.handle(Event::PeerMove { dx: 1, dy: 2 }).actions,
            vec![Action::InjectMove { dx: 1, dy: 2 }]
        );
    }

    #[test]
    fn manual_switch_keeps_receiver_cursor() {
        let mut r = router(1);
        online(&mut r);
        let out = r.handle(Event::ManualSwitch);
        assert!(out.actions.contains(&Action::SendFocus { target: 1, x: KEEP_CURSOR, y: KEEP_CURSOR }));
        let mut m = router(0);
        online(&mut m);
        let out = m.handle(Event::PeerFocus { target: 0, x: KEEP_CURSOR, y: KEEP_CURSOR });
        assert_eq!(m.active(), 0);
        assert!(!out.actions.iter().any(|a| matches!(a, Action::WarpCursor { .. })));
    }

    #[test]
    fn key_repeat_of_locally_held_key_stays_local() {
        let mut r = router(0);
        online(&mut r);
        assert!(!key(&mut r, 30, true).swallow);
        r.handle(Event::EdgeCrossed { x: 0, y: 0 });
        assert_eq!(r.active(), 1);
        let rep = key(&mut r, 30, true);
        assert!(!rep.swallow);
        assert!(rep.actions.is_empty());
        let up = key(&mut r, 30, false);
        assert!(!up.swallow);
        assert!(up.actions.is_empty());
        // After release, a fresh press forwards normally.
        assert!(key(&mut r, 30, true).swallow);
    }

    #[test]
    fn key_repeat_of_forwarded_key_stays_forwarded() {
        let mut r = forwarding_router();
        assert!(key(&mut r, 30, true).swallow);
        r.handle(Event::PeerFocus { target: 0, x: 0, y: 0 });
        assert_eq!(r.active(), 0);
        let rep = key(&mut r, 30, true);
        assert!(rep.swallow);
        assert_eq!(rep.actions, vec![Action::ForwardKey { scancode: 30, pressed: true }]);
        let up = key(&mut r, 30, false);
        assert!(up.swallow);
        assert_eq!(up.actions, vec![Action::ForwardKey { scancode: 30, pressed: false }]);
        assert!(!key(&mut r, 30, true).swallow);
    }
}
