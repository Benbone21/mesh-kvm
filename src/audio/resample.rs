//! Changes sample rate between a device and the 48 kHz on the wire, for any pair of rates.
//!
//! A windowed-sinc filter, read from a table of filter phases with the gap between neighbouring
//! phases interpolated. It adds about 0.2 ms of delay, and a stream can be fed in pieces of any size.

use super::CHANNELS;

/// Input samples each output sample is built from.
const TAPS: usize = 24;
const PHASES: usize = 256;
/// Kaiser window shape: stopband about 80 dB down.
const KAISER_BETA: f64 = 8.0;
/// Where the filter starts to roll off, as a share of the lower of the two Nyquist rates.
const CUTOFF: f64 = 0.92;

/// The modified Bessel function of the first kind, order zero, for the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..32 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

/// `PHASES + 1` rows of `TAPS`, each summing to one, so that a steady level stays the same.
fn filter_table(cutoff: f64) -> Vec<f32> {
    let centre = (TAPS / 2) as f64;
    let mut table = Vec::with_capacity((PHASES + 1) * TAPS);
    for phase in 0..=PHASES {
        let offset = phase as f64 / PHASES as f64;
        let row: Vec<f64> = (0..TAPS)
            .map(|tap| {
                let x = tap as f64 - centre + 1.0 - offset;
                let sinc = if x.abs() < 1e-9 {
                    1.0
                } else {
                    (std::f64::consts::PI * x * cutoff).sin() / (std::f64::consts::PI * x * cutoff)
                };
                let edge = (x / centre).clamp(-1.0, 1.0);
                sinc * bessel_i0(KAISER_BETA * (1.0 - edge * edge).sqrt()) / bessel_i0(KAISER_BETA)
            })
            .collect();
        let sum: f64 = row.iter().sum();
        table.extend(row.iter().map(|value| (value / sum) as f32));
    }
    table
}

pub struct Resampler {
    /// Input frames per output frame.
    step: f64,
    table: Vec<f32>,
    /// Interleaved input not yet used up. The oldest frame is the start of the filter's reach.
    held: Vec<f32>,
    /// Where the next output falls, in frames from the start of `held`.
    position: f64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let cutoff = CUTOFF * (f64::from(to) / f64::from(from)).min(1.0);
        Self {
            step: f64::from(from) / f64::from(to),
            table: filter_table(cutoff),
            held: vec![0.0; (TAPS - 1) * CHANNELS],
            position: 0.0,
        }
    }

    /// Takes interleaved stereo in and adds what it can to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.held.extend_from_slice(input);
        let frames = self.held.len() / CHANNELS;
        while (self.position as usize) + TAPS <= frames {
            let start = self.position as usize;
            let spot = (self.position - start as f64) * PHASES as f64;
            let phase = spot as usize;
            let blend = (spot - phase as f64) as f32;
            let (low, high) = (&self.table[phase * TAPS..][..TAPS], &self.table[(phase + 1) * TAPS..][..TAPS]);
            let (mut left, mut right) = (0.0f32, 0.0f32);
            for tap in 0..TAPS {
                let weight = low[tap] + (high[tap] - low[tap]) * blend;
                left += self.held[(start + tap) * CHANNELS] * weight;
                right += self.held[(start + tap) * CHANNELS + 1] * weight;
            }
            out.extend_from_slice(&[left, right]);
            self.position += self.step;
        }
        let used = (self.position as usize).min(frames);
        self.held.drain(..used * CHANNELS);
        self.position -= used as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frequency: f64, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = (i as f64 / f64::from(rate) * frequency * std::f64::consts::TAU).sin() as f32 * 0.5;
                [s, -s]
            })
            .collect()
    }

    /// The strength of one frequency in the left channel, and of everything else, in dB apart.
    fn signal_to_rest(pcm: &[f32], frequency: f64, rate: u32) -> f64 {
        let left: Vec<f64> = pcm.iter().step_by(CHANNELS).map(|s| f64::from(*s)).collect();
        let w = frequency / f64::from(rate) * std::f64::consts::TAU;
        let (mut sine, mut cosine) = (0.0, 0.0);
        for (i, s) in left.iter().enumerate() {
            sine += s * (w * i as f64).sin();
            cosine += s * (w * i as f64).cos();
        }
        let (sine, cosine) = (sine * 2.0 / left.len() as f64, cosine * 2.0 / left.len() as f64);
        let rest: f64 = left
            .iter()
            .enumerate()
            .map(|(i, s)| (s - sine * (w * i as f64).sin() - cosine * (w * i as f64).cos()).powi(2))
            .sum::<f64>()
            / left.len() as f64;
        10.0 * (((sine * sine + cosine * cosine) / 2.0) / rest.max(1e-30)).log10()
    }

    fn settled(pcm: &[f32]) -> &[f32] {
        &pcm[(pcm.len() / 4) & !1..]
    }

    #[test]
    fn a_tone_comes_out_clean_at_the_rates_devices_use() {
        for from in [44_100, 88_200, 96_000, 192_000, 32_000, 16_000] {
            let mut resampler = Resampler::new(from, 48_000);
            let mut out = Vec::new();
            resampler.process(&tone(1000.0, from, from as usize / 4), &mut out);
            let clean = signal_to_rest(settled(&out), 1000.0, 48_000);
            assert!(clean > 70.0, "{from}: {clean:.1} dB");
        }
        for to in [44_100, 96_000, 16_000] {
            let mut resampler = Resampler::new(48_000, to);
            let mut out = Vec::new();
            resampler.process(&tone(1000.0, 48_000, 12_000), &mut out);
            let clean = signal_to_rest(settled(&out), 1000.0, to);
            assert!(clean > 70.0, "{to}: {clean:.1} dB");
        }
    }

    #[test]
    fn the_rate_changes_by_the_right_amount_and_channels_stay_apart() {
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        resampler.process(&tone(440.0, 44_100, 44_100), &mut out);
        assert!((out.len() as i64 / 2 - 48_000).abs() <= TAPS as i64 * 2, "{}", out.len());
        let pairs = settled(&out).chunks_exact(2);
        assert!(pairs.into_iter().all(|pair| (pair[0] + pair[1]).abs() < 1e-4), "right is left inverted");
    }

    #[test]
    fn sound_too_high_for_the_new_rate_is_removed_not_folded_back() {
        // 20 kHz can't exist at 32 kHz. Nearest-neighbour would fold it down to 12 kHz.
        let mut resampler = Resampler::new(48_000, 32_000);
        let mut out = Vec::new();
        resampler.process(&tone(20_000.0, 48_000, 48_000), &mut out);
        let peak = settled(&out).iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!(peak < 0.002, "{peak}");
    }

    #[test]
    fn pieces_of_any_size_give_the_same_result_as_one() {
        let input = tone(700.0, 44_100, 5000);
        let mut whole = Vec::new();
        Resampler::new(44_100, 48_000).process(&input, &mut whole);
        let mut pieces = Vec::new();
        let mut resampler = Resampler::new(44_100, 48_000);
        for chunk in input.chunks(2 * 37) {
            resampler.process(chunk, &mut pieces);
        }
        assert_eq!(whole.len(), pieces.len());
        assert!(whole.iter().zip(&pieces).all(|(a, b)| (a - b).abs() < 1e-5));
    }

    #[test]
    fn a_steady_level_stays_steady() {
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        resampler.process(&vec![0.25; 8000], &mut out);
        assert!(settled(&out).iter().all(|s| (s - 0.25).abs() < 1e-4));
    }
}
