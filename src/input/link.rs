//! What each device works out from the packets it gets: which of them are too late to act on, how
//! the link is doing, and what to put in its own heartbeat. Pure, so that both backends share it.
//! Time comes in as `now_ms`, on the same clock the router gets.

use crate::protocol::{HeldInput, LinkState, Packet, Payload, LINK_PROTOCOL};
use crate::router::Event;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

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

/// True when `seq` came after `other`. Half the number space each way, so that a wrap counts as
/// going forward.
fn is_newer(seq: u64, other: u64) -> bool {
    (seq.wrapping_sub(other) as i64) > 0
}

/// Packets that must stay in order among themselves. Each kind has its own place in the line, so
/// that a key isn't dropped for being overtaken by a mouse movement, which it has nothing to do with.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stream {
    /// Key and button events, and the held state that has to agree with them.
    Presses,
    Motion,
    Focus,
}

fn stream_of(payload: &Payload) -> Option<Stream> {
    match payload {
        Payload::KeyEvent { .. } | Payload::MouseButton { .. } | Payload::LinkState(_) => Some(Stream::Presses),
        Payload::MouseMoveDelta { .. } | Payload::MouseWheel { .. } => Some(Stream::Motion),
        Payload::FocusSwitchAt { .. } | Payload::ActiveView { .. } => Some(Stream::Focus),
        _ => None,
    }
}

/// Keeps the newest sequence number seen on each stream, and notices the peer starting again.
#[derive(Default)]
struct PacketOrder {
    newest: Option<u64>,
    /// Since when everything has been older than `newest`.
    behind_since: Option<u64>,
    presses: Option<u64>,
    motion: Option<u64>,
    focus: Option<u64>,
}

impl PacketOrder {
    /// Takes note of any packet at all. True if it shows that the peer has restarted.
    fn note(&mut self, seq: u64, now_ms: u64) -> bool {
        let Some(newest) = self.newest else {
            self.newest = Some(seq);
            return false;
        };
        let ahead = seq.wrapping_sub(newest) as i64;
        let behind_too_long = ahead < 0 && now_ms.saturating_sub(*self.behind_since.get_or_insert(now_ms)) >= RESTART_AFTER_MS;
        let restarted = ahead.unsigned_abs() > MAX_LATE_PACKETS || behind_too_long;
        if restarted {
            *self = PacketOrder { newest: Some(seq), ..PacketOrder::default() };
        } else if ahead > 0 {
            self.newest = Some(seq);
            self.behind_since = None;
        }
        restarted
    }

    /// False for a packet that something newer on its stream has already overtaken, which is also
    /// what a duplicate looks like.
    fn fresh(&mut self, stream: Stream, seq: u64) -> bool {
        let newest = match stream {
            Stream::Presses => &mut self.presses,
            Stream::Motion => &mut self.motion,
            Stream::Focus => &mut self.focus,
        };
        let fresh = newest.is_none_or(|newest| is_newer(seq, newest));
        if fresh {
            *newest = Some(seq);
        }
        fresh
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
            self.got = 0;
        }
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
    /// Percent of the peer's packets not arriving here, and of ours not arriving there.
    pub loss_in: f32,
    pub loss_out: f32,
    pub round_trip_ms: Option<u32>,
    /// Control waits to go back to the other device.
    pub returning: bool,
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
    loss_out: Option<f32>,
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
        let mut events = vec![Event::PeerHeld(state.held)];
        if let Some((_, sent_ms)) = self.sent.iter().find(|(beat, _)| *beat == state.echo).copied() {
            let took = now_ms.saturating_sub(sent_ms).saturating_sub(u64::from(state.echo_age_ms)) as f32;
            self.round_trip_ms = Some(self.round_trip_ms.map_or(took, |was| was + (took - was) * SMOOTHING));
            events.push(Event::PeerHeard { sent_ms, panicked: state.panicked });
        }
        events
    }

    /// This device's next heartbeat.
    pub fn next_state(&mut self, held: HeldInput, panicked: bool, now_ms: u64) -> LinkState {
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
            loss_tenths: (self.loss.percent.unwrap_or(0.0) * 10.0).round() as u16,
            panicked,
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

    pub fn quality(&self, returning: bool, now_ms: u64) -> LinkQuality {
        let measured = self.online && (self.loss.percent.is_some() || self.round_trip_ms.is_some());
        LinkQuality {
            measured,
            loss_in: if measured { self.loss.percent.unwrap_or(0.0) } else { 0.0 },
            loss_out: if measured { self.loss_out.unwrap_or(0.0) } else { 0.0 },
            round_trip_ms: self.round_trip_ms.filter(|_| measured).map(|ms| ms.round() as u32),
            returning,
            peer_version: self.peer_version(now_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::NodeId;

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
        Packet::new(seq, NodeId::Debian, Payload::LinkState(link.next_state(held, false, now_ms)))
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
        let loss = link.quality(false, 12_000).loss_in;
        assert!((18.0..=22.0).contains(&loss), "{loss}");
    }

    #[test]
    fn a_clean_link_loses_nothing() {
        let mut link = PeerLink::default();
        link.set_online(true);
        let mut seq = 1000;
        run(&mut link, &mut seq, 0, 6000, |_| false);
        assert_eq!(link.quality(false, 6000).loss_in, 0.0);
    }

    #[test]
    fn reordering_is_not_loss() {
        let mut link = PeerLink::default();
        link.set_online(true);
        for (at, pair) in (1000u64..1400).step_by(2).enumerate() {
            link.arrived(&heartbeat(pair + 1), at as u64 * 40);
            link.arrived(&heartbeat(pair), at as u64 * 40);
        }
        assert!(link.quality(false, 8000).loss_in < 1.0);
    }

    #[test]
    fn a_dropout_counts_as_loss_once_the_link_is_back() {
        let mut link = PeerLink::default();
        link.set_online(true);
        let mut seq = 1000;
        run(&mut link, &mut seq, 0, 4000, |_| false);
        run(&mut link, &mut seq, 4000, 1000, |_| true);
        run(&mut link, &mut seq, 5000, 2000, |_| false);
        assert!(link.quality(false, 7000).loss_in > 5.0);
    }

    #[test]
    fn a_restart_is_not_counted_as_loss() {
        let mut link = PeerLink::default();
        link.set_online(true);
        let mut seq = 1000;
        run(&mut link, &mut seq, 0, 4000, |_| false);
        let mut seq = 1u64 << 50;
        run(&mut link, &mut seq, 4000, 4000, |_| false);
        assert_eq!(link.quality(false, 8000).loss_in, 0.0);
    }

    /// Two ends, with `one_way_ms` each way, swapping heartbeats so that `a` gets an answer.
    fn answered(a: &mut PeerLink, b: &mut PeerLink, sent_ms: u64, one_way_ms: u64, held_ms: u64) -> Vec<Event> {
        let from_a = a.next_state(HeldInput::default(), false, sent_ms);
        b.heard_state(&from_a, sent_ms + one_way_ms);
        let from_b = b.next_state(HeldInput::default(), false, sent_ms + one_way_ms + held_ms);
        a.heard_state(&from_b, sent_ms + 2 * one_way_ms + held_ms)
    }

    #[test]
    fn the_round_trip_leaves_out_the_wait_for_the_next_heartbeat() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        a.set_online(true);
        answered(&mut a, &mut b, 1000, 30, 250);
        assert_eq!(a.quality(false, 2000).round_trip_ms, Some(60));
    }

    #[test]
    fn the_peers_own_reading_is_loss_on_the_way_there() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        a.set_online(true);
        let mut seq = 1000;
        run(&mut b, &mut seq, 0, 4000, |seq| seq % 10 == 0);
        answered(&mut a, &mut b, 5000, 1, 1);
        let loss = a.quality(false, 5100).loss_out;
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
        let report = b.next_state(held, false, 0);
        assert_eq!(a.heard_state(&report, 0), vec![Event::PeerHeld(held)]);
    }

    #[test]
    fn the_app_gets_no_figures_while_the_link_is_down() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        a.set_online(true);
        let mut seq = 1000;
        run(&mut a, &mut seq, 0, 4000, |seq| seq % 5 == 0);
        answered(&mut a, &mut b, 1000, 30, 10);
        assert!(a.quality(false, 4000).measured);
        a.set_online(false);
        let quality = a.quality(true, 4000);
        assert_eq!(quality, LinkQuality { returning: true, ..LinkQuality::default() });
        // Back up, it starts over rather than showing what it had.
        a.set_online(true);
        assert!(!a.quality(false, 4100).measured);
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
        let report = b.next_state(HeldInput::default(), false, 0);
        a.heard_state(&report, 0);
        assert_eq!(a.peer_version(60_000), PeerVersion::Same);
    }

    #[test]
    fn another_protocol_is_said_and_not_acted_on() {
        let (mut a, mut b) = (PeerLink::default(), PeerLink::default());
        let mut report = b.next_state(HeldInput::default(), false, 0);
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
        let report = b.next_state(HeldInput::default(), false, 0);
        a.arrived(&state(&mut b, 1 << 50, HeldInput::default(), 0), 0);
        a.heard_state(&report, 0);
        assert_eq!(a.peer_version(0), PeerVersion::Same);
        a.arrived(&heartbeat(5), 100);
        assert_eq!(a.peer_version(100 + VERSION_WAIT_MS), PeerVersion::Older);
    }
}
