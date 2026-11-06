//! Brings the chosen sources together: a queue for each, and a limiter so the sum can't clip.

use super::{CHANNELS, OPUS_FRAME};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

const FRAME_SAMPLES: usize = OPUS_FRAME * CHANNELS;
/// A queue above this has fallen behind, for instance through clock drift between two devices.
const MOST_QUEUED: usize = FRAME_SAMPLES * 6;
/// What's left after falling behind.
const QUEUE_AFTER_TRIM: usize = FRAME_SAMPLES * 2;
/// After running dry, a source that isn't leading waits for this much before it joins in again.
const REJOIN_AT: usize = FRAME_SAMPLES * 2;
/// Below this a capture has nothing in it: one step of 16-bit audio.
const SOUND_FLOOR: f32 = 1.0 / 32768.0;
/// How long a lead source can go without a frame before another takes over.
const LEAD_PATIENCE: Duration = Duration::from_millis(30);
/// The limiter holds the sum this far below full scale.
const CEILING: f32 = 0.97;
/// How much of the way back to full gain the limiter goes in one frame: about 150 ms in all.
const RELEASE_PER_FRAME: f32 = 0.065;

#[derive(Default)]
struct Queue {
    samples: VecDeque<f32>,
    /// Dry and waiting for `REJOIN_AT`.
    refilling: bool,
    activity: Activity,
}

/// What a capture has delivered lately. A capture delivering silence is working: it's a capture
/// delivering nothing that may not be.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Activity {
    /// When anything last arrived, silence included.
    pub data: Option<Instant>,
    /// When real sound last arrived.
    pub sound: Option<Instant>,
}

impl Queue {
    fn has_frame(&self) -> bool {
        self.samples.len() >= if self.refilling { REJOIN_AT } else { FRAME_SAMPLES }
    }
}

/// Where a capture leaves its audio. Interleaved stereo at 48 kHz.
#[derive(Default)]
pub struct Source {
    queue: Mutex<Queue>,
}

impl Source {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn push(&self, samples: &[f32], signal: &Signal) {
        let now = Instant::now();
        let mut queue = self.lock();
        queue.activity.data = Some(now);
        if samples.iter().any(|sample| sample.abs() >= SOUND_FLOOR) {
            queue.activity.sound = Some(now);
        }
        queue.samples.extend(samples);
        if queue.samples.len() > MOST_QUEUED {
            let excess = queue.samples.len() - QUEUE_AFTER_TRIM;
            queue.samples.drain(..excess);
        }
        drop(queue);
        signal.notify();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().samples.len()
    }

    pub fn activity(&self) -> Activity {
        self.lock().activity
    }

    /// True when `add_to` would take a frame. The two must agree, or the sender is told a frame is
    /// ready that never comes out, and spins.
    fn has_frame(&self) -> bool {
        self.lock().has_frame()
    }

    /// Adds one frame to `mix`, or nothing when it hasn't got one.
    fn add_to(&self, mix: &mut [f32]) {
        let mut queue = self.lock();
        if !queue.has_frame() {
            queue.refilling = true;
            return;
        }
        queue.refilling = false;
        for (sample, value) in mix.iter_mut().zip(queue.samples.drain(..FRAME_SAMPLES)) {
            *sample += value;
        }
    }

    pub fn clear(&self) {
        *self.lock() = Queue::default();
    }
}

/// Wakes the sender when a source has something for it.
#[derive(Default)]
pub struct Signal {
    woken: Mutex<bool>,
    condvar: Condvar,
}

impl Signal {
    pub fn notify(&self) {
        *self.woken.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.condvar.notify_one();
    }

    pub fn wait(&self, longest: Duration) {
        let woken = self.woken.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut woken, _) = self
            .condvar
            .wait_timeout_while(woken, longest, |woken| !*woken)
            .unwrap_or_else(PoisonError::into_inner);
        *woken = false;
    }
}

/// Picks which source sets the pace, so that two sources on different clocks still make one
/// stream of frames. The others join in with whatever they have when the lead has a frame.
#[derive(Default)]
pub struct Mixer {
    lead: Option<usize>,
    lead_fed: Option<Instant>,
}

impl Mixer {
    /// True when a frame is due. A lead that goes quiet hands over after a moment.
    pub fn ready(&mut self, sources: &[&Source], now: Instant) -> bool {
        if self.lead.and_then(|lead| sources.get(lead)).is_some_and(|lead| lead.has_frame()) {
            self.lead_fed = Some(now);
            return true;
        }
        if self.lead_fed.is_some_and(|fed| now.duration_since(fed) < LEAD_PATIENCE) {
            return false;
        }
        self.lead = sources.iter().position(|source| source.has_frame());
        self.lead_fed = self.lead.map(|_| now);
        self.lead.is_some()
    }

    /// One frame of everything that has one.
    pub fn mix(&self, sources: &[&Source], mix: &mut [f32]) {
        mix.fill(0.0);
        for source in sources {
            source.add_to(mix);
        }
    }
}

/// Turns the gain down exactly as far as a frame needs and brings it back smoothly. The frame is
/// read before any of it goes out, so the gain gets there in time without clipping first.
pub struct Limiter {
    gain: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl Limiter {
    pub fn process(&mut self, frame: &mut [f32]) {
        let peak = frame.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let needed = if peak > CEILING { CEILING / peak } else { 1.0 };
        let target = needed.min(self.gain + (1.0 - self.gain) * RELEASE_PER_FRAME);
        // A drop applies from the first sample, or the start of the frame would get through.
        let start = target.min(self.gain);
        let slope = (target - start) / (frame.len() / CHANNELS).max(1) as f32;
        for (index, pair) in frame.chunks_exact_mut(CHANNELS).enumerate() {
            let gain = start + slope * index as f32;
            for sample in pair {
                *sample *= gain;
            }
        }
        self.gain = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frequency: f32, level: f32, at: usize) -> Vec<f32> {
        (0..OPUS_FRAME)
            .flat_map(|i| {
                let t = (at * OPUS_FRAME + i) as f32 / 48_000.0;
                let s = (t * frequency * std::f32::consts::TAU).sin() * level;
                [s, s]
            })
            .collect()
    }

    fn peak(frame: &[f32]) -> f32 {
        frame.iter().fold(0.0, |peak, s| peak.max(s.abs()))
    }

    #[test]
    fn two_full_sources_never_clip_and_quiet_ones_pass_untouched() {
        let (a, b, signal) = (Source::default(), Source::default(), Signal::default());
        let (mixer, mut limiter) = (Mixer::default(), Limiter::default());
        let mut mix = vec![0.0; FRAME_SAMPLES];
        for at in 0..100 {
            a.push(&tone(440.0, 1.0, at), &signal);
            b.push(&tone(660.0, 1.0, at), &signal);
            mixer.mix(&[&a, &b], &mut mix);
            limiter.process(&mut mix);
            assert!(peak(&mix) <= CEILING + 1e-5, "{at}: {}", peak(&mix));
        }
        let mut limiter = Limiter::default();
        let quiet = tone(440.0, 0.3, 0);
        let mut frame = quiet.clone();
        limiter.process(&mut frame);
        assert_eq!(frame, quiet);
    }

    #[test]
    fn the_limiter_gives_the_gain_back_without_a_jump() {
        let mut limiter = Limiter::default();
        let mut loud = tone(440.0, 2.0, 0);
        limiter.process(&mut loud);
        assert!(limiter.gain < 0.5);
        let mut previous = limiter.gain;
        for at in 1..60 {
            let mut quiet = tone(440.0, 0.1, at);
            limiter.process(&mut quiet);
            assert!(limiter.gain >= previous && limiter.gain - previous < 0.1);
            previous = limiter.gain;
        }
        assert!(limiter.gain > 0.95, "{}", limiter.gain);
    }

    #[test]
    fn a_source_that_falls_behind_can_only_add_a_bounded_delay() {
        let (source, signal) = (Source::default(), Signal::default());
        for at in 0..50 {
            source.push(&tone(440.0, 0.5, at), &signal);
        }
        assert!(source.len() <= MOST_QUEUED);
    }

    #[test]
    fn a_source_short_of_a_frame_sits_out_until_it_has_some_in_hand() {
        let (lead, other, signal) = (Source::default(), Source::default(), Signal::default());
        let mut mixer = Mixer::default();
        let mut mix = vec![0.0; FRAME_SAMPLES];
        lead.push(&tone(440.0, 0.5, 0), &signal);
        other.push(&tone(660.0, 0.5, 0)[..100], &signal);
        assert!(mixer.ready(&[&lead, &other], Instant::now()));
        mixer.mix(&[&lead, &other], &mut mix);
        assert_eq!(other.len(), 100, "a broken-up frame would crackle");
        other.push(&tone(660.0, 0.5, 1), &signal);
        mix.fill(0.0);
        other.add_to(&mut mix);
        assert_eq!(peak(&mix), 0.0, "waits for a little in hand, not a bare frame");
        other.push(&tone(660.0, 0.5, 2), &signal);
        other.add_to(&mut mix);
        assert!(peak(&mix) > 0.0);
    }

    #[test]
    fn two_sources_out_of_step_still_make_one_frame_per_lead_frame() {
        let (a, b, signal) = (Source::default(), Source::default(), Signal::default());
        let mut mixer = Mixer::default();
        let mut mix = vec![0.0; FRAME_SAMPLES];
        let (start, mut frames) = (Instant::now(), 0);
        for ms in 0..1000u64 {
            let now = start + Duration::from_millis(ms);
            if ms % 10 == 0 {
                a.push(&tone(440.0, 0.5, 0), &signal);
            }
            if ms % 10 == 3 {
                b.push(&tone(660.0, 0.5, 0), &signal);
            }
            while mixer.ready(&[&a, &b], now) {
                mixer.mix(&[&a, &b], &mut mix);
                frames += 1;
            }
        }
        assert_eq!(frames, 100);
    }

    #[test]
    fn a_lead_that_goes_quiet_hands_over_and_a_new_source_can_start_alone() {
        let (a, b, signal) = (Source::default(), Source::default(), Signal::default());
        let mut mixer = Mixer::default();
        let mut mix = vec![0.0; FRAME_SAMPLES];
        let start = Instant::now();
        a.push(&tone(440.0, 0.5, 0), &signal);
        assert!(mixer.ready(&[&a, &b], start));
        mixer.mix(&[&a, &b], &mut mix);
        // It sat out that frame, so it needs a little in hand, as any source does to join in.
        b.push(&tone(660.0, 0.5, 0), &signal);
        b.push(&tone(660.0, 0.5, 1), &signal);
        assert!(!mixer.ready(&[&a, &b], start + Duration::from_millis(10)), "a may still come");
        assert!(mixer.ready(&[&a, &b], start + LEAD_PATIENCE), "b takes over");
    }

    #[test]
    fn a_source_tells_silence_from_nothing_at_all() {
        let (source, signal) = (Source::default(), Signal::default());
        assert_eq!(source.activity(), Activity::default());
        source.push(&[0.0; 8], &signal);
        assert!(source.activity().data.is_some());
        assert!(source.activity().sound.is_none(), "silence is delivery, not sound");
        source.push(&[0.0, 0.2], &signal);
        assert!(source.activity().sound.is_some());
        source.clear();
        assert_eq!(source.activity(), Activity::default(), "a new capture starts afresh");
    }

    #[test]
    fn a_refilling_lead_is_not_ready_until_it_can_really_give_a_frame() {
        let (lead, signal) = (Source::default(), Signal::default());
        let mut mixer = Mixer::default();
        let mut mix = vec![0.0; FRAME_SAMPLES];
        let now = Instant::now();
        lead.push(&tone(440.0, 0.5, 0), &signal);
        assert!(mixer.ready(&[&lead], now));
        mixer.mix(&[&lead], &mut mix);
        lead.push(&tone(440.0, 0.5, 1)[..100], &signal);
        mixer.mix(&[&lead], &mut mix);
        // Short of a frame, so it's refilling. One whole frame isn't enough to rejoin.
        lead.push(&tone(440.0, 0.5, 2), &signal);
        assert!(!mixer.ready(&[&lead], now), "the sender would spin on a frame that never comes out");
        lead.push(&tone(440.0, 0.5, 3), &signal);
        assert!(mixer.ready(&[&lead], now));
        let before = lead.len();
        mixer.mix(&[&lead], &mut mix);
        assert_eq!(lead.len(), before - FRAME_SAMPLES);
    }

    #[test]
    fn a_signal_wakes_the_sender_at_once_or_times_out() {
        let (source, signal) = (Source::default(), Signal::default());
        let started = std::time::Instant::now();
        signal.wait(Duration::from_millis(5));
        assert!(started.elapsed() >= Duration::from_millis(4));
        source.push(&[0.0; 2], &signal);
        let started = std::time::Instant::now();
        signal.wait(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
