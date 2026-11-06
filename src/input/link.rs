//! What each device works out from the packets it gets: which of them are too late to act on, how
//! the link is doing, and what to put in its own heartbeat. Pure, so that both backends share it.
//! Time comes in as `now_ms`, on the same clock the router gets.

use crate::protocol::{HeldInput, LinkState, Packet, Payload, PressEvent, FIRST_BUTTON, LINK_PROTOCOL};
use crate::router::{Event, Router};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// More packets than could ever be in flight at once. A jump this big is a restart, not lateness.
const MAX_LATE_PACKETS: u64 = 1 << 16;
/// A running peer sends something new with every heartbeat. Hearing only old numbers for this
/// long means it started again from a lower one.
const RESTART_AFTER_MS: u64 = 1000;
const LOSS_WINDOW_MS: u64 = 2000;
/// How much of each new reading goes into the figure shown.
const SMOOTHING: f32 = 0.4;
/// How many of our own heartbeats are remembered, to match with answers.
const BEATS_KEPT: usize = 32;
/// How long a peer that says nothing of its version is given, with the link up.
const VERSION_WAIT_MS: u64 = 3000;
/// How long the app keeps saying the other device can't be read after the last such packet.
const UNREADABLE_FOR_MS: u64 = 10_000;
/// Figures that depend on the other device hearing this one last this long without an answer. A
/// heartbeat and a few lost answers fit well inside it.
const ANSWER_FOR_MS: u64 = 2500;
/// How many presses each packet repeats, counting its own.
const HISTORY: usize = 8;
/// Presses older than this aren't repeated. They would land too late to be what was meant.
const HISTORY_MS: u64 = 1000;
/// The longest a press waits for one before it. Long enough for a copy that's a little behind,
/// short enough not to be felt, and a clean link never waits at all.
const HOLD_MS: u64 = 30;
const MAX_WAITING: usize = 64;

/// True when `seq` came after `other`. Half the number space each way, so that a wrap counts as
/// going forward.
fn is_newer(seq: u64, other: u64) -> bool {
    (seq.wrapping_sub(other) as i64) > 0
}

/// Packets that must stay in order among themselves. Each kind has its own place in the line, so
/// that a key isn't dropped for being overtaken by a mouse movement, which it has nothing to do with.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stream {
    /// A key or button going down. `u16` says which, with buttons after the keys.
    Press(u16),
    Release(u16),
    /// The held state, which has to agree with the two above.
    Held,
    Motion,
    Focus,
}

fn stream_of(payload: &Payload) -> Option<Stream> {
    let press = |input, pressed| Some(if pressed { Stream::Press(input) } else { Stream::Release(input) });
    match *payload {
        Payload::KeyEvent { scancode, pressed } => press(scancode, pressed),
        Payload::MouseButton { button, pressed } => press(FIRST_BUTTON + u16::from(button), pressed),
        Payload::LinkState(_) => Some(Stream::Held),
        Payload::MouseMoveDelta { .. } | Payload::MouseWheel { .. } => Some(Stream::Motion),
        Payload::FocusSwitchAt { .. } | Payload::ActiveView { .. } => Some(Stream::Focus),
        _ => None,
    }
}

/// Moves `newest` up to `seq`, and says if that was a move.
fn advance(newest: &mut Option<u64>, seq: u64) -> bool {
    let newer = newest.is_none_or(|newest| is_newer(seq, newest));
    if newer {
        *newest = Some(seq);
    }
    newer
}

/// Keeps the newest sequence number seen on each stream, and notices the peer starting again.
#[derive(Default)]
struct PacketOrder {
    newest: Option<u64>,
    /// Since when everything has been older than `newest`.
    behind_since: Option<u64>,
    /// When the last packet of any kind came.
    last_packet: Option<u64>,
    /// The newest key or button event, or held state.
    presses: Option<u64>,
    /// The newest event of each key and button.
    inputs: HashMap<u16, u64>,
    held: Option<u64>,
    motion: Option<u64>,
    focus: Option<u64>,
}

impl PacketOrder {
    /// Takes note of any packet at all. True if it shows that the peer has restarted.
    fn note(&mut self, seq: u64, now_ms: u64) -> bool {
        // Silence isn't hearing old numbers, so it can't count towards a restart.
        if self.last_packet.is_none_or(|last| now_ms.saturating_sub(last) >= RESTART_AFTER_MS) {
            self.behind_since = None;
        }
        self.last_packet = Some(now_ms);
        let Some(newest) = self.newest else {
            self.newest = Some(seq);
            return false;
        };
        let ahead = seq.wrapping_sub(newest) as i64;
        let behind_too_long = ahead < 0 && now_ms.saturating_sub(*self.behind_since.get_or_insert(now_ms)) >= RESTART_AFTER_MS;
        let restarted = ahead.unsigned_abs() > MAX_LATE_PACKETS || behind_too_long;
        if restarted {
            *self = PacketOrder { newest: Some(seq), last_packet: Some(now_ms), ..PacketOrder::default() };
        } else if ahead > 0 {
            self.newest = Some(seq);
            self.behind_since = None;
        }
        restarted
    }

    /// False for a packet that something newer has already overtaken, which is also what a
    /// duplicate looks like. A press gives way to any other press, or two keys would land the
    /// wrong way round. A release only gives way to news of its own key: letting go is always safe,
    /// and dropping it would leave the key down until the next heartbeat. Nothing gets past a
    /// report that's newer, as that already says how things stand.
    fn fresh(&mut self, stream: Stream, seq: u64) -> bool {
        match stream {
            Stream::Motion => advance(&mut self.motion, seq),
            Stream::Focus => advance(&mut self.focus, seq),
            Stream::Held => advance(&mut self.presses, seq) && advance(&mut self.held, seq),
            Stream::Press(input) => {
                let fresh = advance(&mut self.presses, seq);
                if fresh {
                    self.inputs.insert(input, seq);
                }
                fresh
            }
            Stream::Release(input) => {
                let known = self.inputs.get(&input).copied();
                let fresh = known.is_none_or(|newest| is_newer(seq, newest)) && self.held.is_none_or(|held| is_newer(seq, held));
                if fresh {
                    self.inputs.insert(input, seq);
                    advance(&mut self.presses, seq);
                }
                fresh
            }
        }
    }
}

/// Every packet takes the next sequence number, so a gap in them is what went missing on the way.
/// That counts all traffic, not just heartbeats, and a dropout shows as one gap when it ends.
#[derive(Default)]
struct LossMeter {
    /// The newest number when this window opened, and when that was.
    start: Option<(u64, u64)>,
    newest: u64,
    got: u64,
    percent: Option<f32>,
}

impl LossMeter {
    fn count(&mut self, seq: u64, now_ms: u64) {
        let Some((first, opened)) = self.start else {
            self.start = Some((seq, now_ms));
            self.newest = seq;
            self.got = 0;
            return;
        };
        self.got += 1;
        if is_newer(seq, self.newest) {
            self.newest = seq;
        }
        if now_ms.saturating_sub(opened) >= LOSS_WINDOW_MS {
            let due = self.newest.wrapping_sub(first);
            if due > 0 {
                let lost = due.saturating_sub(self.got) as f32 * 100.0 / due as f32;
                self.percent = Some(self.percent.map_or(lost, |was| was + (lost - was) * SMOOTHING));
            }
            self.start = Some((self.newest, now_ms));
            // More than was due means late ones from before, which the last window counted as lost.
            self.got = self.got.saturating_sub(due);
        }
    }
}

/// Numbers the keys and buttons this device forwards. Motion isn't in here: one lost movement
/// doesn't matter, and the next one takes its place.
#[derive(Default)]
pub struct PressLog {
    newest: u32,
    /// The latest ones, with when each was made.
    recent: VecDeque<(PressEvent, u64)>,
    reliable: bool,
}

impl PressLog {
    /// Says if the other device can read numbered presses. Until then none are made.
    pub fn set_reliable(&mut self, reliable: bool) {
        self.reliable = reliable;
    }

    /// The number of the last press made. A report takes this before it reads what's held.
    pub fn newest(&self) -> u32 {
        self.newest
    }

    /// The packet to send for `event`, or `None` for a peer that needs the plain one.
    pub fn record(&mut self, event: PressEvent, now_ms: u64) -> Option<Payload> {
        if !self.reliable {
            return None;
        }
        self.newest = self.newest.wrapping_add(1);
        self.recent.push_back((event, now_ms));
        if self.recent.len() > HISTORY {
            self.recent.pop_front();
        }
        self.packet(now_ms)
    }

    /// What's worth saying again with a heartbeat, in case a whole burst of copies was lost.
    pub fn tail(&mut self, now_ms: u64) -> Option<Payload> {
        self.packet(now_ms)
    }

    fn packet(&mut self, now_ms: u64) -> Option<Payload> {
        while self.recent.front().is_some_and(|(_, at)| now_ms.saturating_sub(*at) >= HISTORY_MS) {
            self.recent.pop_front();
        }
        let events: Vec<PressEvent> = self.recent.iter().map(|(event, _)| *event).collect();
        (!events.is_empty()).then_some(Payload::Presses { newest: self.newest, events })
    }

    /// Typing from before a dropout must not turn up after it.
    pub fn forget(&mut self) {
        self.recent.clear();
    }
}

/// Puts numbered presses back in order, without repeats. Nothing waits unless there's a gap.
#[derive(Default)]
struct PressQueue {
    /// The number due next. `None` until one is heard.
    next: Option<u32>,
    /// Each with its number, and when it came.
    waiting: Vec<(u32, PressEvent, u64)>,
    gap_since: Option<u64>,
    stats: PressStats,
}

/// How much waiting for gaps there has been.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct PressStats {
    /// Presses that had to wait, and the longest and total wait of any, in ms.
    pub held: u64,
    pub longest_ms: u64,
    pub total_ms: u64,
    /// Gaps given up on after the cap.
    pub skipped: u64,
}

impl PressQueue {
    fn take(&mut self, newest: u32, events: &[PressEvent], now_ms: u64) -> Vec<PressEvent> {
        // The first ones heard start where the packet does: a packet carries nothing old.
        let first = newest.wrapping_sub((events.len() as u32).saturating_sub(1));
        let next = *self.next.get_or_insert(first);
        for (at, event) in events.iter().enumerate() {
            let order = first.wrapping_add(at as u32);
            let due = (order.wrapping_sub(next) as i32) >= 0;
            if due && !self.waiting.iter().any(|(waiting, ..)| *waiting == order) {
                self.waiting.push((order, *event, now_ms));
            }
        }
        let mut ready = self.drain(now_ms);
        self.gap_since = if self.waiting.is_empty() { None } else { self.gap_since.or(Some(now_ms)) };
        if self.waiting.len() > MAX_WAITING {
            ready.extend(self.skip_gap(now_ms));
        }
        ready
    }

    /// Takes off everything that's next in line.
    fn drain(&mut self, now_ms: u64) -> Vec<PressEvent> {
        let mut ready = Vec::new();
        let Some(mut next) = self.next else { return ready };
        while let Some(at) = self.waiting.iter().position(|(order, ..)| *order == next) {
            let (_, event, came) = self.waiting.swap_remove(at);
            ready.push(event);
            next = next.wrapping_add(1);
            let waited = now_ms.saturating_sub(came);
            if waited > 0 {
                self.stats.held += 1;
                self.stats.total_ms += waited;
                self.stats.longest_ms = self.stats.longest_ms.max(waited);
            }
        }
        self.next = Some(next);
        ready
    }

    fn skip_gap(&mut self, now_ms: u64) -> Vec<PressEvent> {
        let Some(next) = self.next else { return Vec::new() };
        let lowest = self.waiting.iter().map(|(order, ..)| *order).min_by_key(|order| order.wrapping_sub(next));
        self.next = lowest.or(self.next);
        self.stats.skipped += 1;
        let ready = self.drain(now_ms);
        self.gap_since = (!self.waiting.is_empty()).then_some(now_ms);
        ready
    }

    fn due(&mut self, now_ms: u64) -> Vec<PressEvent> {
        match self.gap_since {
            Some(since) if now_ms.saturating_sub(since) >= HOLD_MS => self.skip_gap(now_ms),
            _ => Vec::new(),
        }
    }

    /// The number of the last press acted on.
    fn applied(&self) -> Option<u32> {
        self.next.map(|next| next.wrapping_sub(1))
    }

    fn forget(&mut self) {
        *self = PressQueue { stats: self.stats, ..PressQueue::default() };
    }
}

/// The router event for a press from the peer.
pub fn press_event(event: PressEvent) -> Event {
    match event.input.checked_sub(FIRST_BUTTON) {
        Some(button) => Event::PeerButton { button: button as u8, pressed: event.pressed },
        None => Event::PeerKey { scancode: event.input, pressed: event.pressed },
    }
}

/// What this device can say of the other one's version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerVersion {
    /// Nothing heard yet to go on.
    #[default]
    Unknown,
    Same,
    /// It's heard, and sends no `LinkState`.
    Older,
    /// It sends what this device can't read, or a `LinkState` of another protocol.
    Other,
}

/// How the link looks from here, for the app.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkQuality {
    /// False until there is a figure to show, and whenever the link is down.
    pub measured: bool,
    /// Percent of the peer's packets not arriving here, and of ours not arriving there, once
    /// there's something to go on.
    pub loss_in: Option<f32>,
    pub loss_out: Option<f32>,
    pub round_trip_ms: Option<u32>,
    /// The other device can be heard, and doesn't answer: it can't hear this one.
    pub one_way: bool,
    /// Control waits to go back to the other device.
    pub returning: bool,
    /// Keys and buttons the other device has down here.
    pub held_here: u32,
    pub peer_version: PeerVersion,
}

/// One device's side of the link to the other.
#[derive(Default)]
pub struct PeerLink {
    order: PacketOrder,
    loss: LossMeter,
    beat: u32,
    /// Our recent heartbeats, and when each was sent.
    sent: VecDeque<(u32, u64)>,
    /// The peer's newest heartbeat, and when it came.
    heard: Option<(u32, u64)>,
    round_trip_ms: Option<f32>,
    /// The latest heartbeat the peer has answered, and when an answer last moved on.
    answer: Option<(u32, u64)>,
    loss_out: Option<f32>,
    presses: PressQueue,
    protocol: Option<u16>,
    first_packet: Option<u64>,
    unreadable_at: Option<u64>,
    online: bool,
}

impl PeerLink {
    /// Takes note of a packet from the peer. False if it's input or focus that's too late to act on.
    pub fn arrived(&mut self, packet: &Packet, now_ms: u64) -> bool {
        if self.order.note(packet.seq, now_ms) {
            self.forget();
        }
        self.first_packet.get_or_insert(now_ms);
        self.loss.count(packet.seq, now_ms);
        stream_of(&packet.payload).is_none_or(|stream| self.order.fresh(stream, packet.seq))
    }

    /// Takes in a `Presses` packet, and returns what's ready to act on, in order.
    pub fn take_presses(&mut self, newest: u32, events: &[PressEvent], now_ms: u64) -> Vec<PressEvent> {
        self.presses.take(newest, events, now_ms)
    }

    /// Presses that have waited long enough for one that isn't coming.
    pub fn due_presses(&mut self, now_ms: u64) -> Vec<PressEvent> {
        self.presses.due(now_ms)
    }

    #[cfg(test)]
    pub fn press_stats(&self) -> PressStats {
        self.presses.stats
    }

    /// A packet from the peer that starts right and can't be read.
    pub fn unreadable(&mut self, now_ms: u64) {
        self.unreadable_at = Some(now_ms);
    }

    /// Takes in a `LinkState` that passed `arrived`, and returns what the router should hear of it.
    pub fn heard_state(&mut self, state: &LinkState, now_ms: u64) -> Vec<Event> {
        self.protocol = Some(state.protocol);
        if state.protocol != LINK_PROTOCOL {
            return Vec::new();
        }
        self.heard = Some((state.beat, now_ms));
        self.loss_out = Some(f32::from(state.loss_tenths) / 10.0);
        let mut events = Vec::new();
        // Behind what's been acted on, it knows less than this device does.
        if self.presses.applied().is_none_or(|applied| (state.press_order.wrapping_sub(applied) as i32) >= 0) {
            events.push(Event::PeerHeld(state.held));
        }
        if let Some((_, sent_ms)) = self.sent.iter().find(|(beat, _)| *beat == state.echo).copied() {
            // The same heartbeat answered again says nothing new about the way there.
            if self.answer.is_none_or(|(beat, _)| beat != state.echo) {
                self.answer = Some((state.echo, now_ms));
                let took = now_ms.saturating_sub(sent_ms).saturating_sub(u64::from(state.echo_age_ms)) as f32;
                self.round_trip_ms = Some(self.round_trip_ms.map_or(took, |was| was + (took - was) * SMOOTHING));
            }
            events.push(Event::PeerHeard { sent_ms, panicked: state.panicked });
        }
        events
    }

    /// This device's next heartbeat.
    pub fn next_state(&mut self, held: HeldInput, panicked: bool, press_order: u32, now_ms: u64) -> LinkState {
        self.beat = self.beat.wrapping_add(1).max(1);
        self.sent.push_back((self.beat, now_ms));
        if self.sent.len() > BEATS_KEPT {
            self.sent.pop_front();
        }
        let (echo, heard_at) = self.heard.unwrap_or((0, now_ms));
        LinkState {
            protocol: LINK_PROTOCOL,
            beat: self.beat,
            echo,
            echo_age_ms: now_ms.saturating_sub(heard_at).min(u64::from(u32::MAX)) as u32,
            loss_tenths: (self.loss.percent.unwrap_or(0.0) * 10.0).round().clamp(0.0, 1000.0) as u16,
            panicked,
            press_order,
            held,
        }
    }

    /// Says whether the router has the link up. Figures from before a dropout say nothing about
    /// what comes after it, so they go.
    pub fn set_online(&mut self, online: bool) {
        if self.online && !online {
            self.forget();
        }
        self.online = online;
    }

    fn forget(&mut self) {
        self.loss = LossMeter::default();
        self.heard = None;
        self.round_trip_ms = None;
        self.answer = None;
        self.presses.forget();
        self.loss_out = None;
        self.protocol = None;
        self.first_packet = None;
    }

    pub fn peer_version(&self, now_ms: u64) -> PeerVersion {
        let unreadable = self.unreadable_at.is_some_and(|at| now_ms.saturating_sub(at) < UNREADABLE_FOR_MS);
        match self.protocol {
            _ if unreadable => PeerVersion::Other,
            Some(LINK_PROTOCOL) => PeerVersion::Same,
            Some(_) => PeerVersion::Other,
            None if self.first_packet.is_some_and(|at| now_ms.saturating_sub(at) >= VERSION_WAIT_MS) => PeerVersion::Older,
            None => PeerVersion::Unknown,
        }
    }

    pub fn quality(&self, router: &Router, now_ms: u64) -> LinkQuality {
        let answered = self.answer.is_some_and(|(_, at)| now_ms.saturating_sub(at) < ANSWER_FOR_MS);
        let round_trip_ms = self.round_trip_ms.filter(|_| self.online && answered).map(|ms| ms.round() as u32);
        let loss_in = self.loss.percent.filter(|_| self.online);
        let waited = self.first_packet.is_some_and(|at| now_ms.saturating_sub(at) >= ANSWER_FOR_MS);
        LinkQuality {
            measured: loss_in.is_some() || round_trip_ms.is_some(),
            loss_in,
            loss_out: self.loss_out.filter(|_| self.online && answered),
            round_trip_ms,
            one_way: self.online && !answered && waited && self.protocol == Some(LINK_PROTOCOL),
            returning: router.returning(),
            held_here: router.held_here(),
            peer_version: self.peer_version(now_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::NodeId;
    use crate::router::{RouterConfig, Sharing};

    fn router() -> Router {
        let sharing = Sharing { mouse: true, keyboard: true };
        Router::new(RouterConfig { me: 0, peer: 1, main: 0, sharing, pointer_speed: 1.0 })
    }

    fn key(seq: u64) -> Packet {
        Packet::new(seq, NodeId::Debian, Payload::KeyEvent { scancode: 30, pressed: true })
    }
    fn motion(seq: u64) -> Packet {
        Packet::new(seq, NodeId::Debian, Payload::MouseMoveDelta { dx: 1, dy: 0 })
    }
    fn focus(seq: u64) -> Packet {
        Packet::new(seq, NodeId::Debian, Payload::FocusSwitchAt { target_node: NodeId::Debian, x: 0, y: 0 })
    }
    fn heartbeat(seq: u64) -> Packet {
        Packet::new(seq, NodeId::Debian, Payload::Heartbeat)
    }
    fn state(link: &mut PeerLink, seq: u64, held: HeldInput, now_ms: u64) -> Packet {
        Packet::new(seq, NodeId::Debian, Payload::LinkState(link.next_state(held, false, 0, now_ms)))
    }

    fn tap(input: u16, pressed: bool) -> PressEvent {
        PressEvent { input, pressed }
    }

    /// A log that has heard the other device is on this version.
    fn reliable_log() -> PressLog {
        let mut log = PressLog::default();
        log.set_reliable(true);
        log
    }

    fn parts(payload: Option<Payload>) -> (u32, Vec<PressEvent>) {
        match payload {
            Some(Payload::Presses { newest, events }) => (newest, events),
            other => panic!("expected presses: {other:?}"),
        }
    }

    #[test]
    fn an_older_peer_gets_no_numbered_presses() {
        assert!(PressLog::default().record(tap(30, true), 0).is_none());
    }

    #[test]
    fn each_press_carries_the_ones_just_before_it() {
        let mut log = reliable_log();
        assert_eq!(parts(log.record(tap(29, true), 0)), (1, vec![tap(29, true)]));
        assert_eq!(parts(log.record(tap(46, true), 5)), (2, vec![tap(29, true), tap(46, true)]));
        assert_eq!(log.newest(), 2);
    }

    #[test]
    fn what_a_press_carries_is_short_and_recent() {
        let mut log = reliable_log();
        for at in 0..20 {
            log.record(tap(30, at % 2 == 0), at);
        }
        let (newest, events) = parts(log.record(tap(31, true), 20));
        assert_eq!((newest, events.len()), (21, HISTORY));
        log.record(tap(32, true), 20 + HISTORY_MS);
        let (_, events) = parts(log.record(tap(33, true), 21 + HISTORY_MS));
        assert_eq!(events, vec![tap(32, true), tap(33, true)], "typing from a stall ago is left out");
    }

    #[test]
    fn a_heartbeat_repeats_recent_presses_and_only_those() {
        let mut log = reliable_log();
        assert!(log.tail(0).is_none());
        log.record(tap(30, true), 100);
        log.record(tap(30, false), 150);
        assert_eq!(parts(log.tail(400)), (2, vec![tap(30, true), tap(30, false)]));
        assert!(log.tail(150 + HISTORY_MS).is_none());
    }

    #[test]
    fn numbers_carry_on_after_a_dropout_with_nothing_old_repeated() {
        let mut log = reliable_log();
        log.record(tap(30, true), 0);
        log.forget();
        assert!(log.tail(1).is_none());
        assert_eq!(parts(log.record(tap(31, true), 2)), (2, vec![tap(31, true)]));
    }

    /// A link that has seen press 1, so that gaps after it count.
    fn pressed_link() -> PeerLink {
        let mut link = PeerLink::default();
        assert_eq!(link.take_presses(1, &[tap(1, true)], 0), vec![tap(1, true)]);
        link
    }

    #[test]
    fn presses_on_a_clean_link_go_straight_through() {
        let mut link = pressed_link();
        assert_eq!(link.take_presses(2, &[tap(1, true), tap(2, true)], 0), vec![tap(2, true)]);
        assert_eq!(link.take_presses(3, &[tap(1, true), tap(2, true), tap(3, true)], 0), vec![tap(3, true)]);
        assert_eq!(link.due_presses(1000), vec![]);
        assert_eq!(link.press_stats().held, 0);
    }

    #[test]
    fn copies_count_once() {
        let mut link = pressed_link();
        assert_eq!(link.take_presses(2, &[tap(2, true)], 0).len(), 1);
        assert!(link.take_presses(2, &[tap(2, true)], 0).is_empty());
        assert!(link.take_presses(1, &[tap(1, true)], 0).is_empty(), "a straggler");
    }

    #[test]
    fn a_packet_after_a_loss_brings_what_was_lost_in_order() {
        let mut link = pressed_link();
        // Ctrl's packets never came, and C's carries it.
        let got = link.take_presses(3, &[tap(29, true), tap(46, true)], 0);
        assert_eq!(got, vec![tap(29, true), tap(46, true)]);
    }

    #[test]
    fn a_gap_holds_later_presses_until_the_missing_one_comes() {
        let mut link = pressed_link();
        assert!(link.take_presses(3, &[tap(46, true)], 0).is_empty());
        assert_eq!(link.due_presses(HOLD_MS - 1), vec![]);
        let got = link.take_presses(2, &[tap(29, true)], 10);
        assert_eq!(got, vec![tap(29, true), tap(46, true)]);
        assert_eq!(link.press_stats().held, 1);
    }

    #[test]
    fn a_gap_that_stays_gives_way_after_the_cap() {
        let mut link = pressed_link();
        assert!(link.take_presses(3, &[tap(46, true)], 100).is_empty());
        assert!(link.take_presses(4, &[tap(46, false)], 105).len() == 0);
        assert_eq!(link.due_presses(100 + HOLD_MS), vec![tap(46, true), tap(46, false)]);
        assert!(link.take_presses(2, &[tap(29, true)], 200).is_empty(), "too late now");
        assert_eq!(link.take_presses(5, &[tap(30, true)], 201), vec![tap(30, true)]);
    }

    #[test]
    fn the_first_presses_heard_are_all_kept() {
        // The very first press can be overtaken by the second, which carries it.
        let mut link = PeerLink::default();
        let got = link.take_presses(50, &[tap(1, true), tap(1, false), tap(2, true)], 0);
        assert_eq!(got, vec![tap(1, true), tap(1, false), tap(2, true)]);
    }

    #[test]
    fn press_numbers_wrap() {
        let mut link = PeerLink::default();
        link.take_presses(u32::MAX, &[tap(1, true)], 0);
        assert_eq!(link.take_presses(0, &[tap(2, true)], 0), vec![tap(2, true)]);
        assert_eq!(link.take_presses(1, &[tap(3, true)], 0), vec![tap(3, true)]);
        assert!(link.take_presses(u32::MAX, &[tap(1, true)], 0).is_empty());
    }

    #[test]
    fn a_dropout_or_restart_starts_the_numbers_over() {
        let mut link = pressed_link();
        link.set_online(true);
        link.set_online(false);
        link.set_online(true);
        assert_eq!(link.take_presses(9, &[tap(9, true)], 0), vec![tap(9, true)]);
        // A restart shows in the packet numbers.
        link.arrived(&heartbeat(1 << 40), 0);
        link.arrived(&heartbeat(1), 1);
        assert_eq!(link.take_presses(1, &[tap(5, true)], 2), vec![tap(5, true)]);
    }

    #[test]
    fn an_endless_gap_cannot_pile_up() {
        let mut link = pressed_link();
        for order in 3..200u32 {
            link.take_presses(order, &[tap(30, order % 2 == 0)], 0);
        }
        assert!(link.presses.waiting.len() <= MAX_WAITING);
    }

    #[test]
    fn a_report_older_than_the_presses_acted_on_says_nothing_of_what_is_held() {
        let (mut link, mut sender) = (pressed_link(), PeerLink::default());
        link.take_presses(2, &[tap(2, true)], 0);
        let old = sender.next_state(HeldInput::default(), false, 1, 0);
        assert!(!link.heard_state(&old, 0).iter().any(|e| matches!(e, Event::PeerHeld(_))));
        let current = sender.next_state(HeldInput::default(), false, 2, 0);
        assert!(link.heard_state(&current, 0).iter().any(|e| matches!(e, Event::PeerHeld(_))));
        // One that's ahead means presses went missing, which is what it's for.
        let ahead = sender.next_state(HeldInput::default(), false, 7, 0);
        assert!(link.heard_state(&ahead, 0).iter().any(|e| matches!(e, Event::PeerHeld(_))));
    }

    #[test]
    fn a_silence_does_not_turn_two_stragglers_into_a_restart() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&key(5000), 0));
        assert!(!link.arrived(&key(4990), 100));
        // Nothing at all for a while, and then one more from before.
        assert!(!link.arrived(&key(4991), 100 + RESTART_AFTER_MS + 500));
        assert!(!link.arrived(&key(4992), 100 + RESTART_AFTER_MS + 600));
    }

    /// `a` hears `b`, and `b` hasn't heard `a` for a long time, or ever.
    fn one_way_pair(echo: bool) -> (PeerLink, PeerLink) {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        a.set_online(true);
        a.arrived(&heartbeat(1), 0);
        if echo {
            answered(&mut a, &mut b, 0, 5, 5);
        }
        for now_ms in (400..=4000).step_by(400) {
            a.next_state(HeldInput::default(), false, 0, now_ms);
            let report = b.next_state(HeldInput::default(), false, 0, now_ms);
            a.heard_state(&report, now_ms);
        }
        (a, b)
    }

    #[test]
    fn answers_that_stop_take_the_figures_with_them() {
        let (a, _) = one_way_pair(true);
        let quality = a.quality(&router(), 4000);
        assert_eq!(quality.round_trip_ms, None);
        assert_eq!(quality.loss_out, None);
        assert!(quality.one_way);
    }

    #[test]
    fn a_peer_that_never_heard_this_device_is_said_to_be_one_way() {
        let (a, _) = one_way_pair(false);
        assert!(a.quality(&router(), 4000).one_way);
    }

    #[test]
    fn a_peer_is_given_time_to_answer_at_first() {
        let mut a = PeerLink::default();
        let mut b = PeerLink::default();
        a.set_online(true);
        a.arrived(&heartbeat(1), 0);
        a.heard_state(&b.next_state(HeldInput::default(), false, 0, 0), 0);
        assert!(!a.quality(&router(), ANSWER_FOR_MS - 1).one_way);
    }

    #[test]
    fn no_figure_is_shown_before_it_is_known() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        a.set_online(true);
        answered(&mut a, &mut b, 1000, 30, 10);
        let quality = a.quality(&router(), 1100);
        assert!(quality.measured);
        assert_eq!(quality.loss_in, None);
        assert_eq!(quality.round_trip_ms, Some(60));
    }

    #[test]
    fn the_loss_reported_always_fits() {
        let mut a = PeerLink::default();
        a.loss.percent = Some(1e9);
        assert_eq!(a.next_state(HeldInput::default(), false, 0, 0).loss_tenths, 1000);
    }

    #[test]
    fn late_and_repeated_input_is_dropped() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&key(100), 0));
        assert!(link.arrived(&key(102), 0));
        assert!(!link.arrived(&key(101), 0), "late");
        assert!(!link.arrived(&key(102), 0), "the same packet twice");
        assert!(link.arrived(&key(103), 0));
    }

    #[test]
    fn a_press_that_arrives_after_its_release_is_dropped() {
        let mut link = PeerLink::default();
        let release = Packet::new(11, NodeId::Debian, Payload::KeyEvent { scancode: 30, pressed: false });
        assert!(link.arrived(&release, 0));
        assert!(!link.arrived(&key(10), 0));
    }

    fn release(seq: u64, scancode: u16) -> Packet {
        Packet::new(seq, NodeId::Debian, Payload::KeyEvent { scancode, pressed: false })
    }

    #[test]
    fn a_release_overtaken_by_another_key_still_counts() {
        let mut link = PeerLink::default();
        let other = Packet::new(12, NodeId::Debian, Payload::KeyEvent { scancode: 31, pressed: true });
        assert!(link.arrived(&key(10), 0));
        assert!(link.arrived(&other, 0));
        assert!(link.arrived(&release(11, 30), 0));
    }

    #[test]
    fn a_release_from_before_the_same_key_went_down_again_is_dropped() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&key(12), 0));
        assert!(!link.arrived(&release(11, 30), 0));
        // Nor does one get past a report that's newer, which already says how things stand.
        let mut sender = PeerLink::default();
        assert!(link.arrived(&state(&mut sender, 20, HeldInput::default(), 0), 0));
        assert!(!link.arrived(&release(19, 31), 0));
    }

    #[test]
    fn a_press_overtaken_by_another_key_is_dropped() {
        // Letting it through would type the two the wrong way round.
        let mut link = PeerLink::default();
        let other = Packet::new(12, NodeId::Debian, Payload::KeyEvent { scancode: 31, pressed: true });
        assert!(link.arrived(&other, 0));
        assert!(!link.arrived(&key(11), 0));
    }

    #[test]
    fn a_few_packets_reordered_over_a_window_edge_are_not_loss() {
        let mut link = PeerLink::default();
        link.set_online(true);
        let mut seq = 1000u64;
        // A quiet link, where one late packet in ten would otherwise read as 10% in every other window.
        for window in 0..10u64 {
            for at in 0..10u64 {
                seq += 1;
                if at == 9 && window % 2 == 0 {
                    link.arrived(&heartbeat(seq + 1), window * 2000 + at * 200);
                    link.arrived(&heartbeat(seq), window * 2000 + at * 200 + 201);
                    seq += 1;
                } else {
                    link.arrived(&heartbeat(seq), window * 2000 + at * 200);
                }
            }
        }
        assert!(link.quality(&router(), 20_000).loss_in.unwrap() < 1.0);
    }

    #[test]
    fn a_key_overtaken_by_mouse_movement_still_counts() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&motion(11), 0));
        assert!(link.arrived(&key(10), 0));
        assert!(!link.arrived(&motion(9), 0), "movement keeps its own order");
    }

    #[test]
    fn a_held_report_racing_newer_input_loses_either_way() {
        let (mut link, mut sender) = (PeerLink::default(), PeerLink::default());
        // The report went out first and arrives second, so it's older than what it would undo.
        let report = state(&mut sender, 50, HeldInput::default(), 0);
        assert!(link.arrived(&key(51), 0));
        assert!(!link.arrived(&report, 0));
        // And a press from before a report is covered by that report.
        let report = state(&mut sender, 60, HeldInput::default(), 0);
        assert!(link.arrived(&report, 0));
        assert!(!link.arrived(&key(59), 0));
    }

    #[test]
    fn numbers_wrap_without_anything_being_dropped() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&key(u64::MAX - 1), 0));
        assert!(link.arrived(&key(u64::MAX), 0));
        assert!(link.arrived(&key(0), 0));
        assert!(link.arrived(&key(1), 0));
        assert!(!link.arrived(&key(u64::MAX), 0), "still late across the wrap");
    }

    #[test]
    fn a_peer_restarting_from_a_low_number_is_heard_straight_away() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&key(1_700_000_000_000_000), 0));
        assert!(link.arrived(&key(1), 10));
        assert!(link.arrived(&key(2), 20));
        assert!(!link.arrived(&key(1), 30), "and lateness is judged from the new numbers");
    }

    #[test]
    fn a_peer_restarting_from_a_number_just_below_is_heard_within_a_second() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&key(5000), 0));
        link.arrived(&heartbeat(10), 100);
        assert!(!link.arrived(&key(11), 500));
        assert!(!link.arrived(&key(12), 1099));
        assert!(link.arrived(&key(13), 1100));
        assert!(link.arrived(&key(14), 1110));
    }

    #[test]
    fn stragglers_from_a_running_peer_never_look_like_a_restart() {
        let mut link = PeerLink::default();
        let mut seq = 5000;
        for now_ms in (0..10_000).step_by(400) {
            assert!(link.arrived(&heartbeat(seq), now_ms));
            assert!(!link.arrived(&key(seq - 1), now_ms + 1) || now_ms == 0);
            seq += 5;
            assert!(link.arrived(&key(seq), now_ms + 2));
            seq += 1;
        }
    }

    #[test]
    fn focus_keeps_its_order_and_survives_a_restart() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&focus(10), 0));
        assert!(!link.arrived(&focus(9), 0));
        assert!(!link.arrived(&focus(10), 0));
        assert!(link.arrived(&focus(11), 0));
        assert!(link.arrived(&focus(1 << 40), 0));
        assert!(link.arrived(&focus(3), 0));
    }

    #[test]
    fn everything_else_always_gets_through() {
        let mut link = PeerLink::default();
        assert!(link.arrived(&heartbeat(10), 0));
        assert!(link.arrived(&heartbeat(10), 0));
        assert!(link.arrived(&heartbeat(9), 0));
        let panic = Packet::new(8, NodeId::Debian, Payload::PanicRelease);
        assert!(link.arrived(&panic, 0));
    }

    /// Feeds packets at 25 a second, leaving out those `lost` says.
    fn run(link: &mut PeerLink, seq: &mut u64, from_ms: u64, ms: u64, lost: impl Fn(u64) -> bool) {
        for now_ms in (from_ms..from_ms + ms).step_by(40) {
            *seq += 1;
            if !lost(*seq) {
                link.arrived(&heartbeat(*seq), now_ms);
            }
        }
    }

    #[test]
    fn loss_is_read_from_gaps_in_the_numbers() {
        let mut link = PeerLink::default();
        link.set_online(true);
        let mut seq = 1000;
        run(&mut link, &mut seq, 0, 12_000, |seq| seq % 5 == 0);
        let loss = link.quality(&router(), 12_000).loss_in.unwrap();
        assert!((18.0..=22.0).contains(&loss), "{loss}");
    }

    #[test]
    fn a_clean_link_loses_nothing() {
        let mut link = PeerLink::default();
        link.set_online(true);
        let mut seq = 1000;
        run(&mut link, &mut seq, 0, 6000, |_| false);
        assert_eq!(link.quality(&router(), 6000).loss_in, Some(0.0));
    }

    #[test]
    fn reordering_is_not_loss() {
        let mut link = PeerLink::default();
        link.set_online(true);
        for (at, pair) in (1000u64..1400).step_by(2).enumerate() {
            link.arrived(&heartbeat(pair + 1), at as u64 * 40);
            link.arrived(&heartbeat(pair), at as u64 * 40);
        }
        assert!(link.quality(&router(), 8000).loss_in.unwrap() < 1.0);
    }

    #[test]
    fn a_dropout_counts_as_loss_once_the_link_is_back() {
        let mut link = PeerLink::default();
        link.set_online(true);
        let mut seq = 1000;
        run(&mut link, &mut seq, 0, 4000, |_| false);
        run(&mut link, &mut seq, 4000, 1000, |_| true);
        run(&mut link, &mut seq, 5000, 2000, |_| false);
        assert!(link.quality(&router(), 7000).loss_in.unwrap() > 5.0);
    }

    #[test]
    fn a_restart_is_not_counted_as_loss() {
        let mut link = PeerLink::default();
        link.set_online(true);
        let mut seq = 1000;
        run(&mut link, &mut seq, 0, 4000, |_| false);
        let mut seq = 1u64 << 50;
        run(&mut link, &mut seq, 4000, 4000, |_| false);
        assert_eq!(link.quality(&router(), 8000).loss_in, Some(0.0));
    }

    /// Two ends, with `one_way_ms` each way, swapping heartbeats so that `a` gets an answer.
    fn answered(a: &mut PeerLink, b: &mut PeerLink, sent_ms: u64, one_way_ms: u64, held_ms: u64) -> Vec<Event> {
        let from_a = a.next_state(HeldInput::default(), false, 0, sent_ms);
        b.heard_state(&from_a, sent_ms + one_way_ms);
        let from_b = b.next_state(HeldInput::default(), false, 0, sent_ms + one_way_ms + held_ms);
        a.heard_state(&from_b, sent_ms + 2 * one_way_ms + held_ms)
    }

    #[test]
    fn the_round_trip_leaves_out_the_wait_for_the_next_heartbeat() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        a.set_online(true);
        answered(&mut a, &mut b, 1000, 30, 250);
        assert_eq!(a.quality(&router(), 2000).round_trip_ms, Some(60));
    }

    #[test]
    fn the_peers_own_reading_is_loss_on_the_way_there() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        a.set_online(true);
        let mut seq = 1000;
        run(&mut b, &mut seq, 0, 4000, |seq| seq % 10 == 0);
        answered(&mut a, &mut b, 5000, 1, 1);
        let loss = a.quality(&router(), 5100).loss_out.unwrap();
        assert!((9.0..=11.0).contains(&loss), "{loss}");
    }

    #[test]
    fn an_answer_tells_the_router_when_the_heartbeat_went_out() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        let events = answered(&mut a, &mut b, 1000, 10, 10);
        assert!(events.contains(&Event::PeerHeard { sent_ms: 1000, panicked: false }));
        assert!(events.contains(&Event::PeerHeld(HeldInput::default())));
    }

    #[test]
    fn a_report_with_nothing_to_answer_still_says_what_is_held() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        let mut held = HeldInput::default();
        held.set_key(30, true);
        let report = b.next_state(held, false, 0, 0);
        assert_eq!(a.heard_state(&report, 0), vec![Event::PeerHeld(held)]);
    }

    #[test]
    fn the_app_gets_no_figures_while_the_link_is_down() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        a.set_online(true);
        let mut seq = 1000;
        run(&mut a, &mut seq, 0, 4000, |seq| seq % 5 == 0);
        answered(&mut a, &mut b, 1000, 30, 10);
        assert!(a.quality(&router(), 4000).measured);
        a.set_online(false);
        assert_eq!(a.quality(&router(), 4000), LinkQuality::default());
        // Back up, it starts over rather than showing what it had.
        a.set_online(true);
        assert!(!a.quality(&router(), 4100).measured);
    }

    #[test]
    fn an_older_peer_is_told_from_its_silence() {
        let mut link = PeerLink::default();
        assert_eq!(link.peer_version(0), PeerVersion::Unknown);
        link.arrived(&heartbeat(1), 1000);
        assert_eq!(link.peer_version(1000 + VERSION_WAIT_MS - 1), PeerVersion::Unknown);
        assert_eq!(link.peer_version(1000 + VERSION_WAIT_MS), PeerVersion::Older);
    }

    #[test]
    fn a_peer_on_this_version_is_known_from_one_report() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        let report = b.next_state(HeldInput::default(), false, 0, 0);
        a.heard_state(&report, 0);
        assert_eq!(a.peer_version(60_000), PeerVersion::Same);
    }

    #[test]
    fn another_protocol_is_said_and_not_acted_on() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        let mut report = b.next_state(HeldInput::default(), false, 0, 0);
        report.protocol = LINK_PROTOCOL + 1;
        assert!(a.heard_state(&report, 0).is_empty());
        assert_eq!(a.peer_version(0), PeerVersion::Other);
    }

    #[test]
    fn packets_that_cannot_be_read_mean_another_version_for_a_while() {
        let mut link = PeerLink::default();
        link.unreadable(1000);
        assert_eq!(link.peer_version(1000), PeerVersion::Other);
        assert_eq!(link.peer_version(1000 + UNREADABLE_FOR_MS), PeerVersion::Unknown);
    }

    #[test]
    fn a_peer_that_restarts_older_is_judged_afresh() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        let report = b.next_state(HeldInput::default(), false, 0, 0);
        a.arrived(&state(&mut b, 1 << 50, HeldInput::default(), 0), 0);
        a.heard_state(&report, 0);
        assert_eq!(a.peer_version(0), PeerVersion::Same);
        a.arrived(&heartbeat(5), 100);
        assert_eq!(a.peer_version(100 + VERSION_WAIT_MS), PeerVersion::Older);
    }
}
