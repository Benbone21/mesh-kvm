//! Evens out packets that arrive late, early or not at all, and turns them back into a steady
//! stream of audio with as little delay as the network allows.
//!
//! The buffer holds encoded packets and hands them over only when the player asks, so that the
//! player's clock sets the pace. How much it holds is measured from how packets have been arriving.

use super::codec::{Decoder, Format};
use super::packet::{Body, Packet, Report};
use super::CHANNELS;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// How far back arrival times count towards the buffer's size.
const WINDOW: Duration = Duration::from_secs(4);
/// The share of packets the buffer aims to hold for in time. The worst few are left to concealment:
/// waiting for them would delay everything else.
const COVERED: f64 = 0.98;
/// A spread this small is timing noise, not a reason to hold another frame.
const SMALLEST_SPREAD_MS: f64 = 1.0;
/// Arrivals needed before the measurement means anything.
const FEWEST_ARRIVALS: usize = 20;
/// How often the size is worked out.
const RESIZE_EVERY: Duration = Duration::from_millis(250);
/// How long the buffer must run fuller than it needs to before one frame is dropped.
const TRIM_AFTER: Duration = Duration::from_millis(250);
/// Above the target by this much, the buffer sheds frames faster.
const FAR_OVER_FRAMES: usize = 3;
const TRIM_SPEEDUP: u32 = 5;
/// The longest delay the buffer will build up to.
const MOST_DELAY_MS: f64 = 200.0;
/// Repairing a lost packet takes the next one, so for this long after a loss one is kept in hand.
const REPAIR_MEMORY: Duration = Duration::from_secs(10);
/// How long concealment carries on with nothing arriving before the buffer starts again.
const LONGEST_STALL_MS: f64 = 100.0;
/// A jump bigger than this means the stream started again.
const FURTHEST_JUMP: u32 = 500;
/// The most packets held, however they arrive.
const MOST_HELD: usize = 400;
/// How long the crossfade over a dropped frame takes.
const TRIM_FADE: usize = 120;
/// How long a stream with nothing arriving still counts as alive.
const ALIVE_FOR: Duration = Duration::from_secs(2);
/// How quickly the reported loss follows the latest reading, in each `report`.
const LOSS_SMOOTHING: f64 = 0.3;

/// What to play next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Not playing: nothing is due, so nothing has been lost.
    Idle,
    Packet(Vec<u8>),
    /// A packet is lost, and this one after it carries repair data for it. It will also come up
    /// again in its own turn.
    Repair(Vec<u8>),
    /// A packet is lost or late, with nothing to rebuild it from.
    Conceal,
    /// The buffer has more than it needs: the first packet is to be dropped in favour of the second.
    Trim(Vec<u8>, Vec<u8>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub received: u64,
    /// Frames rebuilt from the next packet's repair data.
    pub repaired: u64,
    /// Frames made up because their packet never came in time.
    pub concealed: u64,
    /// Packets that came after their turn had gone.
    pub late: u64,
    /// Frames dropped to bring the delay down.
    pub trimmed: u64,
}

pub struct JitterBuffer {
    frame_ms: f64,
    floor: usize,
    /// Packets carry repair data for the one before.
    repairable: bool,
    packets: BTreeMap<u32, Vec<u8>>,
    cursor: Option<u32>,
    newest: Option<u32>,
    playing: bool,
    /// The sender has said it has nothing to send for now.
    quiet: bool,
    target: usize,
    /// When a packet first arrived, and its number, for working out how late the others are.
    anchor: Option<(Instant, u32)>,
    arrivals: VecDeque<(Instant, f64)>,
    next_resize: Option<Instant>,
    last_loss: Option<Instant>,
    /// The lowest the level has been since `low_since`.
    low: usize,
    low_since: Option<Instant>,
    stalled_frames: u32,
    counts: Counts,
}

impl JitterBuffer {
    pub fn new(format: Format) -> Self {
        let frame_ms = format.frame() as f64 * 1000.0 / f64::from(super::RATE);
        let floor = match format {
            Format::Opus => 1,
            Format::Pcm => 2,
        };
        Self {
            frame_ms,
            floor,
            repairable: format == Format::Opus,
            packets: BTreeMap::new(),
            cursor: None,
            newest: None,
            playing: false,
            quiet: false,
            target: floor + 1,
            anchor: None,
            arrivals: VecDeque::new(),
            next_resize: None,
            last_loss: None,
            low: usize::MAX,
            low_since: None,
            stalled_frames: 0,
            counts: Counts::default(),
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    pub fn newest(&self) -> Option<u32> {
        self.newest
    }

    fn most_frames(&self) -> usize {
        (MOST_DELAY_MS / self.frame_ms) as usize
    }

    /// Frames from the next one due up to the newest.
    fn level(&self) -> usize {
        match (self.cursor, self.newest) {
            (Some(cursor), Some(newest)) if newest >= cursor => (newest - cursor) as usize + 1,
            (None, _) => self.packets.len(),
            _ => 0,
        }
    }

    /// The delay the buffer is holding now, in ms.
    pub fn buffered_ms(&self) -> f64 {
        self.level() as f64 * self.frame_ms
    }

    fn start_again(&mut self) {
        self.packets.clear();
        self.cursor = None;
        self.newest = None;
        self.playing = false;
        self.anchor = None;
        self.arrivals.clear();
        self.stalled_frames = 0;
        self.low_since = None;
    }

    pub fn push(&mut self, seq: u32, payload: Vec<u8>, now: Instant) {
        let far_ahead = self.newest.is_some_and(|newest| seq.abs_diff(newest) > FURTHEST_JUMP);
        if far_ahead {
            self.start_again();
        }
        if self.cursor.is_some_and(|cursor| seq < cursor) {
            self.counts.late += 1;
            return;
        }
        self.counts.received += 1;
        self.quiet = false;
        self.note_arrival(seq, now);
        self.packets.insert(seq, payload);
        self.newest = Some(self.newest.map_or(seq, |newest| newest.max(seq)));
        while self.packets.len() > MOST_HELD {
            self.packets.pop_first();
        }
    }

    /// The sender has nothing to send. `seq` is the number its next packet will have, so a marker
    /// that was overtaken by the audio after it says nothing.
    pub fn note_quiet(&mut self, seq: u32) {
        if self.newest.is_none_or(|newest| seq > newest) {
            self.quiet = true;
        }
    }

    fn note_arrival(&mut self, seq: u32, now: Instant) {
        let (first_at, first_seq) = *self.anchor.get_or_insert((now, seq));
        let elapsed = now.duration_since(first_at).as_secs_f64() * 1000.0;
        let transit = elapsed - (i64::from(seq) - i64::from(first_seq)) as f64 * self.frame_ms;
        self.arrivals.push_back((now, transit));
        while self.arrivals.front().is_some_and(|(at, _)| now.duration_since(*at) > WINDOW) {
            self.arrivals.pop_front();
        }
    }

    fn resize(&mut self, now: Instant) {
        if self.next_resize.is_some_and(|at| now < at) || self.arrivals.len() < FEWEST_ARRIVALS {
            return;
        }
        self.next_resize = Some(now + RESIZE_EVERY);
        let mut transits: Vec<f64> = self.arrivals.iter().map(|(_, transit)| *transit).collect();
        transits.sort_by(f64::total_cmp);
        let quickest = transits[0];
        let spread = transits[((transits.len() - 1) as f64 * COVERED) as usize] - quickest;
        let needed = ((spread - SMALLEST_SPREAD_MS).max(0.0) / self.frame_ms).ceil() as usize + self.floor;
        let lost_lately = self.last_loss.is_some_and(|at| now.duration_since(at) < REPAIR_MEMORY);
        let least = self.floor + usize::from(lost_lately);
        self.target = needed.max(least).min(self.most_frames());
    }

    /// True once the buffer has run fuller than it needs for a while, which is a shorter while
    /// the more there is to lose.
    fn over_full(&mut self, now: Instant) -> bool {
        let level = self.level();
        self.low = self.low.min(level);
        let since = *self.low_since.get_or_insert(now);
        let far_over = self.low > self.target + FAR_OVER_FRAMES;
        if now.duration_since(since) < if far_over { TRIM_AFTER / TRIM_SPEEDUP } else { TRIM_AFTER } {
            return false;
        }
        let over = self.low > self.target;
        self.low = level;
        self.low_since = Some(now);
        over
    }

    fn lost_one(&mut self, now: Instant) {
        self.last_loss = Some(now);
        self.low_since = None;
        self.low = usize::MAX;
    }

    /// With nothing taking audio for a while, a backlog can build up that would play far behind.
    /// There's no use easing out of that: it's dropped.
    fn drop_stale_backlog(&mut self) {
        if self.level() <= self.most_frames() + self.target {
            return;
        }
        if let (Some(newest), Some(cursor)) = (self.newest, self.cursor) {
            let keep_from = newest + 1 - self.target as u32;
            self.counts.trimmed += u64::from(keep_from - cursor);
            self.packets = self.packets.split_off(&keep_from);
            self.cursor = Some(keep_from);
        }
    }

    /// The next frame, at the player's request.
    pub fn pull(&mut self, now: Instant) -> Next {
        self.resize(now);
        if !self.playing {
            let waited = self.anchor.is_some_and(|(at, _)| {
                now.duration_since(at).as_secs_f64() * 1000.0 >= self.target as f64 * self.frame_ms
            });
            if self.packets.is_empty() || (self.packets.len() < self.target && !waited) {
                return Next::Idle;
            }
            self.cursor = self.packets.keys().next().copied();
            self.playing = true;
        }
        self.drop_stale_backlog();
        let Some(cursor) = self.cursor else { return Next::Idle };

        if let Some(packet) = self.packets.remove(&cursor) {
            self.cursor = Some(cursor + 1);
            self.stalled_frames = 0;
            if self.over_full(now) {
                if let Some(following) = self.packets.remove(&(cursor + 1)) {
                    self.cursor = Some(cursor + 2);
                    self.counts.trimmed += 1;
                    return Next::Trim(packet, following);
                }
            }
            return Next::Packet(packet);
        }

        if self.packets.keys().next().is_some() {
            // Something newer is here, so this one is lost, or too late to wait for.
            self.cursor = Some(cursor + 1);
            self.lost_one(now);
            self.stalled_frames = 0;
            if let Some(next) = self.packets.get(&(cursor + 1)).filter(|_| self.repairable) {
                self.counts.repaired += 1;
                return Next::Repair(next.clone());
            }
            self.counts.concealed += 1;
            return Next::Conceal;
        }

        if self.quiet {
            self.start_again();
            return Next::Idle;
        }
        // Nothing to go on. The number stays where it is: this may be a delay, not a loss.
        self.stalled_frames += 1;
        self.counts.concealed += 1;
        if self.stalled_frames == 1 {
            self.target = (self.target + 1).min(self.most_frames());
        }
        self.lost_one(now);
        if f64::from(self.stalled_frames) * self.frame_ms >= LONGEST_STALL_MS {
            self.start_again();
            return Next::Idle;
        }
        Next::Conceal
    }
}

/// One incoming stream, from packets to audio.
pub struct RxStream {
    epoch: Option<u16>,
    decoder: Option<Decoder>,
    jitter: JitterBuffer,
    /// A decoded frame, and how much of it has gone out.
    frame: Vec<f32>,
    played: usize,
    spare: Vec<f32>,
    last_heard: Option<Instant>,
    /// Added up over the stream's restarts, for showing.
    earlier: Counts,
    reported_received: u64,
    reported_late: u64,
    reported_newest: Option<u32>,
    loss_percent: f64,
    reported_concealed: u64,
    /// Of the frames due lately, the percentage that had to be made up.
    unrepaired_percent: f64,
    /// Audio packets taken in, for telling the sender it's getting through.
    received_total: u32,
}

impl Default for RxStream {
    fn default() -> Self {
        Self {
            epoch: None,
            decoder: None,
            jitter: JitterBuffer::new(Format::Opus),
            frame: Vec::new(),
            played: 0,
            spare: Vec::new(),
            last_heard: None,
            earlier: Counts::default(),
            reported_received: 0,
            reported_late: 0,
            reported_newest: None,
            loss_percent: 0.0,
            reported_concealed: 0,
            unrepaired_percent: 0.0,
            received_total: 0,
        }
    }
}

impl RxStream {
    fn restart(&mut self, format: Format, epoch: u16) {
        let counts = self.jitter.counts();
        self.earlier.received += counts.received;
        self.earlier.repaired += counts.repaired;
        self.earlier.concealed += counts.concealed;
        self.earlier.late += counts.late;
        self.earlier.trimmed += counts.trimmed;
        self.epoch = Some(epoch);
        self.jitter = JitterBuffer::new(format);
        self.decoder = Decoder::new(format).ok();
        self.reported_received = 0;
        self.reported_late = 0;
        self.reported_newest = None;
        self.reported_concealed = 0;
    }

    /// Takes in anything but a report.
    pub fn push(&mut self, packet: &Packet, now: Instant) {
        self.last_heard = Some(now);
        match packet.body {
            Body::Audio(format, payload) => {
                let same_format = self.decoder.as_ref().is_some_and(|decoder| decoder.format() == format);
                if self.epoch != Some(packet.epoch) || !same_format {
                    self.restart(format, packet.epoch);
                }
                self.received_total = self.received_total.wrapping_add(1);
                self.jitter.push(packet.seq, payload.to_vec(), now);
            }
            Body::Quiet if self.epoch == Some(packet.epoch) => self.jitter.note_quiet(packet.seq),
            _ => {}
        }
    }

    /// Whether the other device is sending, even if only to say it's quiet.
    pub fn alive(&self, now: Instant) -> bool {
        self.last_heard.is_some_and(|at| now.duration_since(at) < ALIVE_FOR)
    }

    pub fn counts(&self) -> Counts {
        let (a, b) = (self.earlier, self.jitter.counts());
        Counts {
            received: a.received + b.received,
            repaired: a.repaired + b.repaired,
            concealed: a.concealed + b.concealed,
            late: a.late + b.late,
            trimmed: a.trimmed + b.trimmed,
        }
    }

    pub fn buffered_ms(&self) -> f64 {
        self.jitter.buffered_ms()
    }

    /// The share of packets lately that didn't arrive in time, however they were made up for.
    pub fn loss_percent(&self) -> f64 {
        self.loss_percent
    }

    /// The share of frames lately that couldn't be rebuilt and were made up.
    pub fn unrepaired_percent(&self) -> f64 {
        self.unrepaired_percent
    }

    /// The stream has ended, so what it was like no longer says anything.
    pub fn forget_quality(&mut self) {
        self.loss_percent = 0.0;
        self.unrepaired_percent = 0.0;
    }

    /// Reads the latest loss and sums it up for the sender.
    pub fn report(&mut self, playing: bool) -> Report {
        let counts = self.jitter.counts();
        let newest = self.jitter.newest();
        if let (Some(newest), Some(before)) = (newest, self.reported_newest) {
            let due = u64::from(newest.saturating_sub(before));
            let came = (counts.received - self.reported_received).saturating_sub(counts.late - self.reported_late);
            if due == 0 {
                // Nothing was due, so nothing was lost: a quiet spell lets the figures fall.
                self.loss_percent *= 1.0 - LOSS_SMOOTHING;
                self.unrepaired_percent *= 1.0 - LOSS_SMOOTHING;
            } else {
                let lost = due.saturating_sub(came) as f64 * 100.0 / due as f64;
                self.loss_percent += (lost - self.loss_percent) * LOSS_SMOOTHING;
                let made_up = (counts.concealed - self.reported_concealed) as f64 * 100.0 / due as f64;
                self.unrepaired_percent += (made_up.min(100.0) - self.unrepaired_percent) * LOSS_SMOOTHING;
            }
        }
        if newest.is_some() {
            self.reported_newest = newest;
            self.reported_received = counts.received;
            self.reported_late = counts.late;
            self.reported_concealed = counts.concealed;
        }
        Report {
            received: self.received_total,
            loss_percent: self.loss_percent.ceil().min(100.0) as u8,
            playing,
            buffered_ms: self.buffered_ms() as u16,
        }
    }

    /// Fetches one frame into `self.frame`.
    fn next_frame(&mut self, now: Instant) {
        let next = self.jitter.pull(now);
        let Some(decoder) = self.decoder.as_mut() else {
            self.frame.clear();
            self.frame.resize(super::OPUS_FRAME * CHANNELS, 0.0);
            self.played = 0;
            return;
        };
        self.frame.resize(decoder.format().frame() * CHANNELS, 0.0);
        match next {
            Next::Idle => self.frame.fill(0.0),
            Next::Packet(packet) => {
                if !decoder.packet(&packet, &mut self.frame) {
                    decoder.conceal(&mut self.frame);
                }
            }
            Next::Repair(following) => decoder.repair(&following, &mut self.frame),
            Next::Conceal => decoder.conceal(&mut self.frame),
            Next::Trim(dropped, following) => {
                // The dropped frame is decoded to keep the decoder's state right. Its start runs on
                // from what just played, so a short fade from it hides the join.
                self.spare.resize(self.frame.len(), 0.0);
                if !decoder.packet(&dropped, &mut self.spare) {
                    decoder.conceal(&mut self.spare);
                }
                if !decoder.packet(&following, &mut self.frame) {
                    decoder.conceal(&mut self.frame);
                }
                let fade = TRIM_FADE.min(self.frame.len() / CHANNELS);
                for index in 0..fade {
                    let mix = (index as f32 + 0.5) / fade as f32;
                    for channel in 0..CHANNELS {
                        let at = index * CHANNELS + channel;
                        self.frame[at] = self.spare[at] * (1.0 - mix) + self.frame[at] * mix;
                    }
                }
            }
        }
        self.played = 0;
    }

    /// Fills `out` with interleaved stereo, silence included.
    pub fn fill(&mut self, out: &mut [f32], now: Instant) {
        let mut filled = 0;
        while filled < out.len() {
            if self.played >= self.frame.len() {
                self.next_frame(now);
                self.played = self.played.min(self.frame.len());
            }
            let count = (self.frame.len() - self.played).min(out.len() - filled);
            out[filled..filled + count].copy_from_slice(&self.frame[self.played..self.played + count]);
            self.played += count;
            filled += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::codec::{Format, OpusEncoder, MAX_PACKET};
    use super::super::{OPUS_FRAME, PCM_FRAME, RATE};
    use super::*;

    const FRAME: Duration = Duration::from_millis(10);

    fn opus() -> JitterBuffer {
        JitterBuffer::new(Format::Opus)
    }

    /// Plays one frame every 10 ms against packets that arrive on time.
    fn steady(buffer: &mut JitterBuffer, start: Instant, frames: u32) -> Vec<Next> {
        (0..frames)
            .map(|i| {
                let at = start + FRAME * i;
                buffer.push(i, vec![i as u8], at);
                buffer.pull(at)
            })
            .collect()
    }

    #[test]
    fn packets_on_time_play_in_order_with_nothing_made_up() {
        let mut buffer = opus();
        let out = steady(&mut buffer, Instant::now(), 300);
        let played = out.iter().filter(|next| matches!(next, Next::Packet(_))).count();
        assert!(played >= 295, "{played}");
        assert_eq!(buffer.counts().concealed, 0);
        assert_eq!(buffer.counts().repaired, 0);
    }

    #[test]
    fn a_clean_link_settles_at_the_smallest_delay() {
        let mut buffer = opus();
        steady(&mut buffer, Instant::now(), 300);
        assert_eq!(buffer.target, 1);
        assert!(buffer.level() <= 2, "{}", buffer.level());
    }

    #[test]
    fn a_lost_packet_is_rebuilt_from_the_next_one_and_that_one_still_plays() {
        let mut buffer = opus();
        let t = Instant::now();
        for seq in [0, 1, 3] {
            buffer.push(seq, vec![seq as u8], t);
        }
        assert_eq!(buffer.pull(t), Next::Packet(vec![0]));
        assert_eq!(buffer.pull(t), Next::Packet(vec![1]));
        assert_eq!(buffer.pull(t), Next::Repair(vec![3]));
        assert_eq!(buffer.pull(t), Next::Packet(vec![3]));
        assert_eq!(buffer.counts().repaired, 1);
    }

    #[test]
    fn uncompressed_audio_has_nothing_to_repair_from() {
        let mut buffer = JitterBuffer::new(Format::Pcm);
        let t = Instant::now();
        buffer.target = 1;
        for seq in [0, 2] {
            buffer.push(seq, vec![seq as u8], t);
        }
        assert_eq!(buffer.pull(t), Next::Packet(vec![0]));
        assert_eq!(buffer.pull(t), Next::Conceal);
        assert_eq!(buffer.counts().repaired, 0);
    }

    #[test]
    fn two_lost_in_a_row_means_one_made_up_and_one_rebuilt() {
        let mut buffer = opus();
        let t = Instant::now();
        for seq in [0, 3] {
            buffer.push(seq, vec![seq as u8], t);
        }
        buffer.target = 1;
        assert_eq!(buffer.pull(t), Next::Packet(vec![0]));
        assert_eq!(buffer.pull(t), Next::Conceal);
        assert_eq!(buffer.pull(t), Next::Repair(vec![3]));
        assert_eq!(buffer.pull(t), Next::Packet(vec![3]));
    }

    #[test]
    fn a_packet_out_of_order_is_put_back_in_place() {
        let mut buffer = opus();
        let t = Instant::now();
        for seq in [0, 2, 1] {
            buffer.push(seq, vec![seq as u8], t);
        }
        for seq in 0..3 {
            assert_eq!(buffer.pull(t), Next::Packet(vec![seq]));
        }
    }

    #[test]
    fn a_packet_after_its_turn_is_counted_and_not_played() {
        let mut buffer = opus();
        let t = Instant::now();
        buffer.target = 1;
        buffer.push(0, vec![0], t);
        buffer.push(2, vec![2], t);
        buffer.pull(t);
        buffer.pull(t);
        buffer.push(1, vec![1], t);
        assert_eq!(buffer.counts().late, 1);
        assert_eq!(buffer.pull(t), Next::Packet(vec![2]));
    }

    #[test]
    fn a_stall_holds_the_place_so_a_delay_loses_nothing() {
        let mut buffer = opus();
        let t = Instant::now();
        buffer.target = 1;
        buffer.push(0, vec![0], t);
        assert_eq!(buffer.pull(t), Next::Packet(vec![0]));
        let before = buffer.target;
        assert_eq!(buffer.pull(t + FRAME), Next::Conceal);
        assert_eq!(buffer.pull(t + FRAME * 2), Next::Conceal);
        assert!(buffer.target > before, "it was too small");
        buffer.push(1, vec![1], t + FRAME * 3);
        assert_eq!(buffer.pull(t + FRAME * 3), Next::Packet(vec![1]));
    }

    #[test]
    fn a_long_silence_starts_the_buffer_again() {
        let mut buffer = opus();
        let t = Instant::now();
        buffer.target = 1;
        buffer.push(0, vec![0], t);
        buffer.pull(t);
        for i in 1..10 {
            assert_eq!(buffer.pull(t + FRAME * i), Next::Conceal);
        }
        assert_eq!(buffer.pull(t + FRAME * 10), Next::Idle);
        assert_eq!(buffer.pull(t + FRAME * 11), Next::Idle);
        let concealed = buffer.counts().concealed;
        buffer.push(90, vec![90], t + FRAME * 90);
        buffer.push(91, vec![91], t + FRAME * 91);
        buffer.target = 1;
        assert_eq!(buffer.pull(t + FRAME * 91), Next::Packet(vec![90]));
        assert_eq!(buffer.counts().concealed, concealed);
    }

    #[test]
    fn a_quiet_sender_is_not_a_loss() {
        let mut buffer = opus();
        let t = Instant::now();
        buffer.target = 1;
        buffer.push(0, vec![0], t);
        buffer.pull(t);
        buffer.note_quiet(1);
        assert_eq!(buffer.pull(t + FRAME), Next::Idle);
        assert_eq!(buffer.pull(t + FRAME * 2), Next::Idle);
        assert_eq!(buffer.counts().concealed, 0);
        // The numbers carry on from where they stopped, however much later that is.
        let later = t + Duration::from_secs(60);
        buffer.push(1, vec![1], later);
        buffer.push(2, vec![2], later);
        assert!(matches!(buffer.pull(later), Next::Packet(_)));
    }

    #[test]
    fn a_quiet_marker_overtaken_by_the_audio_after_it_says_nothing() {
        let mut buffer = opus();
        let t = Instant::now();
        buffer.target = 1;
        buffer.push(0, vec![0], t);
        buffer.push(1, vec![1], t);
        buffer.note_quiet(1);
        buffer.pull(t);
        buffer.pull(t);
        assert_eq!(buffer.pull(t + FRAME), Next::Conceal, "still expecting more");
    }

    #[test]
    fn bursty_arrivals_grow_the_buffer_so_nothing_is_lost() {
        let mut buffer = opus();
        let t = Instant::now();
        // Packets come four at a time, every 40 ms, as on a busy Wi-Fi link.
        let mut concealed_late_on = 0;
        for tick in 0..600u32 {
            let now = t + FRAME * tick;
            if tick % 4 == 3 {
                for seq in tick - 3..=tick {
                    buffer.push(seq, vec![0], now);
                }
            }
            let before = buffer.counts().concealed;
            buffer.pull(now);
            if tick > 300 {
                concealed_late_on += buffer.counts().concealed - before;
            }
        }
        assert_eq!(concealed_late_on, 0);
        assert!((4..=6).contains(&buffer.target), "{}", buffer.target);
    }

    #[test]
    fn the_buffer_shrinks_back_when_the_link_calms_down() {
        let mut buffer = opus();
        let t = Instant::now();
        for tick in 0..400u32 {
            let now = t + FRAME * tick;
            if tick % 5 == 4 {
                for seq in tick - 4..=tick {
                    buffer.push(seq, vec![0], now);
                }
            }
            buffer.pull(now);
        }
        assert!(buffer.target >= 4, "{}", buffer.target);
        for tick in 400..1200u32 {
            let now = t + FRAME * tick;
            buffer.push(tick, vec![0], now);
            buffer.pull(now);
        }
        assert_eq!(buffer.target, 1);
        assert!(buffer.level() <= 2, "{}", buffer.level());
        assert!(buffer.counts().trimmed > 0);
    }

    #[test]
    fn a_loss_keeps_a_packet_in_hand_for_repairs_for_a_while() {
        let mut buffer = opus();
        let t = Instant::now();
        let mut seq = 0;
        let mut tick = |buffer: &mut JitterBuffer, at: u32, skip: bool| {
            let now = t + FRAME * at;
            if !skip {
                buffer.push(seq, vec![0], now);
            }
            seq += 1;
            buffer.pull(now);
        };
        for at in 0..100 {
            tick(&mut buffer, at, false);
        }
        assert_eq!(buffer.target, 1);
        tick(&mut buffer, 100, true);
        for at in 101..150 {
            tick(&mut buffer, at, false);
        }
        assert_eq!(buffer.target, 2);
        for at in 150..1200 {
            tick(&mut buffer, at, false);
        }
        assert_eq!(buffer.target, 1, "and lets go once the link is clean");
    }

    #[test]
    fn the_delay_never_grows_past_the_limit() {
        let mut buffer = opus();
        let t = Instant::now();
        for tick in 0..2000u32 {
            let now = t + FRAME * tick;
            if tick % 60 == 59 {
                for seq in tick - 59..=tick {
                    buffer.push(seq, vec![0], now);
                }
            }
            buffer.pull(now);
        }
        assert!(buffer.target <= 20, "{}", buffer.target);
    }

    #[test]
    fn a_slow_player_start_is_shed_within_a_second_or_so() {
        let mut buffer = opus();
        let t = Instant::now();
        // 150 ms arrive before the first frame is asked for.
        for seq in 0..15 {
            buffer.push(seq, vec![0], t + FRAME * seq);
        }
        let mut played = 15u32;
        for tick in 15..130u32 {
            let now = t + FRAME * tick;
            buffer.push(played, vec![0], now);
            played += 1;
            buffer.pull(now);
        }
        assert!(buffer.level() <= 3, "{}", buffer.level());
    }

    #[test]
    fn a_backlog_from_a_player_that_was_away_is_dropped_outright() {
        let mut buffer = opus();
        let t = Instant::now();
        buffer.target = 2;
        buffer.push(0, vec![0], t);
        buffer.push(1, vec![1], t);
        assert_eq!(buffer.pull(t), Next::Packet(vec![0]));
        // Two seconds of audio with nobody asking for it.
        for seq in 2..202 {
            buffer.push(seq, vec![seq as u8], t + FRAME * seq);
        }
        let now = t + FRAME * 202;
        assert_eq!(buffer.pull(now), Next::Packet(vec![200]), "as near to now as the buffer goes");
        assert!(buffer.level() <= 2);
    }

    #[test]
    fn a_big_jump_in_numbers_is_a_new_stream() {
        let mut buffer = opus();
        let t = Instant::now();
        buffer.target = 1;
        buffer.push(10, vec![0], t);
        buffer.pull(t);
        buffer.push(100_000, vec![1], t + FRAME);
        assert_eq!(buffer.pull(t + FRAME), Next::Packet(vec![1]));
    }

    #[test]
    fn a_flood_of_packets_is_held_to_a_limit() {
        let mut buffer = opus();
        let t = Instant::now();
        for seq in 0..490 {
            buffer.push(seq, vec![0], t);
        }
        assert!(buffer.packets.len() <= MOST_HELD);
    }

    fn opus_packets(count: u32) -> Vec<Vec<u8>> {
        let mut encoder = OpusEncoder::new(96).unwrap();
        (0..count)
            .map(|frame| {
                let pcm: Vec<f32> = (0..OPUS_FRAME)
                    .flat_map(|i| {
                        let t = (frame as usize * OPUS_FRAME + i) as f32 / RATE as f32;
                        let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
                        [s, s]
                    })
                    .collect();
                let mut out = [0u8; MAX_PACKET];
                let len = encoder.encode(&pcm, &mut out).unwrap();
                out[..len].to_vec()
            })
            .collect()
    }

    fn audio<'a>(epoch: u16, seq: u32, format: Format, payload: &'a [u8]) -> Packet<'a> {
        Packet { epoch, seq, body: Body::Audio(format, payload) }
    }

    fn level(pcm: &[f32]) -> f32 {
        (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt()
    }

    #[test]
    fn a_stream_decodes_in_whatever_sizes_the_player_asks_for() {
        let packets = opus_packets(50);
        let mut stream = RxStream::default();
        let t = Instant::now();
        let mut out = vec![0.0; 2 * 333];
        let mut heard = 0.0f32;
        for (seq, packet) in packets.iter().enumerate() {
            let now = t + FRAME * seq as u32;
            stream.push(&audio(1, seq as u32, Format::Opus, packet), now);
            stream.fill(&mut out, now);
            heard = heard.max(level(&out));
        }
        assert!(heard > 0.15, "{heard}");
    }

    #[test]
    fn a_restarted_sender_is_picked_up_at_once() {
        let packets = opus_packets(10);
        let mut stream = RxStream::default();
        let t = Instant::now();
        let mut out = vec![0.0; OPUS_FRAME * CHANNELS];
        for seq in 0..5u32 {
            stream.push(&audio(1, 1000 + seq, Format::Opus, &packets[seq as usize]), t);
            stream.fill(&mut out, t);
        }
        for seq in 0..5u32 {
            stream.push(&audio(2, seq, Format::Opus, &packets[5 + seq as usize]), t);
        }
        assert_eq!(stream.counts().received, 10);
        assert_eq!(stream.jitter.newest(), Some(4));
    }

    #[test]
    fn changing_to_uncompressed_changes_the_frame_size_without_a_restart() {
        let packets = opus_packets(5);
        let mut stream = RxStream::default();
        let t = Instant::now();
        let mut out = vec![0.0; OPUS_FRAME * CHANNELS];
        for (seq, packet) in packets.iter().enumerate() {
            stream.push(&audio(1, seq as u32, Format::Opus, packet), t);
        }
        stream.fill(&mut out, t);
        let pcm: Vec<u8> = (0..PCM_FRAME * CHANNELS).flat_map(|_| 8000i16.to_le_bytes()).collect();
        for seq in 5..12u32 {
            stream.push(&audio(1, seq, Format::Pcm, &pcm), t + FRAME);
        }
        stream.fill(&mut out, t + FRAME * 3);
        assert!(level(&out) > 0.2, "{}", level(&out));
    }

    #[test]
    fn nothing_received_means_silence_and_no_crash() {
        let mut stream = RxStream::default();
        let mut out = vec![1.0; 100];
        let t = Instant::now();
        stream.fill(&mut out, t);
        assert_eq!(level(&out), 0.0);
        assert!(!stream.alive(t));
        assert_eq!(stream.report(false).received, 0);
    }

    #[test]
    fn a_quiet_marker_keeps_the_stream_alive_before_any_audio() {
        let mut stream = RxStream::default();
        let t = Instant::now();
        stream.push(&Packet { epoch: 1, seq: 0, body: Body::Quiet }, t);
        assert!(stream.alive(t));
        assert!(!stream.alive(t + ALIVE_FOR));
    }

    #[test]
    fn loss_is_reported_from_gaps_in_the_numbers() {
        let packets = opus_packets(1);
        let mut stream = RxStream::default();
        let t = Instant::now();
        stream.push(&audio(1, 0, Format::Opus, &packets[0]), t);
        let mut report = stream.report(true);
        for round in 0..30u32 {
            for seq in 0..100 {
                if seq % 10 != 0 {
                    stream.push(&audio(1, 1 + round * 100 + seq, Format::Opus, &packets[0]), t);
                }
            }
            report = stream.report(true);
        }
        assert!((9..=11).contains(&report.loss_percent), "{}", report.loss_percent);
        assert!(report.playing);
    }

    #[test]
    fn a_quiet_spell_lets_the_reported_loss_fall() {
        let packets = opus_packets(1);
        let mut stream = RxStream::default();
        let t = Instant::now();
        stream.push(&audio(1, 0, Format::Opus, &packets[0]), t);
        stream.report(true);
        stream.push(&audio(1, 100, Format::Opus, &packets[0]), t);
        let before = stream.report(true).loss_percent;
        assert!(before > 20, "{before}");
        for _ in 0..30 {
            stream.report(true);
        }
        assert!(stream.report(true).loss_percent <= 1);
    }

    #[test]
    fn a_clean_stream_reports_no_loss() {
        let packets = opus_packets(1);
        let mut stream = RxStream::default();
        let t = Instant::now();
        for seq in 0..500 {
            stream.push(&audio(1, seq, Format::Opus, &packets[0]), t);
            if seq % 25 == 0 {
                stream.report(true);
            }
        }
        assert_eq!(stream.report(true).loss_percent, 0);
    }
}
