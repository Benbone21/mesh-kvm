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

use crate::protocol::{HeldInput, SwitchEdge};

pub type DeviceId = u8;
pub const LINK_TIMEOUT_MS: u64 = 1350;
pub const SWITCH_RACE_MS: u64 = 200;
/// How often the main device repeats its view of the active device.
pub const ACTIVE_VIEW_INTERVAL_MS: u64 = 1000;
/// How long a non-main device keeps control with main after main's last view. Main sends one
/// a second and only while it hears this device, so silence means forwarding may reach nobody.
pub const VIEW_TIMEOUT_MS: u64 = 2500;
/// How long a main device tolerates views from a peer that also claims main. A handover applied
/// to one device before the other looks like this for a moment. Past it, the two have a split.
pub const SPLIT_GRACE_MS: u64 = 3000;
/// Coordinate sentinel in `SendFocus`/`PeerFocus`: the receiver must not move its cursor.
pub const KEEP_CURSOR: i32 = i32::MIN;
/// How long after a dropout control still goes back to the device that had it. Past this the
/// person will have settled on the device in front of them.
pub const RESUME_WINDOW_MS: u64 = 15_000;

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
    /// What the peer says it has forwarded and not released. Corrects a press or release that got
    /// lost. Must be newer than any key or button event already handled.
    PeerHeld(HeldInput),
    /// The peer answered a heartbeat of ours sent at `sent_ms`, so it hears us. Sent only by a
    /// peer that can, and `panicked` says it has released all input.
    PeerHeard { sent_ms: u64, panicked: bool },
    PeerFocus { target: DeviceId, x: i32, y: i32 },
    /// The main device's view of the active device. Repairs a desync, so it never warps.
    PeerActiveView { active: DeviceId },
    /// The peer's manual game lock: refuses crossings like `SetCaptive`, tracked separately.
    PeerGameLock(bool),
    PeerPanic,
    Tick { now_ms: u64 },
    ManualSwitch,
    Panic,
    SetCaptive(bool),
    /// The cursor went through the shared edge. `from_peer` means the peer's input drove it
    /// there, so this device's own mouse sharing has no say: refusing would trap the peer's mouse.
    EdgeCrossed { x: i32, y: i32, from_peer: bool },
    /// Live change of what this device shares. Focus stays put, and panic and link rules still
    /// win. Input of a kind turned off is let go on the peer and stays local from then on.
    SetSharing(Sharing),
    /// Live change of the main device. An unknown device is ignored. Any real change ends a
    /// main conflict.
    SetMain(DeviceId),
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
    SendActiveView { active: DeviceId },
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
    peer_game_lock: bool,
    /// Newest `now_ms` seen from any event.
    now: u64,
    last_alive: u64,
    /// When this device last sent `SendActiveView`.
    last_view: u64,
    /// When a non-main device last heard from main that it hears us. `None` until the first
    /// view, and after a fallback.
    view_heard: Option<u64>,
    /// While this device is main: the first and latest of an unbroken run of peer views.
    peer_views: Option<(u64, u64)>,
    /// Both devices claim main, past the grace period. Views no longer settle who is active, so
    /// this device stays local, refuses focus both ways and injects nothing until that ends.
    /// Only ever true on a device that is main, and `active` stays `me` throughout.
    main_conflict: bool,
    /// Our own most recent switch: (time, target we claimed).
    last_switch: Option<(u64, DeviceId)>,
    /// Last `SetBlocking` state emitted.
    blocking: (bool, bool),
    fwd_keys: HashSet<u16>,
    /// Keys pressed while input was local; their repeats and release stay local.
    local_keys: HashSet<u16>,
    fwd_buttons: HashSet<u8>,
    /// What the peer's input has pressed here, as far as it's still down.
    injected: HeldInput,
    /// What the peer's last report held that isn't down here yet.
    unconfirmed: HeldInput,
    /// A dropout took control from the peer, and it goes back up to this time.
    resume_until: Option<u64>,
    /// Only an answer to a heartbeat sent after this time proves the way back works.
    proof_after: u64,
    /// When the newest heartbeat the peer has answered was sent. `None` for a peer that doesn't
    /// answer, or hasn't since the link came up.
    answered: Option<u64>,
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
            peer_game_lock: false,
            now: 0,
            last_alive: 0,
            last_view: 0,
            view_heard: None,
            peer_views: None,
            main_conflict: false,
            last_switch: None,
            blocking: (false, false),
            fwd_keys: HashSet::new(),
            local_keys: HashSet::new(),
            fwd_buttons: HashSet::new(),
            injected: HeldInput::default(),
            unconfirmed: HeldInput::default(),
            resume_until: None,
            proof_after: 0,
            answered: None,
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
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn sharing(&self) -> Sharing {
        self.cfg.sharing
    }
    pub fn main(&self) -> DeviceId {
        self.cfg.main
    }
    pub fn main_conflict(&self) -> bool {
        self.main_conflict
    }
    pub fn captive(&self) -> bool {
        self.captive
    }
    /// What has been forwarded and not released, for the peer to check against.
    pub fn held_forwarded(&self) -> HeldInput {
        let mut held = HeldInput::default();
        self.fwd_keys.iter().for_each(|key| held.set_key(*key, true));
        self.fwd_buttons.iter().for_each(|button| held.set_button(*button, true));
        held
    }
    /// How many keys and buttons the peer's input has down here.
    pub fn held_here(&self) -> u32 {
        (self.injected.keys().count() + self.injected.buttons().count()) as u32
    }
    /// Control went to this device with a dropout, and goes back when the link returns.
    pub fn returning(&self) -> bool {
        self.resume_until.is_some()
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
                    self.unconfirmed = self.unconfirmed.without_buttons();
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
                    self.injected.set_button(button, pressed);
                    self.unconfirmed.set_button(button, false);
                    out.actions.push(Action::InjectButton { button, pressed });
                }
            }
            Event::PeerKey { scancode, pressed } => {
                if self.injecting() {
                    self.injected.set_key(scancode, pressed);
                    self.unconfirmed.set_key(scancode, false);
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
                    self.proof_after = now_ms;
                    // A non-main device stays active for itself until main's first view
                    // proves main hears it.
                    if !self.panicked {
                        self.send_view(&mut out.actions);
                    }
                }
            }
            Event::PeerHeld(held) => self.check_held(held, &mut out.actions),
            Event::PeerHeard { sent_ms, panicked } => {
                if self.online {
                    self.answered = self.answered.max(Some(sent_ms));
                    if panicked {
                        self.resume_until = None;
                    } else if self.resume_until.is_some() && me == self.cfg.main && sent_ms > self.proof_after {
                        self.resume_until = None;
                        self.set_active(peer, &mut out.actions);
                        self.send_view(&mut out.actions);
                    }
                }
            }
            Event::Tick { now_ms } => {
                self.now = self.now.max(now_ms);
                if self.online && self.now.saturating_sub(self.last_alive) > LINK_TIMEOUT_MS {
                    self.link_lost(&mut out.actions);
                }
                if self.active != me && !self.view_fresh() {
                    self.control_lost(&mut out.actions);
                }
                if self.online && self.active != me && self.unanswered() {
                    self.resume_until = Some(self.now + RESUME_WINDOW_MS);
                    self.proof_after = self.answered.unwrap_or(self.now);
                    self.control_lost(&mut out.actions);
                }
                if self.resume_until.is_some_and(|until| self.now > until) {
                    self.resume_until = None;
                }
                // Longer than a link timeout, so that one lost view does not end the conflict.
                let stopped = self.peer_views.is_none_or(|(_, last)| {
                    self.now.saturating_sub(last) > VIEW_TIMEOUT_MS
                });
                if self.main_conflict && stopped {
                    self.end_main_conflict();
                }
                // A conflict ends when views stop, so a panic must not silence them: it would end
                // the peer's conflict but not this one, and only one side would be free to cross.
                if self.online
                    && (!self.panicked || self.main_conflict)
                    && self.now.saturating_sub(self.last_view) >= ACTIVE_VIEW_INTERVAL_MS
                {
                    self.send_view(&mut out.actions);
                }
            }
            Event::PeerActiveView { active } => {
                let known = active == me || active == peer;
                if self.online && known && me == self.cfg.main {
                    self.peer_view_heard(&mut out.actions);
                } else if self.online && known {
                    self.view_heard = Some(self.now);
                    self.resume_until = None;
                    // A view that crossed our own fresh switch on the wire is stale, not a desync.
                    if !self.panicked && !self.conflicts(active) {
                        self.set_active(active, &mut out.actions);
                    }
                }
            }
            Event::PeerGameLock(locked) => self.peer_game_lock = locked,
            Event::PeerFocus { target, x, y } => {
                let known = target == me || target == peer;
                // A startup burst is sent whether or not main hears us, so it proves nothing:
                // handing control to main still takes a view.
                let unproven = me != self.cfg.main && target == peer && self.view_heard.is_none();
                if self.online
                    && known
                    && !unproven
                    && !self.main_conflict
                    && !(self.conflicts(target) && me == self.cfg.main)
                {
                    self.panicked = false;
                    self.last_switch = None;
                    self.resume_until = None;
                    // Main only claims focus while it hears us, so this counts like a view.
                    self.view_heard = Some(self.now);
                    self.set_active(target, &mut out.actions);
                    if target == me && !(x == KEEP_CURSOR && y == KEEP_CURSOR) {
                        out.actions.push(Action::WarpCursor { x, y });
                    }
                }
            }
            Event::EdgeCrossed { x, y, from_peer } => {
                if self.online
                    && !self.panicked
                    && !self.main_conflict
                    && !self.captive
                    && !self.peer_game_lock
                    && (from_peer || self.cfg.sharing.mouse)
                    && self.active == me
                    && self.view_fresh()
                {
                    self.own_switch(peer, x, y, &mut out.actions);
                }
            }
            Event::ManualSwitch => {
                self.panicked = false;
                if self.online && !self.main_conflict {
                    let target = if self.active == me { peer } else { me };
                    if target == me || self.view_fresh() {
                        self.own_switch(target, KEEP_CURSOR, KEEP_CURSOR, &mut out.actions);
                    } else {
                        // Only ask: control moves on main's next view, which proves it heard.
                        // Panic silences views, so this is also how a panic ends from here.
                        out.actions.push(Action::SendFocus {
                            target,
                            x: KEEP_CURSOR,
                            y: KEEP_CURSOR,
                        });
                    }
                }
            }
            Event::SetCaptive(c) => self.captive = c,
            Event::Panic => {
                self.enter_panic(&mut out.actions);
                out.actions.push(Action::SendPanic);
            }
            Event::PeerPanic => self.enter_panic(&mut out.actions),
            Event::SetSharing(sharing) => {
                if !sharing.keyboard {
                    self.release_forwarded_keys(&mut out.actions);
                }
                if !sharing.mouse {
                    self.release_forwarded_buttons(&mut out.actions);
                    self.carry_x = 0.0;
                    self.carry_y = 0.0;
                }
                self.cfg.sharing = sharing;
            }
            Event::SetMain(device) => {
                if (device == me || device == peer) && device != self.cfg.main {
                    self.cfg.main = device;
                    self.resume_until = None;
                    // That the old main heard us says nothing about the new one.
                    self.view_heard = None;
                    self.end_main_conflict();
                }
            }
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
        self.online && !self.panicked && !self.main_conflict && self.active == self.cfg.me
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
            self.release_injected(actions);
        }
        self.active = target;
        self.carry_x = 0.0;
        self.carry_y = 0.0;
    }

    fn own_switch(&mut self, target: DeviceId, x: i32, y: i32, actions: &mut Vec<Action>) {
        self.resume_until = None;
        self.set_active(target, actions);
        self.last_switch = Some((self.now, target));
        actions.push(Action::SendFocus { target, x, y });
    }

    /// Tells the peer who is active. Only the main device's view counts. While control waits to
    /// go back there is nothing to tell: the peer stays active for itself, and adopting this
    /// device instead would swallow its keyboard for no reason.
    fn send_view(&mut self, actions: &mut Vec<Action>) {
        if self.cfg.me == self.cfg.main && self.resume_until.is_none() {
            self.last_view = self.now;
            actions.push(Action::SendActiveView { active: self.active });
        }
    }

    /// Puts what's down here for the peer in line with its report. A key it no longer holds is
    /// let go at once, as that can't hurt. A key it holds that isn't down here takes a second
    /// report in a row. A report can be on its way as the key comes up, and a tap ends before
    /// the next one, so a key that's seen twice really is being held. Pressing that late is only
    /// fine for a key: a button lands wherever the pointer is by then, so it waits for a pointer
    /// that hasn't moved. Auto-repeat needs nothing from here. Its presses come in as `PeerKey`,
    /// repair a missed press by themselves, and take the key off the unconfirmed list.
    fn check_held(&mut self, held: HeldInput, actions: &mut Vec<Action>) {
        if !self.injecting() {
            self.unconfirmed = HeldInput::default();
            return;
        }
        let gone = self.injected.without(&held);
        let missing = held.without(&self.injected);
        let confirmed = missing.and(&self.unconfirmed);
        for scancode in gone.keys() {
            actions.push(Action::InjectKey { scancode, pressed: false });
        }
        for button in gone.buttons() {
            actions.push(Action::InjectButton { button, pressed: false });
        }
        for scancode in confirmed.keys() {
            actions.push(Action::InjectKey { scancode, pressed: true });
        }
        for button in confirmed.buttons() {
            actions.push(Action::InjectButton { button, pressed: true });
        }
        self.injected = self.injected.without(&gone).or(&confirmed);
        self.unconfirmed = missing.without(&confirmed);
    }

    fn release_injected(&mut self, actions: &mut Vec<Action>) {
        self.injected = HeldInput::default();
        self.unconfirmed = HeldInput::default();
        actions.push(Action::ReleaseHeld);
    }

    /// A peer that answers heartbeats has stopped answering this device's, so what's forwarded
    /// goes nowhere while the keyboard here is swallowed. Main has no views to tell it this.
    fn unanswered(&self) -> bool {
        self.cfg.me == self.cfg.main
            && self.answered.is_some_and(|sent| self.now.saturating_sub(sent) > VIEW_TIMEOUT_MS)
    }

    /// Remembers to hand control back, unless a dropout already has, or no way of knowing the
    /// peer hears this device exists. A device that isn't main gets it back from main's view,
    /// and only says so.
    fn note_lost_control(&mut self) {
        let me = self.cfg.me;
        let can_prove = me != self.cfg.main || self.answered.is_some();
        if self.active != me && !self.panicked && !self.main_conflict && can_prove {
            self.resume_until = Some(self.now + RESUME_WINDOW_MS);
        }
    }

    /// The peer sent a view, so it claims main too. Inside the grace period that may be a handover
    /// in progress. Past it, views can no longer repair a desync, which could leave both devices
    /// forwarding at each other, so control is lost here as with no main at all. Views keep going
    /// out regardless, even through a panic: they are how the peer finds the split too, and stays
    /// in it.
    fn peer_view_heard(&mut self, actions: &mut Vec<Action>) {
        let since = match self.peer_views {
            Some((since, last)) if self.now.saturating_sub(last) <= LINK_TIMEOUT_MS => since,
            _ => self.now,
        };
        self.peer_views = Some((since, self.now));
        if !self.main_conflict && self.now.saturating_sub(since) > SPLIT_GRACE_MS {
            self.main_conflict = true;
            self.control_lost(actions);
        }
    }

    fn end_main_conflict(&mut self) {
        self.main_conflict = false;
        self.peer_views = None;
    }

    /// Main counts as hearing this device. Always true on main itself.
    fn view_fresh(&self) -> bool {
        self.cfg.me == self.cfg.main
            || self
                .view_heard
                .is_some_and(|heard| self.now.saturating_sub(heard) <= VIEW_TIMEOUT_MS)
    }

    /// Main has gone quiet: control comes back here, though the link stays up. The peer may
    /// still hear us, so what it holds for us is released before the sets are cleared.
    fn control_lost(&mut self, actions: &mut Vec<Action>) {
        self.active = self.cfg.me;
        self.last_switch = None;
        self.view_heard = None;
        self.release_forwarded_keys(actions);
        self.release_forwarded_buttons(actions);
        self.reset_held();
        self.release_injected(actions);
    }

    /// Lets go on the peer of every key it holds for us. Their physical releases are then local.
    fn release_forwarded_keys(&mut self, actions: &mut Vec<Action>) {
        for scancode in self.fwd_keys.drain() {
            actions.push(Action::ForwardKey { scancode, pressed: false });
        }
    }

    /// Lets go on the peer of every button it holds for us. Their physical releases are then local.
    fn release_forwarded_buttons(&mut self, actions: &mut Vec<Action>) {
        for button in self.fwd_buttons.drain() {
            actions.push(Action::ForwardButton { button, pressed: false });
        }
    }

    fn link_lost(&mut self, actions: &mut Vec<Action>) {
        self.note_lost_control();
        self.online = false;
        self.answered = None;
        self.end_main_conflict();
        self.active = self.cfg.me;
        self.last_switch = None;
        self.view_heard = None;
        self.peer_game_lock = false;
        self.reset_held();
        self.release_injected(actions);
    }

    fn enter_panic(&mut self, actions: &mut Vec<Action>) {
        self.panicked = true;
        self.active = self.cfg.me;
        self.last_switch = None;
        self.resume_until = None;
        self.reset_held();
        self.release_injected(actions);
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

/// A stretch of one screen edge, in pixels along that edge (`start..=end`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeSpan {
    pub edge: SwitchEdge,
    pub start: i32,
    pub end: i32,
}

/// Accumulates outward pointer push against an edge until it exceeds the resistance.
pub struct EdgeTracker {
    resistance: i32,
    pushed: i32,
}

impl EdgeTracker {
    pub const fn new(resistance: i32) -> Self {
        Self { resistance, pushed: 0 }
    }

    pub fn set_resistance(&mut self, resistance: i32) {
        self.resistance = resistance;
    }

    /// True when this movement completes a push across `span`.
    ///
    /// Only outward movement while the pointer sits on the edge pixel inside the span
    /// accumulates. Moving away, leaving the span, or holding a button clears it.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        span: EdgeSpan,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        dx: i32,
        dy: i32,
        buttons_held: bool,
    ) -> bool {
        if buttons_held {
            self.pushed = 0;
            return false;
        }
        let (on_edge, along, outward) = match span.edge {
            SwitchEdge::Left => (x <= 0, y, -dx),
            SwitchEdge::Right => (x >= w - 1, y, dx),
            SwitchEdge::Top => (y <= 0, x, -dy),
            SwitchEdge::Bottom => (y >= h - 1, x, dy),
        };
        if !on_edge || along < span.start || along > span.end || outward < 0 {
            self.pushed = 0;
            return false;
        }
        self.pushed = self.pushed.saturating_add(outward);
        if self.pushed >= self.resistance {
            self.pushed = 0;
            return true;
        }
        false
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
    /// Router 1 online, with main's first view adopted: device 0 is active.
    fn adopted_router(c: RouterConfig) -> Router {
        let mut r = Router::new(c);
        online(&mut r);
        r.handle(Event::PeerActiveView { active: 0 });
        assert_eq!(r.active(), 0);
        r
    }
    /// Heartbeats after `online`, close enough together that the link stays up.
    fn alive_to_3000(r: &mut Router) {
        r.handle(Event::PeerAlive { now_ms: 2000 });
        r.handle(Event::PeerAlive { now_ms: 3000 });
    }
    fn key(r: &mut Router, scancode: u16, pressed: bool) -> Routed {
        r.handle(Event::LocalKey { scancode, pressed })
    }
    /// Router 0 (main) online and forwarding to device 1.
    fn forwarding_router() -> Router {
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::EdgeCrossed { x: 5, y: 6, from_peer: false });
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
        online(&mut r);
        assert!(r.online());
        let out = r.handle(Event::PeerActiveView { active: 0 });
        assert_eq!(r.active(), 0);
        assert!(out.actions.contains(&Action::SetBlocking { mouse: true, keyboard: true }));
    }

    #[test]
    fn non_main_waits_for_first_view() {
        let mut r = router(1);
        let out = online(&mut r);
        assert!(r.online());
        assert_eq!(r.active(), 1);
        assert!(out.actions.is_empty());
        assert!(!key(&mut r, 30, true).swallow);
        assert!(!r.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);
    }

    #[test]
    fn one_way_link_falls_back_to_local() {
        let mut r = adopted_router(cfg(1));
        assert!(key(&mut r, 30, true).swallow);
        // Heartbeats keep arriving, but main has stopped sending views.
        alive_to_3000(&mut r);
        let out = r.handle(Event::Tick { now_ms: 1000 + VIEW_TIMEOUT_MS + 1 });
        assert!(r.online());
        assert_eq!(r.active(), 1);
        assert!(out.actions.contains(&Action::ReleaseHeld));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
        assert!(!key(&mut r, 31, true).swallow);
        assert!(!r.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);
        // A key forwarded before the fallback is local now too.
        assert!(!key(&mut r, 30, false).swallow);
        // More heartbeats alone change nothing.
        r.handle(Event::PeerAlive { now_ms: 4000 });
        r.handle(Event::Tick { now_ms: 4000 });
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn view_timeout_boundary() {
        let mut r = adopted_router(cfg(1));
        alive_to_3000(&mut r);
        let out = r.handle(Event::Tick { now_ms: 1000 + 2500 });
        assert_eq!(r.active(), 0);
        assert!(out.actions.is_empty());
        r.handle(Event::Tick { now_ms: 1000 + 2501 });
        assert_eq!(r.active(), 1);
        assert_eq!(VIEW_TIMEOUT_MS, 2500);
    }

    #[test]
    fn views_keep_control_with_the_peer() {
        let mut r = adopted_router(cfg(1));
        for now_ms in [2000, 3000, 4000, 5000] {
            r.handle(Event::PeerAlive { now_ms });
            r.handle(Event::PeerActiveView { active: 0 });
            r.handle(Event::Tick { now_ms });
        }
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn view_resumes_after_fallback() {
        let mut r = adopted_router(cfg(1));
        alive_to_3000(&mut r);
        r.handle(Event::Tick { now_ms: 3501 });
        assert_eq!(r.active(), 1);
        r.handle(Event::PeerAlive { now_ms: 3600 });
        let out = r.handle(Event::PeerActiveView { active: 0 });
        assert_eq!(r.active(), 0);
        assert!(out.actions.contains(&Action::SetBlocking { mouse: true, keyboard: true }));
        assert!(key(&mut r, 30, true).swallow);
    }

    #[test]
    fn fallback_stays_until_the_next_view() {
        let mut r = adopted_router(cfg(1));
        alive_to_3000(&mut r);
        r.handle(Event::Tick { now_ms: 3501 });
        // Neither a push on the edge nor a manual switch hands control to a silent main.
        let out = r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        assert!(out.actions.is_empty());
        assert_eq!(r.active(), 1);
        let out = r.handle(Event::ManualSwitch);
        assert_eq!(r.active(), 1);
        assert!(!out.actions.iter().any(|a| matches!(a, Action::SetBlocking { .. })));
    }

    #[test]
    fn manual_switch_with_no_view_still_asks_main() {
        // Panic silences main's views, so this is how the laptop ends a panic on both devices.
        let mut r = router(1);
        online(&mut r);
        r.handle(Event::Panic);
        let out = r.handle(Event::ManualSwitch);
        assert!(!r.panicked());
        assert_eq!(r.active(), 1);
        assert!(out.actions.contains(&Action::SendFocus { target: 0, x: KEEP_CURSOR, y: KEEP_CURSOR }));
        r.handle(Event::PeerActiveView { active: 0 });
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn edge_crossing_needs_a_recent_view_on_non_main() {
        let mut r = router(1);
        online(&mut r);
        assert!(r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false }).actions.is_empty());
        assert_eq!(r.active(), 1);
        r.handle(Event::PeerActiveView { active: 1 });
        alive_to_3000(&mut r);
        r.handle(Event::Tick { now_ms: 3501 });
        assert!(r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false }).actions.is_empty());
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn peer_focus_counts_as_hearing_main() {
        let mut r = router(1);
        online(&mut r);
        r.handle(Event::PeerActiveView { active: 1 });
        r.handle(Event::PeerAlive { now_ms: 2000 });
        r.handle(Event::PeerFocus { target: 0, x: KEEP_CURSOR, y: KEEP_CURSOR });
        assert_eq!(r.active(), 0);
        r.handle(Event::PeerAlive { now_ms: 3000 });
        r.handle(Event::PeerAlive { now_ms: 4000 });
        r.handle(Event::Tick { now_ms: 4500 });
        assert_eq!(r.active(), 0);
        r.handle(Event::Tick { now_ms: 4501 });
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn startup_burst_does_not_adopt_before_first_view() {
        let mut r = router(1);
        online(&mut r);
        let burst = Event::PeerFocus { target: 0, x: KEEP_CURSOR, y: KEEP_CURSOR };
        for _ in 0..3 {
            assert!(r.handle(burst).actions.is_empty());
        }
        assert_eq!(r.active(), 1);
        assert!(!key(&mut r, 30, true).swallow);
        // The view that follows is what hands control over.
        r.handle(Event::PeerActiveView { active: 0 });
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn focus_for_this_device_needs_no_view() {
        let mut r = router(1);
        online(&mut r);
        let out = r.handle(Event::PeerFocus { target: 1, x: 4, y: 5 });
        assert!(out.actions.contains(&Action::WarpCursor { x: 4, y: 5 }));
    }

    #[test]
    fn peer_focus_does_not_end_a_fallback() {
        let mut r = adopted_router(cfg(1));
        alive_to_3000(&mut r);
        r.handle(Event::Tick { now_ms: 3501 });
        assert_eq!(r.active(), 1);
        r.handle(Event::PeerFocus { target: 0, x: KEEP_CURSOR, y: KEEP_CURSOR });
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn fallback_forwards_held_releases() {
        let mut r = adopted_router(cfg(1));
        assert!(key(&mut r, 30, true).swallow);
        assert!(key(&mut r, 31, true).swallow);
        assert!(r.handle(Event::LocalButton { button: 1, pressed: true }).swallow);
        alive_to_3000(&mut r);
        let out = r.handle(Event::Tick { now_ms: 3501 });
        assert!(out.actions.contains(&Action::ForwardKey { scancode: 30, pressed: false }));
        assert!(out.actions.contains(&Action::ForwardKey { scancode: 31, pressed: false }));
        assert!(out.actions.contains(&Action::ForwardButton { button: 1, pressed: false }));
        // Already released for the peer, so the physical release stays local and sends nothing.
        let up = key(&mut r, 30, false);
        assert!(!up.swallow);
        assert!(up.actions.is_empty());
        let up = r.handle(Event::LocalButton { button: 1, pressed: false });
        assert!(!up.swallow);
        assert!(up.actions.is_empty());
    }

    #[test]
    fn link_loss_forwards_nothing() {
        let mut r = adopted_router(cfg(1));
        key(&mut r, 30, true);
        let out = r.handle(Event::Tick { now_ms: 1000 + LINK_TIMEOUT_MS + 1 });
        assert!(!out.actions.iter().any(|a| matches!(a, Action::ForwardKey { .. })));
    }

    #[test]
    fn main_never_falls_back_on_views() {
        let mut r = forwarding_router();
        alive_to_3000(&mut r);
        r.handle(Event::PeerAlive { now_ms: 4000 });
        r.handle(Event::Tick { now_ms: 4000 });
        assert!(r.online());
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn link_up_on_main_stays_unblocked() {
        let mut r = router(0);
        let out = online(&mut r);
        assert_eq!(r.active(), 0);
        assert_eq!(out.actions, vec![Action::SendActiveView { active: 0 }]);
    }

    #[test]
    fn shared_input_forwards_to_remote_active() {
        let mut r = adopted_router(cfg(1));
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
        let mut r = adopted_router(c);
        let k = key(&mut r, 30, true);
        assert!(!k.swallow);
        assert!(k.actions.is_empty());
        assert_eq!(r.active(), 0);
        assert!(r.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);

        let mut c = cfg(1);
        c.sharing = Sharing { mouse: false, keyboard: true };
        let mut r = Router::new(c);
        online(&mut r);
        let out = r.handle(Event::PeerActiveView { active: 0 });
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
        r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
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
        r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
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
        r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        let up = r.handle(Event::LocalButton { button: 1, pressed: false });
        assert!(!up.swallow);
        assert!(up.actions.is_empty());
    }

    #[test]
    fn switch_releases_held_on_losing_device() {
        let mut r = router(0);
        online(&mut r);
        let out = r.handle(Event::EdgeCrossed { x: 7, y: 9, from_peer: false });
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
        let mut r = adopted_router(cfg(1));
        r.handle(Event::Tick { now_ms: 1000 + 1351 });
        assert_eq!(r.active(), 1);
        assert!(!key(&mut r, 30, true).swallow);
    }

    #[test]
    fn link_survives_within_timeout() {
        let mut r = forwarding_router();
        let out = r.handle(Event::Tick { now_ms: 1000 + 1350 });
        assert!(r.online());
        assert_eq!(out.actions, vec![Action::SendActiveView { active: 1 }]);
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
        assert!(r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false }).actions.is_empty());
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
        let out = r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        assert!(!out.actions.iter().any(|a| matches!(a, Action::SendFocus { .. })));
        assert_eq!(r.active(), 0);
        r.handle(Event::SetCaptive(false));
        r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
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
        let mut r = adopted_router(cfg(1));
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
        other.handle(Event::PeerActiveView { active: 0 });
        // Views disagree: `other` earlier accepted a claim for device 1 that `main` never saw.
        other.handle(Event::PeerFocus { target: 1, x: 0, y: 0 });
        assert_eq!((main.active(), other.active()), (0, 1));
        // Both switch within the race window with opposite targets.
        let from_main = main.handle(Event::EdgeCrossed { x: 1, y: 2, from_peer: false });
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
        main.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        main.handle(Event::Tick { now_ms: 1000 + SWITCH_RACE_MS + 1 });
        main.handle(Event::PeerFocus { target: 0, x: 0, y: 0 });
        assert_eq!(main.active(), 0);
    }

    #[test]
    fn claim_at_race_boundary_still_conflicts() {
        let mut main = router(0);
        online(&mut main);
        main.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        main.handle(Event::Tick { now_ms: 1000 + SWITCH_RACE_MS });
        main.handle(Event::PeerFocus { target: 0, x: 0, y: 0 });
        assert_eq!(main.active(), 1);
    }

    #[test]
    fn pointer_speed_scales_with_carry() {
        let mut c = cfg(1);
        c.pointer_speed = 0.5;
        let mut r = adopted_router(c);
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
        let mut r = adopted_router(cfg(1));
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
        r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
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

    fn view_of(out: &Routed) -> Option<Event> {
        out.actions.iter().find_map(|a| match a {
            Action::SendActiveView { active } => Some(Event::PeerActiveView { active: *active }),
            _ => None,
        })
    }

    #[test]
    fn desync_recovers_when_laptop_restarts() {
        let mut debian = forwarding_router();
        assert_eq!(debian.active(), 1);
        // The laptop's fresh router comes up for itself while Debian still forwards to it.
        let mut laptop = router(1);
        online(&mut laptop);
        assert_eq!(laptop.active(), 1);
        let now = 1000 + ACTIVE_VIEW_INTERVAL_MS;
        debian.handle(Event::PeerAlive { now_ms: now });
        laptop.handle(Event::PeerAlive { now_ms: now });
        let view = view_of(&debian.handle(Event::Tick { now_ms: now })).unwrap();
        let out = laptop.handle(view);
        laptop.handle(Event::Tick { now_ms: now });
        assert_eq!((debian.active(), laptop.active()), (1, 1));
        assert!(out.actions.is_empty());
    }

    #[test]
    fn view_applies_set_active_effects_without_warp() {
        let mut laptop = router(1);
        online(&mut laptop);
        laptop.handle(Event::PeerActiveView { active: 1 });
        assert_eq!(laptop.active(), 1);
        let out = laptop.handle(Event::PeerActiveView { active: 0 });
        assert_eq!(laptop.active(), 0);
        assert!(out.actions.contains(&Action::ReleaseHeld));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: true, keyboard: true }));
        assert!(!out.actions.iter().any(|a| matches!(a, Action::WarpCursor { .. })));
    }

    #[test]
    fn view_does_not_clear_panic() {
        let mut laptop = router(1);
        online(&mut laptop);
        laptop.handle(Event::Panic);
        assert_eq!(laptop.active(), 1);
        let out = laptop.handle(Event::PeerActiveView { active: 0 });
        assert!(laptop.panicked());
        assert_eq!(laptop.active(), 1);
        assert!(out.actions.is_empty());
    }

    #[test]
    fn main_ignores_peer_view() {
        let mut main = router(0);
        online(&mut main);
        let out = main.handle(Event::PeerActiveView { active: 1 });
        assert_eq!(main.active(), 0);
        assert!(out.actions.is_empty());
    }

    #[test]
    fn view_ignored_offline_and_for_unknown_device() {
        let mut laptop = router(1);
        laptop.handle(Event::PeerActiveView { active: 0 });
        assert_eq!(laptop.active(), 1);
        online(&mut laptop);
        laptop.handle(Event::PeerActiveView { active: 7 });
        assert_eq!(laptop.active(), 1);
    }

    #[test]
    fn stale_view_does_not_undo_own_switch() {
        // Debian's view crossed the laptop's own switch on the wire.
        let mut laptop = router(1);
        online(&mut laptop);
        laptop.handle(Event::PeerActiveView { active: 1 });
        laptop.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        assert_eq!(laptop.active(), 0);
        laptop.handle(Event::PeerActiveView { active: 1 });
        assert_eq!(laptop.active(), 0);
        // Past the race window the main device's view wins again.
        laptop.handle(Event::Tick { now_ms: 1000 + SWITCH_RACE_MS + 1 });
        laptop.handle(Event::PeerActiveView { active: 1 });
        assert_eq!(laptop.active(), 1);
    }

    #[test]
    fn view_sent_at_interval_only_when_online() {
        let mut r = router(0);
        // Offline: nothing, however long it has been.
        assert_eq!(view_of(&r.handle(Event::Tick { now_ms: 900 })), None);
        // Link-up announces at once.
        assert_eq!(view_of(&online(&mut r)), Some(Event::PeerActiveView { active: 0 }));
        let at = |r: &mut Router, now_ms| view_of(&r.handle(Event::Tick { now_ms }));
        assert_eq!(at(&mut r, 1000 + ACTIVE_VIEW_INTERVAL_MS - 1), None);
        assert_eq!(
            at(&mut r, 1000 + ACTIVE_VIEW_INTERVAL_MS),
            Some(Event::PeerActiveView { active: 0 })
        );
        assert_eq!(at(&mut r, 1000 + ACTIVE_VIEW_INTERVAL_MS + 1), None);
        // Link lost at this tick: no view from it or after it.
        assert_eq!(at(&mut r, 10_000), None);
        assert!(!r.online());
        assert_eq!(at(&mut r, 20_000), None);

        // Panicked: silent on ticks and on link-up.
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::Panic);
        r.handle(Event::PeerAlive { now_ms: 2000 });
        assert_eq!(view_of(&r.handle(Event::Tick { now_ms: 2000 })), None);
        let mut r = router(0);
        r.handle(Event::Panic);
        assert_eq!(view_of(&online(&mut r)), None);

        // Only the main device sends views.
        let mut r = router(1);
        assert_eq!(view_of(&online(&mut r)), None);
        r.handle(Event::PeerAlive { now_ms: 2000 });
        assert_eq!(view_of(&r.handle(Event::Tick { now_ms: 2000 })), None);
    }

    #[test]
    fn peer_game_lock_blocks_crossing() {
        let mut r = router(1);
        online(&mut r);
        r.handle(Event::PeerActiveView { active: 1 });
        r.handle(Event::PeerGameLock(true));
        let out = r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        assert!(!out.actions.iter().any(|a| matches!(a, Action::SendFocus { .. })));
        assert_eq!(r.active(), 1);
        // The local game lock is a separate flag: clearing it leaves the peer's in force.
        r.handle(Event::SetCaptive(true));
        r.handle(Event::SetCaptive(false));
        r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        assert_eq!(r.active(), 1);
        r.handle(Event::PeerGameLock(false));
        r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn peer_game_lock_does_not_outlive_the_link() {
        let mut r = router(1);
        online(&mut r);
        r.handle(Event::PeerActiveView { active: 1 });
        r.handle(Event::PeerGameLock(true));
        r.handle(Event::Tick { now_ms: 1000 + LINK_TIMEOUT_MS + 1 });
        r.handle(Event::PeerAlive { now_ms: 5000 });
        r.handle(Event::PeerActiveView { active: 1 });
        r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false });
        assert_eq!(r.active(), 0);
    }

    const OFF: Sharing = Sharing { mouse: false, keyboard: false };
    const ON: Sharing = Sharing { mouse: true, keyboard: true };

    #[test]
    fn turning_off_sharing_stops_blocking() {
        let mut r = forwarding_router();
        let out = r.handle(Event::SetSharing(OFF));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
        assert_eq!(r.sharing(), OFF);
        assert!(!key(&mut r, 30, true).swallow);
        assert!(!r.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);
        assert!(!r.handle(Event::LocalWheel { dx: 0, dy: 1 }).swallow);
        assert!(!r.handle(Event::LocalButton { button: 1, pressed: true }).swallow);
    }

    #[test]
    fn sharing_off_releases_forwarded_keys() {
        let mut r = forwarding_router();
        assert!(key(&mut r, 30, true).swallow);
        let out = r.handle(Event::SetSharing(OFF));
        assert!(out.actions.contains(&Action::ForwardKey { scancode: 30, pressed: false }));
        // The peer already has its release, so the physical one stays here, and only once.
        let up = key(&mut r, 30, false);
        assert!(!up.swallow);
        assert!(up.actions.is_empty());
    }

    #[test]
    fn sharing_off_releases_forwarded_buttons() {
        let mut r = forwarding_router();
        assert!(r.handle(Event::LocalButton { button: 1, pressed: true }).swallow);
        let out = r.handle(Event::SetSharing(OFF));
        assert!(out.actions.contains(&Action::ForwardButton { button: 1, pressed: false }));
        let up = r.handle(Event::LocalButton { button: 1, pressed: false });
        assert!(!up.swallow);
        assert!(up.actions.is_empty());
    }

    #[test]
    fn releases_go_out_before_blocking_ends() {
        let mut r = forwarding_router();
        key(&mut r, 30, true);
        let out = r.handle(Event::SetSharing(OFF));
        assert_eq!(
            out.actions,
            vec![
                Action::ForwardKey { scancode: 30, pressed: false },
                Action::SetBlocking { mouse: false, keyboard: false },
            ]
        );
    }

    #[test]
    fn each_toggle_only_touches_its_own_input() {
        let mut r = forwarding_router();
        key(&mut r, 30, true);
        r.handle(Event::LocalButton { button: 1, pressed: true });

        // Keyboard off: the key lets go, and a drag in progress carries on.
        let out = r.handle(Event::SetSharing(Sharing { mouse: true, keyboard: false }));
        assert_eq!(
            out.actions,
            vec![
                Action::ForwardKey { scancode: 30, pressed: false },
                Action::SetBlocking { mouse: true, keyboard: false },
            ]
        );
        assert!(r.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);
        assert!(r.handle(Event::LocalButton { button: 1, pressed: false }).swallow);

        // Mouse off instead: the button lets go, and a held key carries on.
        let mut r = forwarding_router();
        key(&mut r, 30, true);
        r.handle(Event::LocalButton { button: 1, pressed: true });
        let out = r.handle(Event::SetSharing(Sharing { mouse: false, keyboard: true }));
        assert_eq!(
            out.actions,
            vec![
                Action::ForwardButton { button: 1, pressed: false },
                Action::SetBlocking { mouse: false, keyboard: true },
            ]
        );
        assert!(key(&mut r, 30, false).swallow);
    }

    #[test]
    fn repeat_of_a_released_key_stays_local() {
        let mut r = forwarding_router();
        key(&mut r, 30, true);
        r.handle(Event::SetSharing(OFF));
        assert!(!key(&mut r, 30, true).swallow);
        // Sharing back on with the key still down: it is local until it comes up.
        r.handle(Event::SetSharing(ON));
        assert!(!key(&mut r, 30, true).swallow);
        assert!(!key(&mut r, 30, false).swallow);
        assert!(key(&mut r, 30, true).swallow);
    }

    #[test]
    fn turning_sharing_back_on_resumes() {
        let mut r = forwarding_router();
        r.handle(Event::SetSharing(OFF));
        assert_eq!(r.active(), 1);
        let out = r.handle(Event::SetSharing(ON));
        assert_eq!(out.actions, vec![Action::SetBlocking { mouse: true, keyboard: true }]);
        assert!(key(&mut r, 30, true).swallow);
        assert!(r.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);
    }

    #[test]
    fn sharing_on_while_local_blocks_nothing() {
        let mut r = Router::new(RouterConfig { sharing: OFF, ..cfg(0) });
        online(&mut r);
        let out = r.handle(Event::SetSharing(ON));
        assert!(!out.actions.iter().any(|a| matches!(a, Action::SetBlocking { .. })));
        assert!(!key(&mut r, 30, true).swallow);
    }

    #[test]
    fn sharing_on_does_not_undo_a_panic() {
        let mut r = forwarding_router();
        r.handle(Event::SetSharing(OFF));
        r.handle(Event::Panic);
        let out = r.handle(Event::SetSharing(ON));
        assert!(r.panicked());
        assert!(!out.actions.iter().any(|a| matches!(a, Action::SetBlocking { .. })));
        assert!(!key(&mut r, 30, true).swallow);
    }

    #[test]
    fn mouse_sharing_off_refuses_edge_crossing() {
        let mut r = Router::new(cfg(0));
        online(&mut r);
        r.handle(Event::SetSharing(Sharing { mouse: false, keyboard: true }));
        assert!(r.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false }).actions.is_empty());
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn sharing_off_keeps_injecting_for_the_peer() {
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::SetSharing(OFF));
        let out = r.handle(Event::PeerKey { scancode: 30, pressed: true });
        assert_eq!(out.actions, vec![Action::InjectKey { scancode: 30, pressed: true }]);
    }

    #[test]
    fn set_main_swaps_view_duty() {
        // Promoted: the laptop starts sending views.
        let mut laptop = router(1);
        online(&mut laptop);
        laptop.handle(Event::SetMain(1));
        assert_eq!(laptop.main(), 1);
        let view = view_of(&laptop.handle(Event::Tick { now_ms: 1000 }));
        assert_eq!(view, Some(Event::PeerActiveView { active: 1 }));

        // Demoted: Debian goes quiet and follows the laptop's views.
        let mut debian = router(0);
        online(&mut debian);
        debian.handle(Event::SetMain(1));
        debian.handle(Event::PeerAlive { now_ms: 2000 });
        assert_eq!(view_of(&debian.handle(Event::Tick { now_ms: 2000 })), None);
        debian.handle(Event::PeerActiveView { active: 1 });
        assert_eq!(debian.active(), 1);
        assert!(key(&mut debian, 30, true).swallow);
    }

    #[test]
    fn promoted_device_ignores_views_and_keeps_control_without_them() {
        let mut laptop = adopted_router(cfg(1));
        laptop.handle(Event::SetMain(1));
        laptop.handle(Event::PeerActiveView { active: 1 });
        assert_eq!(laptop.active(), 0);
        alive_to_3000(&mut laptop);
        laptop.handle(Event::Tick { now_ms: 1000 + VIEW_TIMEOUT_MS + 1 });
        assert_eq!(laptop.active(), 0);
    }

    #[test]
    fn demoted_device_takes_control_back_from_a_silent_main() {
        let mut debian = forwarding_router();
        assert!(key(&mut debian, 30, true).swallow);
        debian.handle(Event::SetMain(1));
        // Nothing has proved the new main hears us, so a tick is enough.
        let out = debian.handle(Event::Tick { now_ms: 1300 });
        assert_eq!(debian.active(), 0);
        assert!(out.actions.contains(&Action::ForwardKey { scancode: 30, pressed: false }));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
        assert!(!key(&mut debian, 31, true).swallow);
    }

    #[test]
    fn set_main_to_the_current_main_changes_nothing() {
        let mut laptop = adopted_router(cfg(1));
        assert!(key(&mut laptop, 30, true).swallow);
        assert!(laptop.handle(Event::SetMain(0)).actions.is_empty());
        let out = laptop.handle(Event::Tick { now_ms: 1000 });
        assert!(out.actions.is_empty());
        assert_eq!(laptop.active(), 0);
    }

    #[test]
    fn set_main_ignores_an_unknown_device() {
        let mut laptop = adopted_router(cfg(1));
        assert!(laptop.handle(Event::SetMain(7)).actions.is_empty());
        assert_eq!(laptop.main(), 0);
        laptop.handle(Event::Tick { now_ms: 1000 });
        assert_eq!(laptop.active(), 0);
    }

    /// Delivers heartbeats both ways at `now_ms`, ticks both, and hands over main's view.
    fn exchange(a: &mut Router, b: &mut Router, now_ms: u64) {
        a.handle(Event::PeerAlive { now_ms });
        b.handle(Event::PeerAlive { now_ms });
        let from_a = view_of(&a.handle(Event::Tick { now_ms }));
        let from_b = view_of(&b.handle(Event::Tick { now_ms }));
        if let Some(view) = from_a {
            b.handle(view);
        }
        if let Some(view) = from_b {
            a.handle(view);
        }
    }

    /// Debian and the laptop with the link up and the laptop following Debian's first view.
    fn linked_pair() -> (Router, Router) {
        let mut debian = router(0);
        let mut laptop = router(1);
        let view = view_of(&online(&mut debian)).unwrap();
        online(&mut laptop);
        laptop.handle(view);
        (debian, laptop)
    }

    #[test]
    fn both_routers_settle_after_main_change() {
        // Debian holds control when the laptop becomes main.
        let (mut debian, mut laptop) = linked_pair();
        assert_eq!((debian.active(), laptop.active()), (0, 0));
        debian.handle(Event::SetMain(1));
        laptop.handle(Event::SetMain(1));
        for now_ms in [2000, 3000, 4000, 5000, 6000] {
            exchange(&mut debian, &mut laptop, now_ms);
            assert_eq!(debian.active(), laptop.active());
        }
        assert_eq!(debian.active(), 0);

        // The laptop holds control when it becomes main.
        let (mut debian, mut laptop) = linked_pair();
        let out = debian.handle(Event::EdgeCrossed { x: 1, y: 2, from_peer: false });
        assert!(out.actions.contains(&Action::SendFocus { target: 1, x: 1, y: 2 }));
        laptop.handle(Event::PeerFocus { target: 1, x: 1, y: 2 });
        assert_eq!((debian.active(), laptop.active()), (1, 1));
        debian.handle(Event::SetMain(1));
        laptop.handle(Event::SetMain(1));
        for now_ms in [2000, 3000, 4000, 5000, 6000] {
            exchange(&mut debian, &mut laptop, now_ms);
        }
        assert_eq!((debian.active(), laptop.active()), (1, 1));
        assert!(key(&mut debian, 30, true).swallow);

        // And back again.
        debian.handle(Event::SetMain(0));
        laptop.handle(Event::SetMain(0));
        for now_ms in [7000, 8000, 9000, 10_000, 11_000] {
            exchange(&mut debian, &mut laptop, now_ms);
        }
        assert_eq!(debian.active(), laptop.active());
    }

    /// Debian and the laptop with the link up, each with its own idea of the main device.
    fn pair_with_mains(debian_main: DeviceId, laptop_main: DeviceId) -> (Router, Router) {
        let mut debian = Router::new(RouterConfig { main: debian_main, ..cfg(0) });
        let mut laptop = Router::new(RouterConfig { main: laptop_main, ..cfg(1) });
        online(&mut debian);
        online(&mut laptop);
        (debian, laptop)
    }

    fn exchange_until(a: &mut Router, b: &mut Router, times: &[u64]) {
        for &now_ms in times {
            exchange(a, b, now_ms);
        }
    }

    fn assert_both_local(debian: &mut Router, laptop: &mut Router) {
        assert_eq!((debian.active(), laptop.active()), (0, 1));
        for r in [debian, laptop] {
            assert_eq!(r.blocking, (false, false));
            assert!(!key(r, 40, true).swallow);
            assert!(!r.handle(Event::LocalMove { dx: 1, dy: 1 }).swallow);
            key(r, 40, false);
        }
    }

    /// Two routers that have settled into a split, each local.
    fn split_pair() -> (Router, Router) {
        let (mut debian, mut laptop) = pair_with_mains(0, 1);
        exchange_until(&mut debian, &mut laptop, &[2000, 3000, 4000, 5000, 6000]);
        assert!(debian.main_conflict());
        assert!(laptop.main_conflict());
        (debian, laptop)
    }

    #[test]
    fn only_the_laptop_applied_the_change_so_both_fall_back_then_heal() {
        // Debian forwards, then the laptop alone takes up main: two mains.
        let (mut debian, mut laptop) = linked_pair();
        debian.handle(Event::EdgeCrossed { x: 1, y: 2, from_peer: false });
        laptop.handle(Event::PeerFocus { target: 1, x: 1, y: 2 });
        assert!(key(&mut debian, 30, true).swallow);
        laptop.handle(Event::SetMain(1));
        exchange_until(&mut debian, &mut laptop, &[2000, 3000, 4000, 5000]);
        assert_eq!(debian.active(), 1);

        // The view that crosses the grace period ends it, with the held key let go.
        debian.handle(Event::PeerAlive { now_ms: 6000 });
        laptop.handle(Event::PeerAlive { now_ms: 6000 });
        let from_debian = view_of(&debian.handle(Event::Tick { now_ms: 6000 })).unwrap();
        let from_laptop = view_of(&laptop.handle(Event::Tick { now_ms: 6000 })).unwrap();
        laptop.handle(from_debian);
        let out = debian.handle(from_laptop);
        assert!(out.actions.contains(&Action::ForwardKey { scancode: 30, pressed: false }));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
        exchange_until(&mut debian, &mut laptop, &[7000]);
        assert!(debian.main_conflict() && laptop.main_conflict());
        assert_both_local(&mut debian, &mut laptop);

        // Debian catches up: one main again, on both.
        debian.handle(Event::SetMain(1));
        assert!(!debian.main_conflict());
        exchange_until(&mut debian, &mut laptop, &[8000, 9000, 10_000, 11_000]);
        assert!(!laptop.main_conflict());
        assert_eq!((debian.active(), laptop.active()), (1, 1));
        assert!(key(&mut debian, 30, true).swallow);
        let out = laptop.handle(Event::PeerKey { scancode: 30, pressed: true });
        assert_eq!(out.actions, vec![Action::InjectKey { scancode: 30, pressed: true }]);
    }

    #[test]
    fn only_debian_applied_the_change_so_both_fall_back_then_heal() {
        // The laptop forwards, then Debian alone gives up main: no main, and no views.
        let (mut debian, mut laptop) = linked_pair();
        assert!(key(&mut laptop, 30, true).swallow);
        debian.handle(Event::SetMain(1));
        exchange_until(&mut debian, &mut laptop, &[2000, 3000, 4000]);
        assert!(!debian.main_conflict() && !laptop.main_conflict());
        assert_both_local(&mut debian, &mut laptop);

        // The laptop catches up, and its views bring both together.
        laptop.handle(Event::SetMain(1));
        exchange_until(&mut debian, &mut laptop, &[5000, 6000]);
        assert_eq!(debian.active(), laptop.active());
        assert!(!debian.main_conflict() && !laptop.main_conflict());
    }

    #[test]
    fn two_mains_forwarding_at_each_other_both_end_up_local() {
        let (mut debian, mut laptop) = pair_with_mains(0, 1);
        // The laptop forwards to Debian.
        laptop.handle(Event::EdgeCrossed { x: 1, y: 2, from_peer: false });
        debian.handle(Event::PeerFocus { target: 0, x: 1, y: 2 });
        assert_eq!((debian.active(), laptop.active()), (0, 0));
        // Debian crosses and every focus packet is lost: each forwards to the other.
        debian.handle(Event::Tick { now_ms: 1000 + SWITCH_RACE_MS + 1 });
        debian.handle(Event::EdgeCrossed { x: 1, y: 2, from_peer: false });
        assert_eq!((debian.active(), laptop.active()), (1, 0));
        assert!(key(&mut debian, 30, true).swallow);
        assert!(key(&mut laptop, 31, true).swallow);
        let peer_key = Event::PeerKey { scancode: 32, pressed: true };
        assert!(debian.handle(peer_key).actions.is_empty());
        assert!(laptop.handle(peer_key).actions.is_empty());

        exchange_until(&mut debian, &mut laptop, &[2000, 3000, 4000, 5000, 6000, 7000]);
        assert!(!key(&mut debian, 30, false).swallow);
        assert!(!key(&mut laptop, 31, false).swallow);
        assert_both_local(&mut debian, &mut laptop);
    }

    #[test]
    fn startup_burst_cannot_capture_a_main_laptop_for_long() {
        // Debian restarts on a corrupt file, so it thinks it is main, and its burst reaches a
        // laptop that really is main.
        let (mut debian, mut laptop) = pair_with_mains(0, 1);
        laptop.handle(Event::PeerFocus { target: 0, x: KEEP_CURSOR, y: KEEP_CURSOR });
        assert_eq!(laptop.active(), 0);
        assert!(key(&mut laptop, 30, true).swallow);
        exchange_until(&mut debian, &mut laptop, &[2000, 3000, 4000, 5000, 6000, 7000]);
        assert!(!key(&mut laptop, 30, false).swallow);
        assert_both_local(&mut debian, &mut laptop);
    }

    #[test]
    fn handover_inside_the_grace_period_is_not_a_split() {
        let (mut debian, mut laptop) = linked_pair();
        debian.handle(Event::EdgeCrossed { x: 1, y: 2, from_peer: false });
        laptop.handle(Event::PeerFocus { target: 1, x: 1, y: 2 });
        laptop.handle(Event::SetMain(1));
        // Debian keeps forwarding through views right up to the limit.
        exchange_until(&mut debian, &mut laptop, &[2000, 3000, 4000, 5000]);
        assert!(!debian.main_conflict());
        assert!(key(&mut debian, 30, true).swallow);
        debian.handle(Event::SetMain(1));
        exchange_until(&mut debian, &mut laptop, &[6000, 7000, 8000, 9000, 10_000]);
        assert!(!debian.main_conflict() && !laptop.main_conflict());
        assert_eq!((debian.active(), laptop.active()), (1, 1));
    }

    #[test]
    fn split_starts_only_past_the_grace_period() {
        assert_eq!(SPLIT_GRACE_MS, 3000);
        let mut r = forwarding_router();
        let view = Event::PeerActiveView { active: 1 };
        let at = |r: &mut Router, now_ms| {
            r.handle(Event::PeerAlive { now_ms });
            r.handle(view)
        };
        at(&mut r, 2000);
        at(&mut r, 3000);
        at(&mut r, 4000);
        at(&mut r, 2000 + SPLIT_GRACE_MS);
        assert!(!r.main_conflict());
        assert_eq!(r.active(), 1);
        let out = at(&mut r, 2000 + SPLIT_GRACE_MS + 1);
        assert!(r.main_conflict());
        assert_eq!(r.active(), 0);
        assert!(out.actions.contains(&Action::ReleaseHeld));
    }

    #[test]
    fn a_gap_in_peer_views_restarts_the_grace_period() {
        let mut r = forwarding_router();
        let view = Event::PeerActiveView { active: 1 };
        for now_ms in [2000, 3000, 4000] {
            r.handle(Event::PeerAlive { now_ms });
            r.handle(view);
        }
        // Heartbeats carry on, but no view comes for longer than a link timeout.
        r.handle(Event::PeerAlive { now_ms: 5000 });
        r.handle(Event::PeerAlive { now_ms: 4000 + LINK_TIMEOUT_MS + 1 });
        r.handle(view);
        // Past the grace period from the first view, but not from the one after the gap.
        r.handle(Event::PeerAlive { now_ms: 6000 });
        r.handle(view);
        assert!(!r.main_conflict());
    }

    #[test]
    fn split_refuses_focus_both_ways() {
        let (mut debian, mut laptop) = split_pair();
        let claim = Event::PeerFocus { target: 0, x: 1, y: 2 };
        assert!(laptop.handle(claim).actions.is_empty());
        assert_eq!(laptop.active(), 1);
        for ev in [Event::EdgeCrossed { x: 1, y: 2, from_peer: false }, Event::ManualSwitch] {
            assert!(debian.handle(ev).actions.is_empty());
            assert!(laptop.handle(ev).actions.is_empty());
        }
        let crossing = Event::EdgeCrossed { x: 1, y: 2, from_peer: true };
        assert!(laptop.handle(crossing).actions.is_empty());
        assert_both_local(&mut debian, &mut laptop);
    }

    #[test]
    fn split_injects_nothing_from_the_peer() {
        let (mut debian, _) = split_pair();
        let out = debian.handle(Event::PeerKey { scancode: 30, pressed: true });
        assert!(out.actions.is_empty());
    }

    #[test]
    fn split_keeps_sending_views_so_the_peer_stays_split_too() {
        let (mut debian, _) = split_pair();
        debian.handle(Event::PeerAlive { now_ms: 7000 });
        assert!(view_of(&debian.handle(Event::Tick { now_ms: 7000 })).is_some());
    }

    #[test]
    fn split_survives_one_lost_view() {
        let (mut debian, mut laptop) = split_pair();
        // 7000's view is lost.
        debian.handle(Event::PeerAlive { now_ms: 7000 });
        debian.handle(Event::Tick { now_ms: 7000 });
        debian.handle(Event::PeerAlive { now_ms: 7900 });
        debian.handle(Event::Tick { now_ms: 7900 });
        assert!(debian.main_conflict());
        exchange_until(&mut debian, &mut laptop, &[8000]);
        assert!(debian.main_conflict());
    }

    #[test]
    fn split_ends_when_the_views_stop() {
        let (mut debian, _) = split_pair();
        for now_ms in [7000, 8000] {
            debian.handle(Event::PeerAlive { now_ms });
            debian.handle(Event::Tick { now_ms });
        }
        assert!(debian.main_conflict());
        debian.handle(Event::PeerAlive { now_ms: 6000 + VIEW_TIMEOUT_MS + 1 });
        debian.handle(Event::Tick { now_ms: 6000 + VIEW_TIMEOUT_MS + 1 });
        assert!(!debian.main_conflict());
        // Local, and free to cross again.
        debian.handle(Event::EdgeCrossed { x: 1, y: 2, from_peer: false });
        assert_eq!(debian.active(), 1);
    }

    #[test]
    fn split_ends_when_main_changes_or_the_link_drops() {
        let (mut debian, _) = split_pair();
        debian.handle(Event::SetMain(0));
        assert!(debian.main_conflict());
        debian.handle(Event::SetMain(1));
        assert!(!debian.main_conflict());

        let (mut debian, _) = split_pair();
        debian.handle(Event::Tick { now_ms: 6000 + LINK_TIMEOUT_MS + 1 });
        assert!(!debian.online());
        assert!(!debian.main_conflict());
    }

    #[test]
    fn panic_still_works_during_a_split() {
        let (mut debian, _) = split_pair();
        let out = debian.handle(Event::Panic);
        assert!(debian.panicked());
        assert!(out.actions.contains(&Action::SendPanic));
    }

    #[test]
    fn panic_during_a_split_cannot_end_it_on_one_side_only() {
        let (mut a, mut b) = split_pair();
        // F12 on A reaches B, so both are panicked.
        a.handle(Event::Panic);
        b.handle(Event::PeerPanic);
        assert!(a.panicked() && b.panicked());
        // F11 on A, inside B's last view's timeout, ends A's panic and tells B nothing.
        exchange(&mut a, &mut b, 7000);
        assert!(a.handle(Event::ManualSwitch).actions.is_empty());
        assert!(!a.panicked() && b.panicked());

        for now_ms in (8000..=60_000).step_by(1000) {
            exchange(&mut a, &mut b, now_ms);
            // A pushes on the edge at every step, and neither device gets anywhere.
            let crossing = Event::EdgeCrossed { x: 1, y: 2, from_peer: false };
            assert!(a.handle(crossing).actions.is_empty());
            assert!(a.main_conflict() && b.main_conflict());
            assert_eq!((a.active(), b.active()), (0, 1));
            assert_eq!(a.blocking, (false, false));
            assert_eq!(b.blocking, (false, false));
        }
        assert!(!key(&mut a, 30, true).swallow);
        assert!(!key(&mut b, 30, true).swallow);
    }

    #[test]
    fn panicked_main_outside_a_conflict_stays_silent() {
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::Panic);
        for now_ms in [2000, 3000, 4000] {
            r.handle(Event::PeerAlive { now_ms });
            assert_eq!(view_of(&r.handle(Event::Tick { now_ms })), None);
        }
    }

    #[test]
    fn peer_driven_crossing_ignores_this_devices_mouse_sharing() {
        // Debian's mouse is on the laptop, whose own mouse is not shared.
        let mut laptop = Router::new(RouterConfig {
            sharing: Sharing { mouse: false, keyboard: true },
            ..cfg(1)
        });
        online(&mut laptop);
        laptop.handle(Event::PeerActiveView { active: 1 });
        let moved = laptop.handle(Event::PeerMove { dx: -5, dy: 0 });
        assert_eq!(moved.actions, vec![Action::InjectMove { dx: -5, dy: 0 }]);

        // The laptop's own trackpad on that edge stays put.
        let own = laptop.handle(Event::EdgeCrossed { x: 0, y: 9, from_peer: false });
        assert!(own.actions.is_empty());
        assert_eq!(laptop.active(), 1);

        let out = laptop.handle(Event::EdgeCrossed { x: 0, y: 9, from_peer: true });
        assert!(out.actions.contains(&Action::SendFocus { target: 0, x: 0, y: 9 }));
        assert_eq!(laptop.active(), 0);
    }

    fn held(keys: &[u16], buttons: &[u8]) -> HeldInput {
        let mut held = HeldInput::default();
        keys.iter().for_each(|key| held.set_key(*key, true));
        buttons.iter().for_each(|button| held.set_button(*button, true));
        held
    }
    /// A local router with the peer's input coming in.
    fn injecting_router() -> Router {
        let mut r = router(0);
        online(&mut r);
        r
    }
    fn peer_key(r: &mut Router, scancode: u16, pressed: bool) -> Routed {
        r.handle(Event::PeerKey { scancode, pressed })
    }

    #[test]
    fn what_is_forwarded_is_reported_until_it_is_released() {
        let mut r = forwarding_router();
        assert_eq!(r.held_forwarded(), HeldInput::default());
        key(&mut r, 30, true);
        r.handle(Event::LocalButton { button: 1, pressed: true });
        assert_eq!(r.held_forwarded(), held(&[30], &[1]));
        key(&mut r, 30, false);
        r.handle(Event::LocalButton { button: 1, pressed: false });
        assert_eq!(r.held_forwarded(), HeldInput::default());
    }

    #[test]
    fn a_key_the_peer_no_longer_holds_is_released() {
        let mut r = injecting_router();
        peer_key(&mut r, 30, true);
        r.handle(Event::PeerButton { button: 1, pressed: true });
        // Every copy of the releases was lost, and the heartbeat says nothing is held.
        let out = r.handle(Event::PeerHeld(HeldInput::default()));
        assert!(out.actions.contains(&Action::InjectKey { scancode: 30, pressed: false }));
        assert!(out.actions.contains(&Action::InjectButton { button: 1, pressed: false }));
        // And only once.
        assert!(r.handle(Event::PeerHeld(HeldInput::default())).actions.is_empty());
    }

    #[test]
    fn a_key_the_peer_still_holds_is_left_alone() {
        let mut r = injecting_router();
        peer_key(&mut r, 30, true);
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
    }

    #[test]
    fn a_missed_press_waits_for_a_second_report() {
        let mut r = injecting_router();
        // One report could be about to be overtaken by a release, or be a tap that's already over.
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
        let out = r.handle(Event::PeerHeld(held(&[30], &[])));
        assert_eq!(out.actions, vec![Action::InjectKey { scancode: 30, pressed: true }]);
        // It counts as held from now on, so that it's let go like any other.
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
        let out = r.handle(Event::PeerHeld(HeldInput::default()));
        assert_eq!(out.actions, vec![Action::InjectKey { scancode: 30, pressed: false }]);
    }

    #[test]
    fn a_report_in_between_says_the_key_was_let_go() {
        let mut r = injecting_router();
        r.handle(Event::PeerHeld(held(&[30], &[])));
        r.handle(Event::PeerHeld(HeldInput::default()));
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
    }

    #[test]
    fn a_release_seen_between_reports_cancels_the_press() {
        let mut r = injecting_router();
        r.handle(Event::PeerHeld(held(&[30], &[])));
        peer_key(&mut r, 30, false);
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
    }

    #[test]
    fn auto_repeat_presses_are_not_doubled_by_a_report() {
        let mut r = injecting_router();
        r.handle(Event::PeerHeld(held(&[30], &[])));
        // A repeat is the same as a press on the wire, and proves the key is down.
        peer_key(&mut r, 30, true);
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
    }

    #[test]
    fn a_missed_button_is_pressed_only_where_the_pointer_was() {
        let mut r = injecting_router();
        r.handle(Event::PeerHeld(held(&[], &[1])));
        let out = r.handle(Event::PeerHeld(held(&[], &[1])));
        assert_eq!(out.actions, vec![Action::InjectButton { button: 1, pressed: true }]);

        // Once the pointer has moved, a press would land on something else.
        let mut r = injecting_router();
        r.handle(Event::PeerHeld(held(&[], &[1])));
        r.handle(Event::PeerMove { dx: 40, dy: 0 });
        assert!(r.handle(Event::PeerHeld(held(&[], &[1]))).actions.is_empty());
    }

    #[test]
    fn reports_do_nothing_while_this_device_is_not_being_driven() {
        let mut r = forwarding_router();
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
        // Nor does a half-seen key carry over to when it is.
        let mut r = injecting_router();
        r.handle(Event::PeerHeld(held(&[30], &[])));
        r.handle(Event::Panic);
        r.handle(Event::ManualSwitch);
        r.handle(Event::ManualSwitch);
        r.handle(Event::Tick { now_ms: 1000 + SWITCH_RACE_MS + 1 });
        assert_eq!(r.active(), 0);
        assert!(r.handle(Event::PeerHeld(held(&[30], &[]))).actions.is_empty());
    }

    #[test]
    fn nothing_carries_over_a_release_of_everything() {
        for lose in [Event::Panic, Event::PeerPanic, Event::Tick { now_ms: 1000 + LINK_TIMEOUT_MS + 1 }] {
            let mut r = injecting_router();
            peer_key(&mut r, 30, true);
            let out = r.handle(lose);
            assert!(out.actions.contains(&Action::ReleaseHeld));
            r.handle(Event::ManualSwitch);
            r.handle(Event::PeerAlive { now_ms: 5000 });
            // The key was let go with everything else, so no report has anything to release.
            let out = r.handle(Event::PeerHeld(HeldInput::default()));
            assert!(!out.actions.iter().any(|a| matches!(a, Action::InjectKey { .. })), "{lose:?}");
        }
    }

    /// Router 0 (main) forwarding to device 1, which has been answering its heartbeats.
    fn answered_router() -> Router {
        let mut r = forwarding_router();
        r.handle(Event::PeerHeard { sent_ms: 1000, panicked: false });
        r
    }

    #[test]
    fn control_goes_back_after_a_dropout() {
        let mut r = answered_router();
        let at = 1000 + LINK_TIMEOUT_MS + 1;
        r.handle(Event::Tick { now_ms: at });
        assert!(!r.online());
        assert_eq!(r.active(), 0);
        assert!(r.returning());
        // Nothing is blocked or forwarded while it lasts.
        assert!(!key(&mut r, 30, true).swallow);
        key(&mut r, 30, false);

        // The link is back, but only an answer proves that the peer can hear this device.
        let out = r.handle(Event::PeerAlive { now_ms: 3000 });
        assert!(r.online());
        assert_eq!(r.active(), 0);
        assert!(!out.actions.iter().any(|a| matches!(a, Action::SendActiveView { .. } | Action::SetBlocking { .. })));
        assert!(!key(&mut r, 31, true).swallow);
        key(&mut r, 31, false);
        // An answer to a heartbeat from before it returned proves nothing.
        r.handle(Event::PeerHeard { sent_ms: 2900, panicked: false });
        assert_eq!(r.active(), 0);

        let out = r.handle(Event::PeerHeard { sent_ms: 3100, panicked: false });
        assert_eq!(r.active(), 1);
        assert!(!r.returning());
        assert!(out.actions.contains(&Action::SendActiveView { active: 1 }));
        assert!(out.actions.contains(&Action::SetBlocking { mouse: true, keyboard: true }));
        assert!(key(&mut r, 30, true).swallow);
    }

    #[test]
    fn a_gap_seen_only_when_the_link_returns_is_a_dropout_too() {
        let mut r = answered_router();
        r.handle(Event::PeerAlive { now_ms: 1000 + LINK_TIMEOUT_MS + 1 });
        assert!(r.online());
        assert_eq!(r.active(), 0);
        assert!(r.returning());
        r.handle(Event::PeerHeard { sent_ms: 2500, panicked: false });
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn a_second_dropout_before_the_proof_keeps_control_returning() {
        let mut r = answered_router();
        r.handle(Event::Tick { now_ms: 2400 });
        r.handle(Event::PeerAlive { now_ms: 3000 });
        r.handle(Event::Tick { now_ms: 3000 + LINK_TIMEOUT_MS + 1 });
        assert!(!r.online());
        assert!(r.returning());
    }

    #[test]
    fn a_long_dropout_leaves_control_where_it_is() {
        let mut r = answered_router();
        r.handle(Event::Tick { now_ms: 2400 });
        r.handle(Event::Tick { now_ms: 2400 + RESUME_WINDOW_MS });
        assert!(r.returning());
        r.handle(Event::Tick { now_ms: 2401 + RESUME_WINDOW_MS });
        assert!(!r.returning());
        let now_ms = 2500 + RESUME_WINDOW_MS;
        let out = r.handle(Event::PeerAlive { now_ms });
        assert!(out.actions.contains(&Action::SendActiveView { active: 0 }));
        r.handle(Event::PeerHeard { sent_ms: now_ms + 100, panicked: false });
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn release_all_input_during_a_dropout_keeps_control_here() {
        let mut r = answered_router();
        r.handle(Event::Tick { now_ms: 2400 });
        r.handle(Event::Panic);
        assert!(!r.returning());
        r.handle(Event::PeerAlive { now_ms: 3000 });
        r.handle(Event::PeerHeard { sent_ms: 3100, panicked: false });
        assert_eq!(r.active(), 0);
        assert!(!key(&mut r, 30, true).swallow);
    }

    #[test]
    fn a_peer_that_released_all_input_is_not_given_control_back() {
        let mut r = answered_router();
        r.handle(Event::Tick { now_ms: 2400 });
        r.handle(Event::PeerAlive { now_ms: 3000 });
        r.handle(Event::PeerHeard { sent_ms: 3100, panicked: true });
        assert_eq!(r.active(), 0);
        assert!(!r.returning());
        // Views carry on, now that nothing is waiting.
        let out = r.handle(Event::Tick { now_ms: 3000 + ACTIVE_VIEW_INTERVAL_MS });
        assert!(out.actions.contains(&Action::SendActiveView { active: 0 }));
    }

    #[test]
    fn a_choice_made_after_the_link_returns_stands() {
        // Pushing across here sends control over anyway, and needs no proof.
        let mut r = answered_router();
        r.handle(Event::Tick { now_ms: 2400 });
        r.handle(Event::PeerAlive { now_ms: 3000 });
        r.handle(Event::EdgeCrossed { x: 1, y: 1, from_peer: false });
        assert_eq!(r.active(), 1);
        assert!(!r.returning());
        // The peer asking for control back is a choice too.
        let mut r = answered_router();
        r.handle(Event::Tick { now_ms: 2400 });
        r.handle(Event::PeerAlive { now_ms: 3000 });
        r.handle(Event::PeerFocus { target: 0, x: KEEP_CURSOR, y: KEEP_CURSOR });
        assert!(!r.returning());
        r.handle(Event::PeerHeard { sent_ms: 3100, panicked: false });
        assert_eq!(r.active(), 0);
    }

    #[test]
    fn an_older_peer_gets_the_old_behaviour() {
        let mut r = forwarding_router();
        r.handle(Event::Tick { now_ms: 2400 });
        assert_eq!(r.active(), 0);
        assert!(!r.returning());
        let out = r.handle(Event::PeerAlive { now_ms: 3000 });
        assert!(out.actions.contains(&Action::SendActiveView { active: 0 }));
    }

    #[test]
    fn control_on_this_device_stays_here_after_a_dropout() {
        let mut r = router(0);
        online(&mut r);
        r.handle(Event::PeerHeard { sent_ms: 1000, panicked: false });
        r.handle(Event::Tick { now_ms: 2400 });
        assert!(!r.returning());
        let out = r.handle(Event::PeerAlive { now_ms: 3000 });
        assert!(out.actions.contains(&Action::SendActiveView { active: 0 }));
    }

    #[test]
    fn a_non_main_device_hands_back_on_main_view_and_waits_for_nothing_else() {
        let mut r = adopted_router(cfg(1));
        r.handle(Event::Tick { now_ms: 2400 });
        assert_eq!(r.active(), 1);
        assert!(r.returning());
        r.handle(Event::PeerAlive { now_ms: 3000 });
        assert_eq!(r.active(), 1);
        assert!(!key(&mut r, 30, true).swallow);
        key(&mut r, 30, false);
        r.handle(Event::PeerActiveView { active: 0 });
        assert_eq!(r.active(), 0);
        assert!(!r.returning());
        assert!(key(&mut r, 30, true).swallow);
    }

    #[test]
    fn a_pair_finds_its_way_back_after_a_dropout() {
        let (mut main, mut other) = (router(0), router(1));
        online(&mut main);
        online(&mut other);
        main.handle(Event::PeerHeard { sent_ms: 1000, panicked: false });
        let view = view_of(&main.handle(Event::EdgeCrossed { x: 0, y: 0, from_peer: false }));
        assert_eq!(view, None);
        other.handle(Event::PeerFocus { target: 1, x: 0, y: 0 });
        assert_eq!((main.active(), other.active()), (1, 1));

        main.handle(Event::Tick { now_ms: 2400 });
        other.handle(Event::Tick { now_ms: 2400 });
        assert_eq!((main.active(), other.active()), (0, 1));
        assert!(!main.online() && !other.online());
        main.handle(Event::PeerAlive { now_ms: 4400 });
        other.handle(Event::PeerAlive { now_ms: 4400 });
        // Each on its own until main knows it's heard, so no key goes astray on either.
        assert_eq!((main.active(), other.active()), (0, 1));
        let out = main.handle(Event::PeerHeard { sent_ms: 4500, panicked: false });
        other.handle(view_of(&out).unwrap());
        assert_eq!((main.active(), other.active()), (1, 1));
        assert!(key(&mut main, 30, true).swallow);
        assert!(!key(&mut other, 30, true).swallow);
    }

    #[test]
    fn a_peer_that_stops_hearing_this_device_loses_its_input() {
        let mut r = answered_router();
        // Its heartbeats keep arriving, but none answers one of ours.
        for now_ms in [2000, 3000] {
            r.handle(Event::PeerAlive { now_ms });
            r.handle(Event::Tick { now_ms });
        }
        assert_eq!(r.active(), 1);
        r.handle(Event::PeerAlive { now_ms: 3500 });
        let out = r.handle(Event::Tick { now_ms: 1000 + VIEW_TIMEOUT_MS + 1 });
        assert!(r.online());
        assert_eq!(r.active(), 0);
        assert!(r.returning());
        assert!(out.actions.contains(&Action::SetBlocking { mouse: false, keyboard: false }));
        assert!(!key(&mut r, 30, true).swallow);
        key(&mut r, 30, false);
        // Only a newer answer brings it back.
        r.handle(Event::PeerHeard { sent_ms: 1000, panicked: false });
        assert_eq!(r.active(), 0);
        r.handle(Event::PeerHeard { sent_ms: 3600, panicked: false });
        assert_eq!(r.active(), 1);
    }

    #[test]
    fn answers_keep_control_with_the_peer() {
        let mut r = answered_router();
        for now_ms in (2000..10_000).step_by(1000) {
            r.handle(Event::PeerAlive { now_ms });
            r.handle(Event::PeerHeard { sent_ms: now_ms - 100, panicked: false });
            r.handle(Event::Tick { now_ms });
        }
        assert_eq!(r.active(), 1);
    }

    fn span(edge: SwitchEdge, start: i32, end: i32) -> EdgeSpan {
        EdgeSpan { edge, start, end }
    }

    #[test]
    fn crosses_after_resistance() {
        let mut t = EdgeTracker::new(55);
        let s = span(SwitchEdge::Left, 0, 1079);
        assert!(!t.push(s, 0, 500, 1920, 1080, -30, 0, false));
        assert!(t.push(s, 0, 500, 1920, 1080, -30, 0, false));
    }

    #[test]
    fn moving_away_resets_push() {
        let mut t = EdgeTracker::new(55);
        let s = span(SwitchEdge::Left, 0, 1079);
        assert!(!t.push(s, 0, 500, 1920, 1080, -30, 0, false));
        assert!(!t.push(s, 0, 500, 1920, 1080, 5, 0, false));
        assert!(!t.push(s, 0, 500, 1920, 1080, -30, 0, false));
    }

    #[test]
    fn no_cross_outside_span() {
        let mut t = EdgeTracker::new(55);
        let s = span(SwitchEdge::Bottom, 0, 500);
        for _ in 0..10 {
            assert!(!t.push(s, 900, 1079, 1920, 1080, 0, 30, false));
        }
    }

    #[test]
    fn no_cross_with_button_held() {
        let mut t = EdgeTracker::new(55);
        let s = span(SwitchEdge::Left, 0, 1079);
        for _ in 0..10 {
            assert!(!t.push(s, 0, 500, 1920, 1080, -30, 0, true));
        }
    }

    #[test]
    fn releasing_drag_needs_fresh_push() {
        let mut t = EdgeTracker::new(55);
        let s = span(SwitchEdge::Left, 0, 1079);
        for _ in 0..10 {
            assert!(!t.push(s, 0, 500, 1920, 1080, -30, 0, true));
        }
        assert!(!t.push(s, 0, 500, 1920, 1080, -1, 0, false));
    }
}
