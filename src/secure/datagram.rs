//! One encrypted UDP channel: its sessions, the handshakes that make them, and sealing and opening
//! packets. It owns no socket. Time comes in as `now_ms`, and `seal` takes none, so the age of a
//! session is only looked at in `open` and `tick`.
//!
//! Numbers on the wire are big-endian. Locks are never held across Noise work, and never two at once.

use super::keys::{Pairing, NOISE_IK};
use super::window::ReplayWindow;
use snow::{Builder, HandshakeState, StatelessTransportState};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// What sealing adds to a packet: 13 bytes of header and a 16-byte tag.
pub const OVERHEAD: usize = 29;

const HANDSHAKE_1: u8 = 0xE1;
const HANDSHAKE_2: u8 = 0xE2;
const DATA: u8 = 0xE3;
const OLD_MAGICS: [[u8; 4]; 2] = [crate::protocol::PACKET_MAGIC, crate::audio::packet::MAGIC];

const INDEX_LEN: usize = 4;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const DATA_HEADER: usize = 1 + INDEX_LEN + 8;
/// The most Noise will take in one message.
const MAX_PLAIN: usize = 65535 - TAG_LEN;
/// `e`, `s` and the payload, the last two encrypted.
const NOISE_1_LEN: usize = KEY_LEN + KEY_LEN + TAG_LEN + INDEX_LEN + TAG_LEN;
/// `e` and the encrypted payload.
const NOISE_2_LEN: usize = KEY_LEN + INDEX_LEN + TAG_LEN;
const HANDSHAKE_1_LEN: usize = 1 + NOISE_1_LEN;
const HANDSHAKE_2_LEN: usize = 1 + INDEX_LEN + NOISE_2_LEN;

const QUICK_RETRY_MS: u64 = 250;
const QUICK_RETRIES_FOR_MS: u64 = 10_000;
const SLOW_RETRY_MS: u64 = 1000;
const RETRY_JITTER_MS: u64 = 50;
const REKEY_AFTER_MS: u64 = 10 * 60 * 1000;
const DROP_AFTER_MS: u64 = 60 * 60 * 1000;
const PREVIOUS_FOR_MS: u64 = 60_000;
const ANSWERS_PER_SECOND: usize = 10;
/// Enough tries in flight to cover a second of round trip at the quick pace, on either side.
const MAX_OUTSTANDING: usize = 4;
const MAX_PENDING: usize = 4;
/// A device that has data in and nothing going out says so after this long.
const PASSIVE_KEEPALIVE_MS: u64 = 1000;
/// Sending into this much silence means the peer may have lost the session.
const SILENCE_MS: u64 = 3000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    Input,
    Audio,
}

impl Label {
    fn prologue(self) -> &'static [u8] {
        match self {
            Label::Input => b"mesh-kvm 1 input",
            Label::Audio => b"mesh-kvm 1 audio",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Both,
    Initiator,
    Responder,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Opened<'a> {
    /// For the caller.
    Data(&'a [u8]),
    /// Send this straight back to where it came from.
    Reply(&'a [u8]),
    /// A keepalive, or a handshake needing no reply.
    Heard,
    /// Failed a check, and must not count as activity.
    Refused,
    /// Starts `MKVM` or `MKAU`.
    OldPeer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkSecurity {
    Unpaired,
    Connecting,
    Encrypted,
}

struct Session {
    transport: StatelessTransportState,
    ours: u32,
    theirs: u32,
    made_ms: u64,
    /// Taken with `fetch_add`, so that no two packets ever share a counter.
    next_counter: AtomicU64,
    seen: Mutex<ReplayWindow>,
}

impl Session {
    fn new(transport: StatelessTransportState, ours: u32, theirs: u32, now_ms: u64) -> Arc<Session> {
        Arc::new(Session {
            transport,
            ours,
            theirs,
            made_ms: now_ms,
            next_counter: AtomicU64::new(0),
            seen: Mutex::new(ReplayWindow::default()),
        })
    }

    fn age(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.made_ms)
    }

    fn seal(&self, plain: &[u8], out: &mut Vec<u8>) -> bool {
        out.clear();
        if plain.len() > MAX_PLAIN {
            return false;
        }
        let counter = self.next_counter.fetch_add(1, Ordering::Relaxed);
        out.resize(DATA_HEADER + plain.len() + TAG_LEN, 0);
        out[0] = DATA;
        out[1..1 + INDEX_LEN].copy_from_slice(&self.theirs.to_be_bytes());
        out[1 + INDEX_LEN..DATA_HEADER].copy_from_slice(&counter.to_be_bytes());
        if self.transport.write_message(counter, plain, &mut out[DATA_HEADER..]).is_err() {
            out.clear();
            return false;
        }
        true
    }

    /// Leaves the plaintext in `out`, or returns false with `out` empty.
    fn open(&self, counter: u64, sealed: &[u8], out: &mut Vec<u8>) -> bool {
        if !lock(&self.seen).may_be_new(counter) {
            return false;
        }
        out.resize(sealed.len().saturating_sub(TAG_LEN), 0);
        // The bit is only set once the tag has checked out.
        let good = self.transport.read_message(counter, sealed, out).is_ok()
            && lock(&self.seen).accept(counter);
        if !good {
            out.clear();
        }
        good
    }
}

struct Retired {
    session: Arc<Session>,
    since_ms: u64,
}

struct Pending {
    session: Arc<Session>,
    order: u64,
}

/// Dropped sessions are handed back, to be freed outside the lock.
#[derive(Default)]
struct Gone {
    lost_current: bool,
    sessions: Vec<Arc<Session>>,
}

#[derive(Default)]
struct Slots {
    /// Sends and receives.
    current: Option<Arc<Session>>,
    /// When current began, in the same count as `Pending::order`. Times can tie, and this can't.
    current_order: u64,
    /// Receives only, for 60 seconds after being replaced, for packets in flight.
    previous: Option<Retired>,
    /// Receive only: a responder's sessions with no data from the initiator yet, oldest first. That
    /// proves one is not a replayed handshake 1, which can get this far but no further.
    pending: VecDeque<Pending>,
    last_order: u64,
}

impl Slots {
    fn expire(&mut self, now_ms: u64) -> Gone {
        let old = |s: &Arc<Session>| s.age(now_ms) >= DROP_AFTER_MS;
        let mut gone = Gone::default();
        if let Some(session) = self.current.take_if(|s| old(s)) {
            gone.lost_current = true;
            gone.sessions.push(session);
        }
        let retired = self.previous.take_if(|r| {
            now_ms.saturating_sub(r.since_ms) >= PREVIOUS_FOR_MS || old(&r.session)
        });
        gone.sessions.extend(retired.map(|r| r.session));
        self.pending.retain(|p| {
            let keep = !old(&p.session);
            if !keep {
                gone.sessions.push(p.session.clone());
            }
            keep
        });
        gone
    }

    fn with_index(&self, index: u32) -> Option<Arc<Session>> {
        let previous = self.previous.as_ref().map(|r| &r.session);
        [self.current.as_ref(), previous]
            .into_iter()
            .flatten()
            .chain(self.pending.iter().map(|p| &p.session))
            .find(|s| s.ours == index)
            .cloned()
    }

    fn next_order(&mut self) -> u64 {
        self.last_order += 1;
        self.last_order
    }

    fn make_current(&mut self, session: Arc<Session>, now_ms: u64) -> Option<Retired> {
        self.current_order = self.next_order();
        let replaced = self.current.replace(session)?;
        self.previous.replace(Retired { session: replaced, since_ms: now_ms })
    }

    fn add_pending(&mut self, session: Arc<Session>) -> Option<Pending> {
        let order = self.next_order();
        self.pending.push_back(Pending { session, order });
        if self.pending.len() > MAX_PENDING {
            self.pending.pop_front()
        } else {
            None
        }
    }
}

struct Outstanding {
    index: u32,
    noise: HandshakeState,
}

#[derive(Clone, Copy)]
struct Retry {
    started_ms: u64,
    next_ms: u64,
}

#[derive(Default)]
struct Shake {
    /// The handshake 1s a handshake 2 can answer, oldest first. Each try adds one, so that an answer
    /// slower than the retries still finds its handshake.
    outstanding: VecDeque<Outstanding>,
    retry: Option<Retry>,
    /// Rule 2's second keepalive. The first goes back at once as an `Opened::Reply`, and this one
    /// on the next `tick`, so that one lost packet doesn't leave the responder pending.
    keepalive_owed: bool,
    /// When handshake 1s were taken up in the last second.
    answered_ms: VecDeque<u64>,
    handshake_heard_ms: Option<u64>,
    /// The last data or handshake 2 that checked out. A valid handshake 1 doesn't count, as a replay
    /// is one, and it must not hold off what silence heals.
    authentic_ms: Option<u64>,
    /// The first data in since anything went out.
    unanswered_since_ms: Option<u64>,
    /// The first `tick` that saw something sealed since `authentic_ms`.
    sealing_since_ms: Option<u64>,
}

pub struct Channel {
    private: [u8; KEY_LEN],
    peer: Option<[u8; KEY_LEN]>,
    /// Decides who gives way when both ends have a handshake going.
    lower_key: bool,
    label: Label,
    role: Role,
    slots: Mutex<Slots>,
    shake: Mutex<Shake>,
    // `seal` has no clock and takes no lock for these, so `tick` works out when they happened.
    // Keepalives don't set them: only what the caller sends should ever look for an answer.
    /// Something was sealed since anything authentic came in.
    sealed_unheard: AtomicBool,
    /// Something was sealed since `tick` last looked.
    sealed_lately: AtomicBool,
}

// A panic while a lock is held is an abort in release builds, so what's behind one is always whole.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn random_u32() -> Option<u32> {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).ok()?;
    Some(u32::from_be_bytes(bytes))
}

impl Channel {
    pub fn new(pairing: &Pairing, label: Label, role: Role) -> Channel {
        Channel {
            private: pairing.identity.private,
            peer: pairing.peer.as_ref().map(|p| p.public),
            lower_key: pairing.peer.as_ref().is_some_and(|p| pairing.identity.public < p.public),
            label,
            role,
            slots: Mutex::default(),
            shake: Mutex::default(),
            sealed_unheard: AtomicBool::new(false),
            sealed_lately: AtomicBool::new(false),
        }
    }

    /// Encrypts for the peer. False with no session yet: the packet is dropped, as a lost one would
    /// be. Safe from a Windows hook: its only lock is held to copy out an `Arc`.
    pub fn seal(&self, plain: &[u8], out: &mut Vec<u8>) -> bool {
        let current = lock(&self.slots).current.clone();
        match current {
            Some(session) => {
                self.sealed_unheard.store(true, Ordering::Relaxed);
                self.sealed_lately.store(true, Ordering::Relaxed);
                session.seal(plain, out)
            }
            None => {
                out.clear();
                false
            }
        }
    }

    /// Takes in a datagram. Only `Opened::Data` is for the caller.
    pub fn open<'a>(&self, datagram: &[u8], now_ms: u64, out: &'a mut Vec<u8>) -> Opened<'a> {
        out.clear();
        if OLD_MAGICS.iter().any(|magic| datagram.starts_with(magic)) {
            return Opened::OldPeer;
        }
        match datagram.first() {
            Some(&DATA) => self.open_data(datagram, now_ms, out),
            Some(&HANDSHAKE_1) => self.answer(datagram, now_ms, out),
            Some(&HANDSHAKE_2) => self.finish(datagram, now_ms, out),
            _ => Opened::Refused,
        }
    }

    /// Handshake packets due now, from the loop that owns the socket.
    pub fn tick(&self, now_ms: u64, send: &mut dyn FnMut(&[u8])) {
        if self.peer.is_none() {
            return;
        }
        let (gone, current) = {
            let mut slots = lock(&self.slots);
            (slots.expire(now_ms), slots.current.clone())
        };
        let can_start = self.role != Role::Responder;
        let (keepalive, due) = {
            let mut shake = lock(&self.shake);
            if gone.lost_current {
                shake.retry = None;
            }
            if self.sealed_lately.swap(false, Ordering::Relaxed) {
                shake.unanswered_since_ms = None;
            }
            let passive = shake
                .unanswered_since_ms
                .is_some_and(|since| now_ms.saturating_sub(since) >= PASSIVE_KEEPALIVE_MS);
            if passive {
                shake.unanswered_since_ms = None;
            }
            let keepalive = std::mem::take(&mut shake.keepalive_owed) || passive;

            let wanted = can_start
                && match &current {
                    Some(session) => {
                        session.age(now_ms) >= REKEY_AFTER_MS || self.silent(&mut shake, now_ms)
                    }
                    None => true,
                };
            if !wanted {
                // A handshake nobody is retrying must not keep beating the peer's in `answer`.
                shake.outstanding.clear();
                shake.retry = None;
            }
            (keepalive, wanted && shake.retry.is_none_or(|retry| now_ms >= retry.next_ms))
        };
        if let (true, Some(session)) = (keepalive, &current) {
            let mut packet = Vec::new();
            if session.seal(&[], &mut packet) {
                send(&packet);
            }
        }
        if due {
            self.start_handshake(now_ms, send);
        }
    }

    /// Rule 9. The wait runs from the first seal `tick` saw as well as from the last thing heard, or
    /// the first packet after a quiet spell would start a handshake on a link with nothing wrong.
    fn silent(&self, shake: &mut Shake, now_ms: u64) -> bool {
        if !self.sealed_unheard.load(Ordering::Relaxed) {
            shake.sealing_since_ms = None;
            return false;
        }
        let since = *shake.sealing_since_ms.get_or_insert(now_ms);
        let quiet_from = since.max(shake.authentic_ms.unwrap_or(0));
        now_ms.saturating_sub(quiet_from) >= SILENCE_MS
    }

    pub fn security(&self) -> LinkSecurity {
        if self.peer.is_none() {
            LinkSecurity::Unpaired
        } else if lock(&self.slots).current.is_some() {
            LinkSecurity::Encrypted
        } else {
            LinkSecurity::Connecting
        }
    }

    /// When the last valid handshake 1 came. A replay looks the same, so this is only good for a note.
    pub fn handshake_heard_at(&self) -> Option<u64> {
        lock(&self.shake).handshake_heard_ms
    }

    fn noise(&self) -> Option<Builder<'_>> {
        let params = NOISE_IK.parse().ok()?;
        Builder::new(params)
            .local_private_key(&self.private)
            .ok()?
            .prologue(self.label.prologue())
            .ok()
    }

    /// A random index no session here is using. Drawn outside the lock, as it is a system call.
    fn fresh_index(&self) -> Option<u32> {
        loop {
            let index = random_u32()?;
            if lock(&self.slots).with_index(index).is_none() {
                return Some(index);
            }
        }
    }

    fn start_handshake(&self, now_ms: u64, send: &mut dyn FnMut(&[u8])) {
        let made = self.write_handshake_1();
        let jitter = random_u32().map_or(0, |r| u64::from(r) % (RETRY_JITTER_MS + 1));
        let mut packet = None;
        {
            let mut shake = lock(&self.shake);
            let started_ms = shake.retry.map_or(now_ms, |retry| retry.started_ms);
            let quick = now_ms.saturating_sub(started_ms) < QUICK_RETRIES_FOR_MS;
            let wait = if quick { QUICK_RETRY_MS } else { SLOW_RETRY_MS };
            shake.retry = Some(Retry { started_ms, next_ms: now_ms + wait + jitter });
            if let Some((outstanding, bytes)) = made {
                shake.outstanding.push_back(outstanding);
                if shake.outstanding.len() > MAX_OUTSTANDING {
                    shake.outstanding.pop_front();
                }
                packet = Some(bytes);
            }
        }
        if let Some(bytes) = packet {
            send(&bytes);
        }
    }

    fn write_handshake_1(&self) -> Option<(Outstanding, [u8; HANDSHAKE_1_LEN])> {
        let peer = self.peer.as_ref()?;
        let index = self.fresh_index()?;
        let mut noise = self.noise()?.remote_public_key(peer).ok()?.build_initiator().ok()?;
        let mut packet = [0u8; HANDSHAKE_1_LEN];
        packet[0] = HANDSHAKE_1;
        let written = noise.write_message(&index.to_be_bytes(), &mut packet[1..]).ok()?;
        (written == NOISE_1_LEN).then_some((Outstanding { index, noise }, packet))
    }

    fn open_data<'a>(&self, datagram: &[u8], now_ms: u64, out: &'a mut Vec<u8>) -> Opened<'a> {
        let Some((index, counter, sealed)) = parse_data(datagram) else {
            return Opened::Refused;
        };
        let (found, gone) = {
            let mut slots = lock(&self.slots);
            let gone = slots.expire(now_ms);
            (slots.with_index(index), gone)
        };
        if gone.lost_current {
            lock(&self.shake).retry = None;
        }
        let Some(session) = found else {
            return Opened::Refused;
        };
        if !session.open(counter, sealed, out) {
            return Opened::Refused;
        }
        self.confirm(&session, now_ms);
        self.heard(now_ms, !out.is_empty());
        if out.is_empty() {
            Opened::Heard
        } else {
            Opened::Data(out)
        }
    }

    /// Something authentic came in. `data` is false for a keepalive, as answering those would have
    /// two devices doing it for ever.
    fn heard(&self, now_ms: u64, data: bool) {
        self.sealed_unheard.store(false, Ordering::Relaxed);
        let mut shake = lock(&self.shake);
        shake.authentic_ms = Some(now_ms);
        shake.sealing_since_ms = None;
        if data && shake.unanswered_since_ms.is_none() {
            shake.unanswered_since_ms = Some(now_ms);
            self.sealed_lately.store(false, Ordering::Relaxed);
        }
    }

    /// Rule 4: the first data on a pending session makes it current, if it is newer than current.
    /// An older one is what the peer has moved on from, or will: taking it is how two devices end
    /// up each sending on what the other is about to forget.
    fn confirm(&self, session: &Arc<Session>, now_ms: u64) {
        let (others, replaced) = {
            let mut slots = lock(&self.slots);
            let Some(pending) = slots.pending.iter().find(|p| Arc::ptr_eq(&p.session, session))
            else {
                return;
            };
            if slots.current.is_some() && pending.order < slots.current_order {
                return;
            }
            let others = std::mem::take(&mut slots.pending);
            (others, slots.make_current(session.clone(), now_ms))
        };
        drop((others, replaced));
        let mut shake = lock(&self.shake);
        shake.outstanding.clear();
        shake.retry = None;
    }

    /// Rule 3: a valid handshake 1 gets an answer and a new pending session, and leaves current
    /// alone, so that a replayed one can do nothing that lasts.
    fn answer<'a>(&self, datagram: &[u8], now_ms: u64, out: &'a mut Vec<u8>) -> Opened<'a> {
        if self.role == Role::Initiator || datagram.len() != HANDSHAKE_1_LEN {
            return Opened::Refused;
        }
        let Some(peer) = self.peer else {
            return Opened::Refused;
        };
        // Counted before the key operations, as those are what this limit is there to save.
        if !self.may_answer(now_ms) {
            return Opened::Refused;
        }
        let Some((noise, theirs)) = self.read_handshake_1(datagram, &peer) else {
            return Opened::Refused;
        };
        {
            let mut shake = lock(&self.shake);
            shake.handshake_heard_ms = Some(now_ms);
            // Rule 5: both ends have one going, and only one of them can win.
            if !shake.outstanding.is_empty() {
                if self.lower_key {
                    return Opened::Heard;
                }
                shake.outstanding.clear();
            }
        }
        let Some(session) = self.write_handshake_2(noise, theirs, now_ms, out) else {
            out.clear();
            return Opened::Refused;
        };
        let evicted = lock(&self.slots).add_pending(session);
        drop(evicted);
        Opened::Reply(out)
    }

    fn may_answer(&self, now_ms: u64) -> bool {
        let mut shake = lock(&self.shake);
        while shake.answered_ms.front().is_some_and(|&at| now_ms.saturating_sub(at) >= 1000) {
            shake.answered_ms.pop_front();
        }
        if shake.answered_ms.len() >= ANSWERS_PER_SECOND {
            return false;
        }
        shake.answered_ms.push_back(now_ms);
        true
    }

    fn read_handshake_1(
        &self,
        datagram: &[u8],
        peer: &[u8; KEY_LEN],
    ) -> Option<(HandshakeState, [u8; INDEX_LEN])> {
        let mut noise = self.noise()?.build_responder().ok()?;
        let mut payload = [0u8; INDEX_LEN];
        let read = noise.read_message(datagram.get(1..)?, &mut payload).ok()?;
        (read == INDEX_LEN && noise.get_remote_static() == Some(&peer[..]))
            .then_some((noise, payload))
    }

    fn write_handshake_2(
        &self,
        mut noise: HandshakeState,
        theirs: [u8; INDEX_LEN],
        now_ms: u64,
        out: &mut Vec<u8>,
    ) -> Option<Arc<Session>> {
        let ours = self.fresh_index()?;
        out.resize(HANDSHAKE_2_LEN, 0);
        out[0] = HANDSHAKE_2;
        out[1..1 + INDEX_LEN].copy_from_slice(&theirs);
        let written = noise.write_message(&ours.to_be_bytes(), &mut out[1 + INDEX_LEN..]).ok()?;
        if written != NOISE_2_LEN {
            return None;
        }
        let transport = noise.into_stateless_transport_mode().ok()?;
        Some(Session::new(transport, ours, u32::from_be_bytes(theirs), now_ms))
    }

    /// Rule 2: a valid handshake 2 makes the session current.
    fn finish<'a>(&self, datagram: &[u8], now_ms: u64, out: &'a mut Vec<u8>) -> Opened<'a> {
        if datagram.len() != HANDSHAKE_2_LEN {
            return Opened::Refused;
        }
        let Some((answers, noise_message)) = parse_handshake_2(datagram) else {
            return Opened::Refused;
        };
        let taken = {
            let mut shake = lock(&self.shake);
            let at = shake.outstanding.iter().position(|o| o.index == answers);
            at.and_then(|at| shake.outstanding.remove(at))
        };
        let Some(Outstanding { index, mut noise }) = taken else {
            return Opened::Refused;
        };
        let mut payload = [0u8; INDEX_LEN];
        match noise.read_message(noise_message, &mut payload) {
            Ok(INDEX_LEN) => {}
            _ => {
                // Anyone can see an index go by and forge this. The real answer may be on its way,
                // and a failed read leaves `noise` as it was.
                let mut shake = lock(&self.shake);
                if shake.outstanding.len() < MAX_OUTSTANDING {
                    shake.outstanding.push_front(Outstanding { index, noise });
                }
                return Opened::Refused;
            }
        }
        let Ok(transport) = noise.into_stateless_transport_mode() else {
            return Opened::Refused;
        };
        let session = Session::new(transport, index, u32::from_be_bytes(payload), now_ms);
        let replaced = lock(&self.slots).make_current(session.clone(), now_ms);
        drop(replaced);
        {
            let mut shake = lock(&self.shake);
            // The rest of this burst of tries. A late answer to one of them is refused.
            shake.outstanding.clear();
            shake.retry = None;
            shake.keepalive_owed = true;
        }
        self.heard(now_ms, false);
        if session.seal(&[], out) {
            Opened::Reply(out)
        } else {
            Opened::Heard
        }
    }
}

fn parse_data(datagram: &[u8]) -> Option<(u32, u64, &[u8])> {
    if datagram.len() < OVERHEAD {
        return None;
    }
    let index = datagram.get(1..1 + INDEX_LEN)?.try_into().ok()?;
    let counter = datagram.get(1 + INDEX_LEN..DATA_HEADER)?.try_into().ok()?;
    Some((u32::from_be_bytes(index), u64::from_be_bytes(counter), datagram.get(DATA_HEADER..)?))
}

fn parse_handshake_2(datagram: &[u8]) -> Option<(u32, &[u8])> {
    let index = datagram.get(1..1 + INDEX_LEN)?.try_into().ok()?;
    Some((u32::from_be_bytes(index), datagram.get(1 + INDEX_LEN..)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::keys::{Identity, PeerKey};
    use std::collections::HashSet;

    const STEP_MS: u64 = 5;

    fn peer_of(public: [u8; 32]) -> Option<PeerKey> {
        Some(PeerKey { public, name: "Peer".to_string(), paired_at: 1 })
    }

    fn paired_pair() -> (Pairing, Pairing) {
        let mut a = Pairing::fresh();
        let mut b = Pairing::fresh();
        a.peer = peer_of(b.identity.public);
        b.peer = peer_of(a.identity.public);
        (a, b)
    }

    fn with_identity(identity: &Identity, peer: [u8; 32]) -> Pairing {
        Pairing { identity: identity.clone(), peer: peer_of(peer) }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0.max(1);
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            (self.next() >> 33) % n
        }
    }

    struct Flight {
        at: u64,
        to: usize,
        bytes: Vec<u8>,
    }

    /// Two channels and the air between them. `tick` output and `Reply`s go through `put`, which
    /// applies the loss and the delay. Nothing here sleeps: `run` moves the clock.
    struct Wire {
        ends: [Arc<Channel>; 2],
        now: u64,
        loss_percent: u64,
        delay_ms: u64,
        jitter_ms: u64,
        rng: Rng,
        flights: Vec<Flight>,
        got: [Vec<Vec<u8>>; 2],
        refused: [usize; 2],
        /// Everything that was offered to the air, lost or not, as an attacker on the Wi-Fi sees it.
        heard: Vec<(usize, Vec<u8>)>,
        /// Each end sends a numbered packet this often, as the heartbeats do.
        chatter_ms: Option<u64>,
        chatter_sealed: [u64; 2],
        /// Ends that send no chatter, as a receiver with reports off.
        quiet: [bool; 2],
    }

    impl Wire {
        fn new(a: Channel, b: Channel) -> Wire {
            Wire {
                ends: [Arc::new(a), Arc::new(b)],
                now: 1000,
                loss_percent: 0,
                delay_ms: 5,
                jitter_ms: 0,
                rng: Rng(0x9E37_79B9_7F4A_7C15),
                flights: Vec::new(),
                got: [Vec::new(), Vec::new()],
                refused: [0, 0],
                heard: Vec::new(),
                chatter_ms: None,
                chatter_sealed: [0, 0],
                quiet: [false, false],
            }
        }

        fn paired(a: Role, b: Role) -> Wire {
            let (pa, pb) = paired_pair();
            Wire::new(Channel::new(&pa, Label::Input, a), Channel::new(&pb, Label::Input, b))
        }

        /// A handshake from end 0 alone, walked through by hand so that it can't cross with one
        /// from end 1.
        fn connected() -> Wire {
            let (pa, pb) = paired_pair();
            Wire::connected_from(&pa, &pb)
        }

        fn connected_from(pa: &Pairing, pb: &Pairing) -> Wire {
            let mut wire = Wire::new(
                Channel::new(pa, Label::Input, Role::Both),
                Channel::new(pb, Label::Input, Role::Both),
            );
            let (a, b) = (wire.ends[0].clone(), wire.ends[1].clone());
            let now = wire.now;
            let h1 = first_handshake(&a, now);
            let mut h2 = Vec::new();
            assert!(matches!(b.open(&h1, now, &mut h2), Opened::Reply(_)));
            // A slow answer: their ages differ, so that they don't reach 10 minutes in one tick.
            wire.now += 1000;
            let now = wire.now;
            let mut keepalive = Vec::new();
            assert!(matches!(a.open(&h2, now, &mut keepalive), Opened::Reply(_)));
            assert_eq!(opened_kind(&b, &keepalive, now), "heard");
            assert_eq!(a.security(), LinkSecurity::Encrypted);
            assert_eq!(b.security(), LinkSecurity::Encrypted);
            wire.heard.push((0, h1));
            wire.heard.push((1, h2));
            wire
        }

        fn put(&mut self, from: usize, bytes: Vec<u8>) {
            self.heard.push((from, bytes.clone()));
            if self.rng.below(100) < self.loss_percent {
                return;
            }
            let jitter = if self.jitter_ms > 0 { self.rng.below(self.jitter_ms + 1) } else { 0 };
            self.flights.push(Flight { at: self.now + self.delay_ms + jitter, to: 1 - from, bytes });
        }

        fn tick_one(&mut self, end: usize) {
            let mut sent = Vec::new();
            self.ends[end].tick(self.now, &mut |p| sent.push(p.to_vec()));
            for p in sent {
                self.put(end, p);
            }
        }

        fn deliver(&mut self, to: usize, bytes: &[u8]) {
            let mut out = Vec::new();
            let channel = self.ends[to].clone();
            match channel.open(bytes, self.now, &mut out) {
                Opened::Data(d) => self.got[to].push(d.to_vec()),
                Opened::Reply(r) => {
                    let r = r.to_vec();
                    self.put(to, r);
                }
                Opened::Heard | Opened::OldPeer => {}
                Opened::Refused => self.refused[to] += 1,
            }
        }

        fn step(&mut self) {
            self.now += STEP_MS;
            self.tick_one(0);
            self.tick_one(1);
            if let Some(every) = self.chatter_ms {
                if self.now % every == 0 {
                    let quiet = self.quiet;
                    for end in (0..2).filter(|&end| !quiet[end]) {
                        let plain = self.chatter_sealed[end].to_be_bytes();
                        if self.send(end, &plain) {
                            self.chatter_sealed[end] += 1;
                        }
                    }
                }
            }
            let now = self.now;
            let mut due = Vec::new();
            let mut i = 0;
            while i < self.flights.len() {
                if self.flights[i].at <= now {
                    due.push(self.flights.remove(i));
                } else {
                    i += 1;
                }
            }
            for f in due {
                self.deliver(f.to, &f.bytes);
            }
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms / STEP_MS {
                self.step();
            }
        }

        fn send(&mut self, from: usize, plain: &[u8]) -> bool {
            let mut out = Vec::new();
            let sealed = self.ends[from].seal(plain, &mut out);
            if sealed {
                self.put(from, out);
            }
            sealed
        }

        fn carries(&mut self, from: usize, plain: &[u8]) -> bool {
            let to = 1 - from;
            let before = self.got[to].len();
            if !self.send(from, plain) {
                return false;
            }
            self.run(self.delay_ms + self.jitter_ms + 2 * STEP_MS);
            self.got[to][before..].iter().any(|p| p == plain)
        }

        fn seal_now(&self, from: usize, plain: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            assert!(self.ends[from].seal(plain, &mut out));
            out
        }

        fn count_of_kind(&self, kind: u8) -> usize {
            self.heard.iter().filter(|(_, p)| p.first() == Some(&kind)).count()
        }

        fn first_of_kind(&self, from: usize, kind: u8) -> Vec<u8> {
            self.heard
                .iter()
                .find(|(end, p)| *end == from && p.first() == Some(&kind))
                .map(|(_, p)| p.clone())
                .expect("a packet of that kind went out")
        }
    }

    fn first_handshake(channel: &Channel, now: u64) -> Vec<u8> {
        let mut sent = Vec::new();
        channel.tick(now, &mut |p| sent.push(p.to_vec()));
        assert_eq!(sent.len(), 1);
        sent.remove(0)
    }

    fn opened_kind(channel: &Channel, bytes: &[u8], now: u64) -> &'static str {
        let mut out = Vec::new();
        match channel.open(bytes, now, &mut out) {
            Opened::Data(_) => "data",
            Opened::Reply(_) => "reply",
            Opened::Heard => "heard",
            Opened::Refused => "refused",
            Opened::OldPeer => "old peer",
        }
    }

    #[test]
    fn a_channel_can_be_shared_between_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<Channel>();
    }

    #[test]
    fn two_paired_channels_connect_and_carry_data() {
        let mut wire = Wire::paired(Role::Both, Role::Both);
        assert_eq!(wire.ends[0].security(), LinkSecurity::Connecting);
        wire.tick_one(0);
        wire.run(100);
        assert!(wire.carries(0, b"from a"));
        assert!(wire.carries(1, b"from b"));
        assert_eq!(wire.ends[0].security(), LinkSecurity::Encrypted);
        assert_eq!(wire.ends[1].security(), LinkSecurity::Encrypted);
        assert_eq!(wire.seal_now(0, &[7; 100]).len(), 100 + OVERHEAD);
    }

    #[test]
    fn audio_is_ready_before_the_first_sound() {
        let mut wire = Wire::paired(Role::Initiator, Role::Responder);
        wire.run(100);
        // The keepalives alone confirm it.
        assert_eq!(wire.ends[0].security(), LinkSecurity::Encrypted);
        assert_eq!(wire.ends[1].security(), LinkSecurity::Encrypted);
        assert!(wire.carries(0, b"sound"));
        assert!(wire.carries(1, b"report"));
    }

    #[test]
    fn nothing_is_sealed_before_a_session() {
        let (pa, pb) = paired_pair();
        let a = Channel::new(&pa, Label::Input, Role::Both);
        let b = Channel::new(&pb, Label::Input, Role::Responder);
        let mut out = vec![1, 2, 3];
        assert!(!a.seal(b"too soon", &mut out));
        assert!(out.is_empty());
        // Pending only receives, so a responder still has nothing to seal with.
        let h1 = first_handshake(&a, 0);
        assert_eq!(opened_kind(&b, &h1, 0), "reply");
        assert!(!b.seal(b"too soon", &mut out));
        assert_eq!(b.security(), LinkSecurity::Connecting);
    }

    #[test]
    fn an_unpaired_channel_never_sends_or_answers() {
        let (pa, pb) = paired_pair();
        let a = Channel::new(&pa, Label::Input, Role::Both);
        let lone = Pairing { peer: None, ..pb };
        let b = Channel::new(&lone, Label::Input, Role::Both);
        assert_eq!(b.security(), LinkSecurity::Unpaired);
        let mut sent = 0;
        for now in (0..30_000).step_by(10) {
            b.tick(now, &mut |_| sent += 1);
        }
        assert_eq!(sent, 0);
        let h1 = first_handshake(&a, 0);
        assert_eq!(opened_kind(&b, &h1, 0), "refused");
        assert_eq!(b.handshake_heard_at(), None);
        let mut out = Vec::new();
        assert!(!b.seal(b"x", &mut out));
        assert_eq!(b.security(), LinkSecurity::Unpaired);
    }

    #[test]
    fn a_stranger_key_gets_no_reply() {
        let (pa, _) = paired_pair();
        let ours = Channel::new(&pa, Label::Input, Role::Both);
        // The stranger knows our public key, which is no secret, and uses its own key.
        let stranger_keys = Pairing::fresh();
        let stranger_pairing = with_identity(&stranger_keys.identity, pa.identity.public);
        let stranger = Channel::new(&stranger_pairing, Label::Input, Role::Both);
        let h1 = first_handshake(&stranger, 0);
        assert_eq!(opened_kind(&ours, &h1, 0), "refused");
        assert_eq!(ours.handshake_heard_at(), None);
        assert_eq!(ours.security(), LinkSecurity::Connecting);
    }

    #[test]
    fn handshakes_retry_every_250ms_then_every_second() {
        let (pa, _) = paired_pair();
        let a = Channel::new(&pa, Label::Input, Role::Both);
        let mut times = Vec::new();
        let mut packets = HashSet::new();
        let start = 5000;
        for now in start..start + 20_000 {
            a.tick(now, &mut |p| {
                times.push(now);
                packets.insert(p.to_vec());
            });
        }
        assert_eq!(times[0], start);
        // Every try is a new handshake, with a new ephemeral key.
        assert_eq!(packets.len(), times.len());
        let mut gaps = HashSet::new();
        for pair in times.windows(2) {
            let gap = pair[1] - pair[0];
            gaps.insert(gap);
            if pair[0] - start < 10_000 {
                assert!((250..=300).contains(&gap), "{gap} ms at {}", pair[0] - start);
            } else {
                assert!((1000..=1050).contains(&gap), "{gap} ms at {}", pair[0] - start);
            }
        }
        assert!(gaps.len() > 5, "the gaps should vary: {gaps:?}");
    }

    #[test]
    fn a_responder_never_starts_and_an_initiator_never_answers() {
        let (pa, pb) = paired_pair();
        let a = Channel::new(&pa, Label::Input, Role::Initiator);
        let b = Channel::new(&pb, Label::Input, Role::Responder);
        let mut sent = 0;
        for now in (0..20_000).step_by(10) {
            b.tick(now, &mut |_| sent += 1);
        }
        assert_eq!(sent, 0);
        let other = Channel::new(&pb, Label::Input, Role::Initiator);
        let h1 = first_handshake(&other, 0);
        assert_eq!(opened_kind(&a, &h1, 0), "refused");
    }

    fn connect_time_at_30_percent(seed: u64, keys: &(Pairing, Pairing), delay_ms: u64) -> Option<u64> {
        let a = Channel::new(&keys.0, Label::Input, Role::Both);
        let b = Channel::new(&keys.1, Label::Input, Role::Both);
        let mut wire = Wire::new(a, b);
        wire.rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        wire.loss_percent = 30;
        wire.delay_ms = delay_ms;
        wire.chatter_ms = Some(20);
        let start = wire.now;
        // The two ends start a random time apart, as two devices do.
        let head_start = wire.rng.below(250) / STEP_MS;
        for _ in 0..head_start {
            wire.now += STEP_MS;
            wire.tick_one(0);
        }
        while wire.now - start < 3000 {
            wire.step();
            if wire.ends.iter().all(|e| e.security() == LinkSecurity::Encrypted) {
                return Some(wire.now - start);
            }
        }
        None
    }

    /// Runs `check` for each seed below `seeds` across threads, as debug builds are slow at key
    /// operations, and counts the ones that pass.
    fn count_seeds(seeds: u64, check: impl Fn(u64) -> bool + Sync) -> usize {
        const THREADS: u64 = 8;
        std::thread::scope(|scope| {
            let check = &check;
            let workers: Vec<_> = (0..THREADS)
                .map(|t| {
                    scope.spawn(move || {
                        (0..seeds).filter(|s| s % THREADS == t).filter(|&s| check(s)).count()
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).sum()
        })
    }

    #[test]
    fn lost_handshakes_at_30_percent_still_connect() {
        let keys = paired_pair();
        for delay_ms in [5, 80] {
            let in_time = count_seeds(1000, |seed| {
                connect_time_at_30_percent(seed, &keys, delay_ms).is_some_and(|t| t <= 2000)
            });
            println!("{delay_ms} ms each way: {in_time} of 1000 within 2 seconds");
            assert!(in_time >= 950, "{in_time} of 1000 at {delay_ms} ms each way");
        }
    }

    #[test]
    fn lossy_starts_all_survive() {
        let keys = paired_pair();
        let working = count_seeds(200, |seed| {
            let mut wire = Wire::new(
                Channel::new(&keys.0, Label::Input, Role::Both),
                Channel::new(&keys.1, Label::Input, Role::Both),
            );
            wire.rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            wire.loss_percent = 30;
            wire.delay_ms = 5;
            wire.jitter_ms = 60;
            wire.chatter_ms = Some(100);
            // The second device starts anywhere up to a second after the first.
            for _ in 0..wire.rng.below(200) {
                wire.now += STEP_MS;
                wire.tick_one(0);
            }
            wire.run(5000);
            wire.loss_percent = 0;
            // Past the end of any `previous` left from the start.
            wire.run(PREVIOUS_FOR_MS + 5000);
            wire.carries(0, b"from a") && wire.carries(1, b"from b")
        });
        println!("{working} of 200 work both ways after a minute");
        assert_eq!(working, 200);
    }

    #[test]
    fn both_starting_at_once_works_both_ways() {
        let mut wire = Wire::paired(Role::Both, Role::Both);
        wire.delay_ms = 10;
        wire.run(200);
        assert!(wire.carries(0, b"from a"));
        assert!(wire.carries(1, b"from b"));
        assert_eq!(wire.ends[0].security(), LinkSecurity::Encrypted);
        assert_eq!(wire.ends[1].security(), LinkSecurity::Encrypted);
    }

    #[test]
    fn crossed_handshakes_still_work_after_two_minutes() {
        let mut wire = Wire::paired(Role::Both, Role::Both);
        wire.delay_ms = 10;
        wire.chatter_ms = Some(100);
        wire.run(200);
        wire.run(120_000);
        assert!(wire.carries(0, b"from a"), "a to b is dead two minutes after both started at once");
        assert!(wire.carries(1, b"from b"), "b to a is dead two minutes after both started at once");
    }

    #[test]
    fn a_restarted_peer_is_taken_without_losing_the_old_session_first() {
        let (pa, pb) = paired_pair();
        let wire = Wire::connected_from(&pa, &pb);
        let old_b = wire.ends[1].clone();
        let new_b = Channel::new(&pb, Label::Input, Role::Both);
        let a = wire.ends[0].clone();
        let now = wire.now;

        let h1 = first_handshake(&new_b, now);
        let mut answer = Vec::new();
        match a.open(&h1, now, &mut answer) {
            Opened::Reply(r) => assert_eq!(r.first(), Some(&0xE2)),
            other => panic!("expected an answer, got {other:?}"),
        }
        // Answered, but that could be a replay, so the old session still sends and receives.
        assert_eq!(a.security(), LinkSecurity::Encrypted);
        let still_old = wire.seal_now(0, b"to the old one");
        let mut out = Vec::new();
        assert_eq!(old_b.open(&still_old, now, &mut out), Opened::Data(b"to the old one"));
        let mut from_old = Vec::new();
        assert!(old_b.seal(b"from the old one", &mut from_old));
        assert_eq!(a.open(&from_old, now, &mut out), Opened::Data(b"from the old one"));

        // The restarted device answers with a keepalive, which is what moves the other over.
        let mut keepalive = Vec::new();
        match new_b.open(&answer, now, &mut keepalive) {
            Opened::Reply(r) => assert_eq!(r.len(), OVERHEAD),
            other => panic!("expected a keepalive, got {other:?}"),
        }
        assert_eq!(new_b.security(), LinkSecurity::Encrypted);
        assert_eq!(a.open(&keepalive, now, &mut out), Opened::Heard);
        let new_session = wire.seal_now(0, b"to the new one");
        assert_eq!(new_b.open(&new_session, now, &mut out), Opened::Data(b"to the new one"));
        // Packets still in flight from before the restart open on previous.
        let mut late = Vec::new();
        assert!(old_b.seal(b"late", &mut late));
        assert_eq!(a.open(&late, now, &mut out), Opened::Data(b"late"));
    }

    #[test]
    fn a_replayed_handshake_leaves_current_alone() {
        let mut wire = Wire::connected();
        let h1 = wire.first_of_kind(0, 0xE1);
        let before = wire.seal_now(1, b"x")[1..5].to_vec();
        for _ in 0..5 {
            wire.run(1000);
            let mut answer = Vec::new();
            let now = wire.now;
            let b = wire.ends[1].clone();
            let Opened::Reply(reply) = b.open(&h1, now, &mut answer) else {
                panic!("a replay still looks valid");
            };
            assert_eq!(b.handshake_heard_at(), Some(now));
            // The real device has long forgotten that handshake.
            assert_eq!(opened_kind(&wire.ends[0], reply, now), "refused");
            assert!(wire.carries(0, b"from a"));
            assert!(wire.carries(1, b"from b"));
            assert_eq!(wire.seal_now(1, b"x")[1..5], before[..]);
        }
    }

    #[test]
    fn a_handshake_for_another_channel_is_refused() {
        let (pa, pb) = paired_pair();
        let input = Channel::new(&pa, Label::Input, Role::Both);
        let audio = Channel::new(&pb, Label::Audio, Role::Both);
        let h1 = first_handshake(&audio, 0);
        assert_eq!(opened_kind(&input, &h1, 0), "refused");
        assert_eq!(input.handshake_heard_at(), None);
        let h1 = first_handshake(&input, 0);
        assert_eq!(opened_kind(&audio, &h1, 0), "refused");
    }

    #[test]
    fn packets_reflected_to_their_sender_are_refused() {
        let mut wire = Wire::connected();
        let now = wire.now;
        let (a, b) = (wire.ends[0].clone(), wire.ends[1].clone());
        assert_eq!(opened_kind(&a, &wire.first_of_kind(0, 0xE1), now), "refused");
        assert_eq!(opened_kind(&b, &wire.first_of_kind(1, 0xE2), now), "refused");
        let from_a = wire.seal_now(0, b"to b");
        let from_b = wire.seal_now(1, b"to a");
        assert_eq!(opened_kind(&a, &from_a, now), "refused");
        // An attacker can write the sender's own index in, as it travels in the clear.
        let mut aimed = from_a.clone();
        aimed[1..5].copy_from_slice(&from_b[1..5]);
        assert_eq!(opened_kind(&a, &aimed, now), "refused");
        assert_eq!(opened_kind(&b, &from_a, now), "data");
        wire.run(10);
        assert!(wire.carries(0, b"still fine"));
    }

    #[test]
    fn changing_any_byte_of_a_data_packet_refuses_it() {
        let wire = Wire::connected();
        let now = wire.now;
        let b = wire.ends[1].clone();
        let packet = wire.seal_now(0, b"a key press, about this long....");
        for at in 0..packet.len() {
            for bit in 0..8 {
                let mut changed = packet.clone();
                changed[at] ^= 1 << bit;
                assert_eq!(opened_kind(&b, &changed, now), "refused", "byte {at} bit {bit}");
            }
        }
        // None of that moved the window.
        assert_eq!(opened_kind(&b, &packet, now), "data");
        assert_eq!(opened_kind(&b, &packet, now), "refused");
        // Nor did a forged counter far ahead.
        let next = wire.seal_now(0, b"next");
        let mut far = next.clone();
        far[5] = 0x7F;
        assert_eq!(opened_kind(&b, &far, now), "refused");
        assert_eq!(opened_kind(&b, &next, now), "data");
    }

    #[test]
    fn data_across_a_key_rotation_is_not_lost() {
        let mut wire = Wire::connected();
        wire.delay_ms = 20;
        wire.chatter_ms = Some(10);
        let old_index = wire.seal_now(0, b"x")[1..5].to_vec();
        // The old session stays in use right up to the change.
        wire.run(REKEY_AFTER_MS - 2000);
        let late_a = wire.seal_now(0, b"late from a");
        let late_b = wire.seal_now(1, b"late from b");
        assert_eq!(late_a[1..5], old_index[..]);
        wire.run(2000);
        assert_ne!(wire.seal_now(0, b"x")[1..5], old_index[..], "the keys should have changed");
        // Held back across the change, and they still open, on previous.
        let (a, b) = (wire.ends[0].clone(), wire.ends[1].clone());
        assert_eq!(opened_kind(&b, &late_a, wire.now), "data");
        assert_eq!(opened_kind(&a, &late_b, wire.now), "data");
        wire.run(1000);
        for end in 0..2 {
            let delivered: HashSet<&Vec<u8>> = wire.got[1 - end].iter().collect();
            let sealed = wire.chatter_sealed[end];
            let missing = (0..sealed - 10)
                .filter(|n| !delivered.contains(&n.to_be_bytes().to_vec()))
                .count();
            assert_eq!(missing, 0, "from end {end}, of {sealed}");
        }
        assert_eq!(wire.refused, [0, 0]);
    }

    #[test]
    fn previous_is_refused_after_60_seconds() {
        let mut wire = Wire::connected();
        wire.chatter_ms = Some(1000);
        let late = wire.seal_now(0, b"late");
        wire.run(REKEY_AFTER_MS + 1000);
        let b = wire.ends[1].clone();
        wire.run(PREVIOUS_FOR_MS);
        assert_eq!(opened_kind(&b, &late, wire.now), "refused");
    }

    #[test]
    fn links_survive_rotation_at_every_delay() {
        // A responder's session is older by the flight of a handshake 1, so its handshake for the
        // change reaches the initiator just as the initiator starts its own. They nearly always cross.
        let keys = paired_pair();
        for delay_ms in [1, 5, 10, 40] {
            let alive = count_seeds(16, |seed| {
                let mut wire = Wire::new(
                    Channel::new(&keys.0, Label::Input, Role::Both),
                    Channel::new(&keys.1, Label::Input, Role::Both),
                );
                wire.delay_ms = delay_ms;
                wire.chatter_ms = Some(100);
                let (a, b) = (wire.ends[0].clone(), wire.ends[1].clone());
                let h1 = first_handshake(&a, wire.now);
                let mut h2 = Vec::new();
                wire.now += delay_ms;
                assert!(matches!(b.open(&h1, wire.now, &mut h2), Opened::Reply(_)));
                wire.now += delay_ms;
                let mut keepalive = Vec::new();
                assert!(matches!(a.open(&h2, wire.now, &mut keepalive), Opened::Reply(_)));
                wire.now += delay_ms;
                assert_eq!(opened_kind(&b, &keepalive, wire.now), "heard");
                wire.now += seed % 5;
                wire.run(REKEY_AFTER_MS + PREVIOUS_FOR_MS + 5000);
                wire.carries(0, b"from a") && wire.carries(1, b"from b")
            });
            assert_eq!(alive, 16, "at {delay_ms} ms each way");
        }
    }

    #[test]
    fn a_rotation_started_by_both_at_once_keeps_working() {
        let mut wire = Wire::paired(Role::Both, Role::Both);
        wire.delay_ms = 10;
        wire.chatter_ms = Some(100);
        wire.tick_one(0);
        wire.run(100);
        // Long enough for the 10-minute change, however it fell, and for previous to run out.
        for _ in 0..3 {
            wire.run(REKEY_AFTER_MS / 2);
            wire.run(5000);
            assert!(wire.carries(0, b"from a"), "a to b is dead at {} ms", wire.now);
            assert!(wire.carries(1, b"from b"), "b to a is dead at {} ms", wire.now);
        }
    }

    #[test]
    fn a_session_is_dropped_at_one_hour() {
        let mut wire = Wire::connected();
        let a = wire.ends[0].clone();
        // The link has gone, so rotation can never finish. Ticks are sparse, as each try is slow
        // in a debug build, and the hour is all this looks at.
        wire.loss_percent = 100;
        wire.now += DROP_AFTER_MS - 5000;
        wire.step();
        assert_eq!(a.security(), LinkSecurity::Encrypted);
        wire.seal_now(0, b"just in time");
        wire.now += 5000;
        wire.step();
        assert_eq!(a.security(), LinkSecurity::Connecting);
        let mut out = Vec::new();
        assert!(!a.seal(b"too late", &mut out));
        // `open` decides this too, and doesn't wait for a tick.
        let (pa, pb) = paired_pair();
        let c = Channel::new(&pa, Label::Input, Role::Both);
        let d = Channel::new(&pb, Label::Input, Role::Both);
        let h1 = first_handshake(&c, 0);
        let mut answer = Vec::new();
        assert!(matches!(d.open(&h1, 0, &mut answer), Opened::Reply(_)));
        let mut keepalive = Vec::new();
        assert!(matches!(c.open(&answer, 0, &mut keepalive), Opened::Reply(_)));
        assert_eq!(opened_kind(&d, &keepalive, 0), "heard");
        let mut late = Vec::new();
        assert!(c.seal(b"late", &mut late));
        assert_eq!(opened_kind(&d, &late, DROP_AFTER_MS), "refused");
        assert!(!d.seal(b"late", &mut out));
    }

    #[test]
    fn handshake_answers_are_limited_to_10_a_second() {
        let (pa, pb) = paired_pair();
        let a = Channel::new(&pa, Label::Input, Role::Both);
        let b = Channel::new(&pb, Label::Input, Role::Both);
        let h1 = first_handshake(&a, 0);
        let replies = |from: u64, to: u64| {
            (from..to).filter(|&now| opened_kind(&b, &h1, now) == "reply").count()
        };
        assert_eq!(replies(1000, 1100), 10);
        // Still inside the second those ten began.
        assert_eq!(replies(1100, 2000), 0);
        assert_eq!(replies(2000, 2100), 10);
        // Across any second, not only whole ones: 20 at the join of two would be 20 in a second.
        assert_eq!(replies(3995, 4005), 10);
        assert_eq!(replies(4005, 4995), 0);
    }

    #[test]
    fn reordered_and_repeated_packets_open_once_each() {
        let wire = Wire::connected();
        let mut rng = Rng(42);
        let mut packets: Vec<(u64, Vec<u8>)> = (0..500u64)
            .map(|n| (n, wire.seal_now(0, &n.to_be_bytes())))
            .collect();
        packets.extend(packets.clone());
        packets.extend(packets[..300].to_vec());
        for i in (1..packets.len()).rev() {
            packets.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let b = wire.ends[1].clone();
        let mut opened = Vec::new();
        for (_, p) in &packets {
            let mut out = Vec::new();
            if let Opened::Data(d) = b.open(p, wire.now, &mut out) {
                opened.push(d.to_vec());
            }
        }
        assert_eq!(opened.len(), 500);
        let unique: HashSet<&Vec<u8>> = opened.iter().collect();
        assert_eq!(unique.len(), 500);
    }

    #[test]
    fn an_old_packet_is_reported_as_old_peer() {
        let wire = Wire::connected();
        let (pa, _) = paired_pair();
        let lone = Channel::new(&Pairing { peer: None, ..pa }, Label::Input, Role::Both);
        for channel in [&*wire.ends[0], &lone] {
            assert_eq!(opened_kind(channel, b"MKVM\x02rest of it", 0), "old peer");
            assert_eq!(opened_kind(channel, b"MKAU\x01\x02", 0), "old peer");
            assert_eq!(opened_kind(channel, b"MKAU", 0), "old peer");
            assert_eq!(opened_kind(channel, b"MKV", 0), "refused");
            assert_eq!(opened_kind(channel, b"MKVA\x01", 0), "refused");
        }
    }

    #[test]
    fn any_bytes_are_dropped_without_panic() {
        let mut wire = Wire::connected();
        let (a, b) = (wire.ends[0].clone(), wire.ends[1].clone());
        let index = wire.seal_now(0, b"x")[1..5].to_vec();
        let mut rng = Rng(7);
        let mut now = wire.now;
        for len in 0..=1500usize {
            for first in [None, Some(0xE1), Some(0xE2), Some(0xE3)] {
                let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                if let (Some(first), Some(at)) = (first, bytes.first_mut()) {
                    *at = first;
                }
                if first == Some(0xE3) && len >= 5 {
                    // Aimed at a session that exists, to get as far as the cipher.
                    bytes[1..5].copy_from_slice(&index);
                }
                now += 200;
                let kind = opened_kind(&b, &bytes, now);
                assert!(kind == "refused", "{kind} for {len} bytes");
            }
        }
        wire.now = now;
        // Every valid packet, cut at every length.
        let h1 = wire.first_of_kind(0, 0xE1);
        let h2 = wire.first_of_kind(1, 0xE2);
        let empty = wire.seal_now(0, b"");
        let data = wire.seal_now(0, &[9; 1400]);
        for packet in [&h1, &h2, &empty, &data] {
            for len in 0..packet.len() {
                now += 200;
                assert_eq!(opened_kind(&b, &packet[..len], now), "refused", "{len}");
                assert_eq!(opened_kind(&a, &packet[..len], now), "refused", "{len}");
            }
        }
    }

    #[test]
    fn waking_after_hours_starts_a_new_session() {
        let mut wire = Wire::connected();
        let before = wire.seal_now(0, b"before the sleep");
        wire.now += 6 * 3600 * 1000;
        let (a, b) = (wire.ends[0].clone(), wire.ends[1].clone());
        // Nothing from before the sleep is good any more, even before a tick.
        assert_eq!(opened_kind(&b, &before, wire.now), "refused");
        wire.tick_one(0);
        let mut out = Vec::new();
        assert!(!a.seal(b"stale keys", &mut out));
        wire.run(200);
        assert_eq!(a.security(), LinkSecurity::Encrypted);
        assert_eq!(b.security(), LinkSecurity::Encrypted);
        assert!(wire.carries(0, b"morning"));
        assert!(wire.carries(1, b"morning"));
    }

    #[test]
    fn retries_are_quick_again_after_a_session_is_lost() {
        let mut wire = Wire::connected();
        wire.loss_percent = 100;
        // Rotation starts, and has long been down to one try a second when the hour ends.
        wire.now += REKEY_AFTER_MS;
        wire.step();
        wire.now += QUICK_RETRIES_FOR_MS + 5000;
        wire.step();
        wire.now = 1000 + DROP_AFTER_MS - 1000;
        wire.step();
        wire.heard.clear();
        wire.run(6000);
        let ones = wire.heard.iter().filter(|(end, p)| *end == 0 && p[0] == 0xE1).count();
        assert!(ones >= 15, "only {ones} tries in the 5 seconds after losing the session");
    }

    #[test]
    fn a_peer_with_a_new_key_never_shows_encrypted() {
        let (pa, _) = paired_pair();
        let reinstalled = Pairing::fresh();
        let b_pairing = with_identity(&reinstalled.identity, pa.identity.public);
        let mut wire = Wire::new(
            Channel::new(&pa, Label::Input, Role::Both),
            Channel::new(&b_pairing, Label::Input, Role::Both),
        );
        for _ in 0..3000 {
            wire.step();
            assert_eq!(wire.ends[0].security(), LinkSecurity::Connecting);
            assert_eq!(wire.ends[1].security(), LinkSecurity::Connecting);
        }
        assert!(wire.refused[0] > 0 && wire.refused[1] > 0);
        assert_eq!(wire.ends[0].handshake_heard_at(), None);
    }

    #[test]
    fn a_forged_handshake_2_does_not_cost_the_real_one() {
        let (pa, pb) = paired_pair();
        let a = Channel::new(&pa, Label::Input, Role::Both);
        let b = Channel::new(&pb, Label::Input, Role::Both);
        let h1 = first_handshake(&a, 0);
        let mut answer = Vec::new();
        assert!(matches!(b.open(&h1, 0, &mut answer), Opened::Reply(_)));
        let mut forged = answer.clone();
        for at in 5..forged.len() {
            forged[at] ^= 0x55;
        }
        assert_eq!(opened_kind(&a, &forged, 0), "refused");
        assert_eq!(a.security(), LinkSecurity::Connecting);
        assert_eq!(opened_kind(&a, &answer, 0), "reply");
        assert_eq!(a.security(), LinkSecurity::Encrypted);
        // A real one replayed afterwards finds nothing waiting for it.
        assert_eq!(opened_kind(&a, &answer, 0), "refused");
    }

    #[test]
    fn answers_slower_than_the_retries_still_count() {
        let (pa, pb) = paired_pair();
        let a = Channel::new(&pa, Label::Input, Role::Both);
        let b = Channel::new(&pb, Label::Input, Role::Both);
        let tries: Vec<Vec<u8>> = (0..5).map(|n| first_handshake(&a, n * 300)).collect();
        let answers: Vec<Vec<u8>> = tries
            .iter()
            .enumerate()
            .map(|(n, h1)| {
                let mut h2 = Vec::new();
                let now = 2000 + n as u64 * 200;
                assert!(matches!(b.open(h1, now, &mut h2), Opened::Reply(_)));
                h2
            })
            .collect();
        // Only the last four are kept.
        assert_eq!(opened_kind(&a, &answers[0], 3000), "refused");
        assert_eq!(opened_kind(&a, &answers[1], 3000), "reply");
        // One finished, and a late answer from the same burst finds nothing.
        assert_eq!(opened_kind(&a, &answers[2], 3000), "refused");
        assert_eq!(opened_kind(&a, &answers[4], 3000), "refused");
    }

    #[test]
    fn a_400ms_round_trip_connects() {
        for (first, second) in [(Role::Both, Role::Both), (Role::Initiator, Role::Responder)] {
            let mut wire = Wire::paired(first, second);
            wire.delay_ms = 200;
            wire.chatter_ms = Some(100);
            wire.run(2000);
            assert_eq!(wire.ends[0].security(), LinkSecurity::Encrypted, "{first:?}");
            assert_eq!(wire.ends[1].security(), LinkSecurity::Encrypted, "{second:?}");
            wire.run(PREVIOUS_FOR_MS + 5000);
            assert!(wire.carries(0, b"from a"));
            assert!(wire.carries(1, b"from b"));
        }
    }

    /// The first has the lower key.
    fn by_key() -> (Pairing, Pairing) {
        let (a, b) = paired_pair();
        if a.identity.public < b.identity.public {
            (a, b)
        } else {
            (b, a)
        }
    }

    #[test]
    fn when_handshakes_meet_the_lower_key_wins() {
        let (low_keys, high_keys) = by_key();
        let low = Channel::new(&low_keys, Label::Input, Role::Both);
        let high = Channel::new(&high_keys, Label::Input, Role::Both);
        let from_low = first_handshake(&low, 0);
        let from_high = first_handshake(&high, 0);
        // Ignored, but still heard, for the one-way note.
        assert_eq!(opened_kind(&low, &from_high, 10), "heard");
        assert_eq!(low.handshake_heard_at(), Some(10));
        let mut answer = Vec::new();
        assert!(matches!(high.open(&from_low, 10, &mut answer), Opened::Reply(_)));
        let mut keepalive = Vec::new();
        assert!(matches!(low.open(&answer, 20, &mut keepalive), Opened::Reply(_)));
        assert_eq!(opened_kind(&high, &keepalive, 30), "heard");
        assert_eq!(low.security(), LinkSecurity::Encrypted);
        assert_eq!(high.security(), LinkSecurity::Encrypted);
    }

    #[test]
    fn a_handshake_given_up_on_does_not_beat_the_peer() {
        let (low_keys, high_keys) = by_key();
        let mut wire = Wire::connected_from(&low_keys, &high_keys);
        wire.quiet = [false, true];
        wire.loss_percent = 100;
        wire.chatter_ms = Some(20);
        // The lower key starts a handshake into the silence, and then the link comes back.
        wire.run(SILENCE_MS + 500);
        wire.loss_percent = 0;
        assert!(wire.carries(1, b"back again"));
        wire.chatter_ms = None;
        wire.run(50);
        let high = Channel::new(&high_keys, Label::Input, Role::Both);
        let h1 = first_handshake(&high, wire.now);
        assert_eq!(opened_kind(&wire.ends[0], &h1, wire.now), "reply");
    }

    #[test]
    fn data_on_a_pending_older_than_current_opens_but_stays_pending() {
        let (pa, pb) = paired_pair();
        let a = Channel::new(&pa, Label::Input, Role::Initiator);
        let b = Channel::new(&pb, Label::Input, Role::Both);
        let c = Channel::new(&pa, Label::Input, Role::Both);
        // b answers a, and pending comes first. Then b finishes a handshake of its own.
        let to_a = first_handshake(&a, 0);
        let mut for_a = Vec::new();
        assert!(matches!(b.open(&to_a, 0, &mut for_a), Opened::Reply(_)));
        let to_c = first_handshake(&b, 0);
        let mut for_b = Vec::new();
        assert!(matches!(c.open(&to_c, 0, &mut for_b), Opened::Reply(_)));
        let mut keepalive = Vec::new();
        assert!(matches!(b.open(&for_b, 0, &mut keepalive), Opened::Reply(_)));
        let index = wire_index(&b);
        let mut out = Vec::new();
        assert!(matches!(a.open(&for_a, 0, &mut out), Opened::Reply(_)));
        assert_eq!(opened_kind(&b, &out, 0), "heard");
        let mut data = Vec::new();
        assert!(a.seal(b"on the older one", &mut data));
        assert_eq!(b.open(&data, 0, &mut out), Opened::Data(b"on the older one"));
        assert_eq!(wire_index(&b), index, "current should not have moved");
    }

    fn wire_index(channel: &Channel) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(channel.seal(b"x", &mut out));
        out[1..5].to_vec()
    }

    #[test]
    fn a_device_that_only_receives_sends_a_keepalive_each_second() {
        let mut wire = Wire::paired(Role::Initiator, Role::Responder);
        wire.quiet = [false, true];
        wire.chatter_ms = Some(20);
        wire.run(500);
        let sent_by_b = |wire: &Wire| {
            wire.heard.iter().filter(|(end, p)| *end == 1 && p[0] == 0xE3).count()
        };
        let before = sent_by_b(&wire);
        wire.run(10_000);
        let keepalives = sent_by_b(&wire) - before;
        assert!((9..=11).contains(&keepalives), "{keepalives}");
    }

    #[test]
    fn keepalives_are_not_answered_with_keepalives() {
        let mut wire = Wire::connected();
        wire.run(1000);
        wire.heard.clear();
        wire.send(0, b"one packet");
        wire.run(30_000);
        let keepalives = wire.heard.iter().filter(|(_, p)| p[0] == 0xE3).count() - 1;
        assert_eq!(keepalives, 1, "one, from the end that got the packet");
    }

    #[test]
    fn a_device_that_sends_nothing_never_looks_for_an_answer() {
        let mut wire = Wire::paired(Role::Initiator, Role::Responder);
        wire.run(500);
        let before = wire.count_of_kind(0xE1);
        wire.run(60_000);
        assert_eq!(wire.count_of_kind(0xE1), before);
    }

    #[test]
    fn an_audio_receiver_restart_recovers_within_5_seconds() {
        let (pa, pb) = paired_pair();
        let mut wire = Wire::new(
            Channel::new(&pa, Label::Audio, Role::Initiator),
            Channel::new(&pb, Label::Audio, Role::Responder),
        );
        wire.delay_ms = 20;
        wire.quiet = [false, true];
        wire.chatter_ms = Some(20);
        wire.run(2000);
        wire.ends[1] = Arc::new(Channel::new(&pb, Label::Audio, Role::Responder));
        wire.got[1].clear();
        let restarted = wire.now;
        while wire.got[1].is_empty() {
            wire.step();
            assert!(wire.now - restarted <= 5000, "no sound for 5 seconds");
        }
        println!("sound back after {} ms", wire.now - restarted);
    }

    #[test]
    fn silence_keeps_current_until_the_new_session_is_confirmed() {
        let mut wire = Wire::paired(Role::Initiator, Role::Responder);
        wire.quiet = [false, true];
        wire.chatter_ms = Some(20);
        wire.run(500);
        let index = wire_index(&wire.ends[0]);
        wire.loss_percent = 100;
        wire.run(SILENCE_MS + 1000);
        assert!(wire.heard.iter().rev().take(200).any(|(_, p)| p[0] == 0xE1));
        assert_eq!(wire_index(&wire.ends[0]), index);
        wire.loss_percent = 0;
        wire.run(600);
        assert_ne!(wire_index(&wire.ends[0]), index);
        assert!(wire.carries(0, b"sound"));
    }

    #[test]
    fn a_quiet_responder_does_not_cause_handshake_churn() {
        let mut wire = Wire::paired(Role::Initiator, Role::Responder);
        wire.delay_ms = 20;
        wire.quiet = [false, true];
        wire.chatter_ms = Some(20);
        wire.run(500);
        let at_start = wire.count_of_kind(0xE1);
        // Steady sound, up to the first rotation.
        wire.run(REKEY_AFTER_MS - 60_000);
        assert_eq!(wire.count_of_kind(0xE1), at_start);
        // Sound with long gaps, as speech has, past it and a second one.
        for _ in 0..240 {
            wire.chatter_ms = Some(20);
            wire.run(1000);
            wire.chatter_ms = None;
            wire.run(4000);
        }
        assert_eq!(wire.count_of_kind(0xE1), at_start + 2, "only the two rotations");
    }

    #[test]
    fn a_replay_that_evicts_pending_heals_by_silence() {
        let (pa, pb) = paired_pair();
        let mut wire = Wire::new(
            Channel::new(&pa, Label::Audio, Role::Initiator),
            Channel::new(&pb, Label::Audio, Role::Responder),
        );
        let (a, b) = (wire.ends[0].clone(), wire.ends[1].clone());
        // Tries an attacker recorded, and then the one that gets through.
        let recorded: Vec<Vec<u8>> = (0..4).map(|n| first_handshake(&a, n * 300)).collect();
        wire.now = 1200;
        let real = first_handshake(&a, wire.now);
        let mut answer = Vec::new();
        assert!(matches!(b.open(&real, wire.now, &mut answer), Opened::Reply(_)));
        for replay in &recorded {
            assert_eq!(opened_kind(&b, replay, wire.now), "reply");
        }
        let mut keepalive = Vec::new();
        assert!(matches!(a.open(&answer, wire.now, &mut keepalive), Opened::Reply(_)));
        // The session the initiator now has is gone from the responder.
        assert_eq!(opened_kind(&b, &keepalive, wire.now), "refused");
        assert_eq!(a.security(), LinkSecurity::Encrypted);
        assert_eq!(b.security(), LinkSecurity::Connecting);

        wire.quiet = [false, true];
        wire.chatter_ms = Some(20);
        let start = wire.now;
        while b.security() != LinkSecurity::Encrypted {
            wire.step();
            assert!(wire.now - start <= 5000, "still not healed");
        }
        assert!(wire.carries(0, b"sound"));
    }

    #[test]
    fn sealing_during_a_session_change_never_repeats_a_nonce() {
        let mut wire = Wire::connected();
        wire.chatter_ms = Some(1000);
        let stop = Arc::new(AtomicBool::new(false));
        let mut sealers = Vec::new();
        for end in 0..2 {
            for _ in 0..2 {
                let channel = wire.ends[end].clone();
                let stop = stop.clone();
                sealers.push(std::thread::spawn(move || {
                    let mut seen = Vec::new();
                    let mut out = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        if channel.seal(b"x", &mut out) {
                            seen.push(out[1..13].to_vec());
                        }
                    }
                    seen
                }));
            }
        }
        let mut changes = HashSet::new();
        for _ in 0..30 {
            changes.insert(wire.seal_now(0, b"x")[1..5].to_vec());
            wire.now += REKEY_AFTER_MS;
            wire.run(100);
        }
        stop.store(true, Ordering::Relaxed);
        let mut all = HashSet::new();
        let mut total = 0;
        for sealer in sealers {
            for pair in sealer.join().unwrap() {
                total += 1;
                assert!(all.insert(pair), "an index and counter went out twice");
            }
        }
        assert!(total > 1000, "only {total} packets");
        assert!(changes.len() >= 25, "only {} sessions", changes.len());
    }

    #[test]
    #[ignore]
    fn seal_and_open_timing() {
        let wire = Wire::connected();
        let (a, b) = (wire.ends[0].clone(), wire.ends[1].clone());
        let rounds = 100_000u32;
        for size in [40usize, 160, 1400] {
            let plain = vec![0x5A; size];
            let mut sealed = Vec::new();
            let mut opened = Vec::new();
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                assert!(a.seal(&plain, &mut sealed));
                assert!(matches!(b.open(&sealed, wire.now, &mut opened), Opened::Data(_)));
            }
            let each = started.elapsed().as_secs_f64() * 1e6 / f64::from(rounds);
            println!("{size} bytes: {each:.2} us per packet, seal and open together");
        }
    }
}
