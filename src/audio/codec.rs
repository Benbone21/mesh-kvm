//! Opus, and the uncompressed path that stands beside it. Encoding and decoding work on 48 kHz
//! stereo f32, interleaved.

use super::{CHANNELS, OPUS_FRAME, PCM_FRAME, RATE};
use std::os::raw::{c_int, c_void};

const OPUS_OK: c_int = 0;
const APPLICATION_AUDIO: c_int = 2049;
const SET_BITRATE: c_int = 4002;
const SET_VBR: c_int = 4006;
const SET_COMPLEXITY: c_int = 4010;
const SET_INBAND_FEC: c_int = 4012;
const SET_PACKET_LOSS_PERC: c_int = 4014;
const SET_VBR_CONSTRAINT: c_int = 4020;
/// The most an Opus frame can come to, at the highest bitrate offered, with room to spare.
pub const MAX_PACKET: usize = 1275;
/// High enough for full quality and still a small share of one core at 10 ms frames.
const COMPLEXITY: c_int = 8;

extern "C" {
    fn opus_encoder_create(rate: i32, channels: c_int, application: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_encoder_destroy(encoder: *mut c_void);
    fn opus_encoder_ctl(encoder: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encode_float(encoder: *mut c_void, pcm: *const f32, frame: c_int, data: *mut u8, room: i32) -> i32;
    fn opus_decoder_create(rate: i32, channels: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_decoder_destroy(decoder: *mut c_void);
    fn opus_decode_float(decoder: *mut c_void, data: *const u8, len: i32, pcm: *mut f32, frame: c_int, fec: c_int) -> c_int;
}

pub struct OpusEncoder {
    state: *mut c_void,
}

// Opus state is plain memory with no ties to the thread that made it.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(kbps: u32) -> Result<Self, String> {
        let mut error = 0;
        let state = unsafe { opus_encoder_create(RATE as i32, CHANNELS as c_int, APPLICATION_AUDIO, &mut error) };
        if state.is_null() || error != OPUS_OK {
            return Err(format!("the Opus encoder wouldn't start ({error})"));
        }
        let encoder = Self { state };
        encoder.set(SET_VBR, 1);
        encoder.set(SET_VBR_CONSTRAINT, 1);
        encoder.set(SET_COMPLEXITY, COMPLEXITY);
        encoder.set(SET_INBAND_FEC, 1);
        encoder.set_bitrate(kbps);
        Ok(encoder)
    }

    fn set(&self, request: c_int, value: c_int) {
        unsafe { opus_encoder_ctl(self.state, request, value) };
    }

    pub fn set_bitrate(&self, kbps: u32) {
        self.set(SET_BITRATE, (kbps * 1000) as c_int);
    }

    /// The loss the other device reports, which decides how much the encoder spends on repair data.
    pub fn set_loss_percent(&self, percent: u32) {
        self.set(SET_PACKET_LOSS_PERC, percent.min(100) as c_int);
    }

    /// One 10 ms frame into `out`, returning the bytes written.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Option<usize> {
        debug_assert_eq!(pcm.len(), OPUS_FRAME * CHANNELS);
        let written = unsafe {
            opus_encode_float(self.state, pcm.as_ptr(), OPUS_FRAME as c_int, out.as_mut_ptr(), out.len() as i32)
        };
        usize::try_from(written).ok()
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.state) };
    }
}

pub struct OpusDecoder {
    state: *mut c_void,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let state = unsafe { opus_decoder_create(RATE as i32, CHANNELS as c_int, &mut error) };
        if state.is_null() || error != OPUS_OK {
            return Err(format!("the Opus decoder wouldn't start ({error})"));
        }
        Ok(Self { state })
    }

    fn decode(&mut self, data: Option<&[u8]>, fec: bool, out: &mut [f32]) -> bool {
        let (pointer, len) = data.map_or((std::ptr::null(), 0), |d| (d.as_ptr(), d.len() as i32));
        let decoded = unsafe {
            opus_decode_float(self.state, pointer, len, out.as_mut_ptr(), OPUS_FRAME as c_int, c_int::from(fec))
        };
        decoded == OPUS_FRAME as c_int
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.state) };
    }
}

/// How long the fade of a repeated frame takes to reach silence, in frames.
const PCM_FADE_FRAMES: f32 = 4.0;

/// Stands in for a decoder on the uncompressed path, so that both go through one jitter buffer.
#[derive(Default)]
pub struct PcmDecoder {
    last: Vec<f32>,
    fade: f32,
}

impl PcmDecoder {
    fn decode(&mut self, data: &[u8], out: &mut [f32]) -> bool {
        if data.len() != out.len() * 2 {
            return false;
        }
        for (sample, bytes) in out.iter_mut().zip(data.chunks_exact(2)) {
            *sample = f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32768.0;
        }
        self.last.clear();
        self.last.extend_from_slice(out);
        self.fade = 1.0;
        true
    }

    /// Plays the last frame backwards and fading, which meets the audio before it without a click.
    /// The fade has already been applied to what's kept, so each repeat falls away faster.
    fn conceal(&mut self, out: &mut [f32]) {
        if self.last.len() != out.len() {
            out.fill(0.0);
            return;
        }
        let frames = out.len() / CHANNELS;
        let step = 1.0 / (PCM_FADE_FRAMES * frames as f32);
        for (frame, pair) in out.chunks_exact_mut(CHANNELS).enumerate() {
            let from = (frames - 1 - frame) * CHANNELS;
            self.fade = (self.fade - step).max(0.0);
            pair.copy_from_slice(&self.last[from..from + CHANNELS]);
            pair.iter_mut().for_each(|sample| *sample *= self.fade);
        }
        // Turned around, so that the next repeat runs on from this one.
        self.last.clear();
        self.last.extend_from_slice(out);
    }
}

/// What a stream's packets hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Opus,
    Pcm,
}

impl Format {
    /// Frames of audio in one packet. Uncompressed stays at 5 ms, so that a packet fits an
    /// Ethernet frame and is never split on the way.
    pub fn frame(self) -> usize {
        match self {
            Format::Opus => OPUS_FRAME,
            Format::Pcm => PCM_FRAME,
        }
    }
}

pub enum Decoder {
    Opus(OpusDecoder),
    Pcm(PcmDecoder),
}

impl Decoder {
    pub fn new(format: Format) -> Result<Self, String> {
        Ok(match format {
            Format::Opus => Decoder::Opus(OpusDecoder::new()?),
            Format::Pcm => Decoder::Pcm(PcmDecoder::default()),
        })
    }

    pub fn format(&self) -> Format {
        match self {
            Decoder::Opus(_) => Format::Opus,
            Decoder::Pcm(_) => Format::Pcm,
        }
    }

    /// Decodes one packet into `out`, which holds one frame. False leaves `out` to be concealed.
    pub fn packet(&mut self, data: &[u8], out: &mut [f32]) -> bool {
        match self {
            Decoder::Opus(decoder) => decoder.decode(Some(data), false, out),
            Decoder::Pcm(decoder) => decoder.decode(data, out),
        }
    }

    /// Rebuilds the frame before `next` from the repair data it carries.
    pub fn repair(&mut self, next: &[u8], out: &mut [f32]) {
        match self {
            Decoder::Opus(decoder) => {
                if !decoder.decode(Some(next), true, out) {
                    self.conceal(out);
                }
            }
            Decoder::Pcm(_) => self.conceal(out),
        }
    }

    /// Makes up a frame in place of one that's missing.
    pub fn conceal(&mut self, out: &mut [f32]) {
        match self {
            Decoder::Opus(decoder) => {
                if !decoder.decode(None, false, out) {
                    out.fill(0.0);
                }
            }
            Decoder::Pcm(decoder) => decoder.conceal(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frame: usize, at: usize) -> Vec<f32> {
        (0..frame)
            .flat_map(|i| {
                let t = (at * frame + i) as f32 / RATE as f32;
                let s = (t * 1000.0 * std::f32::consts::TAU).sin() * 0.3;
                [s, s]
            })
            .collect()
    }

    /// A frame's strength, the same however its samples fall.
    fn level(pcm: &[f32]) -> f32 {
        (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt()
    }

    #[test]
    fn audio_comes_through_opus_at_every_bitrate() {
        for kbps in [64, 96, 128, 256] {
            let mut encoder = OpusEncoder::new(kbps).unwrap();
            let mut decoder = Decoder::new(Format::Opus).unwrap();
            let (mut packet, mut out) = ([0u8; MAX_PACKET], vec![0.0; OPUS_FRAME * CHANNELS]);
            for at in 0..20 {
                let len = encoder.encode(&tone(OPUS_FRAME, at), &mut packet).unwrap();
                assert!(len <= kbps as usize * 1000 / 8 / 100 * 2, "{kbps}: {len} bytes");
                assert!(decoder.packet(&packet[..len], &mut out));
            }
            assert!((level(&out) - 0.212).abs() < 0.02, "{kbps}: {}", level(&out));
        }
    }

    #[test]
    fn a_missing_opus_frame_is_made_up_from_what_came_before() {
        let mut encoder = OpusEncoder::new(96).unwrap();
        let mut decoder = Decoder::new(Format::Opus).unwrap();
        let (mut packet, mut out) = ([0u8; MAX_PACKET], vec![0.0; OPUS_FRAME * CHANNELS]);
        for at in 0..10 {
            let len = encoder.encode(&tone(OPUS_FRAME, at), &mut packet).unwrap();
            decoder.packet(&packet[..len], &mut out);
        }
        decoder.conceal(&mut out);
        assert!(level(&out) > 0.1, "{}", level(&out));
    }

    #[test]
    fn at_the_top_hint_music_stays_in_the_music_mode() {
        let mut encoder = OpusEncoder::new(96).unwrap();
        encoder.set_loss_percent(5);
        let mut packet = [0u8; MAX_PACKET];
        let mut music_mode = 0;
        for frame in 0..300 {
            let pcm: Vec<f32> = (0..OPUS_FRAME)
                .flat_map(|i| {
                    let t = (frame * OPUS_FRAME + i) as f32 / RATE as f32;
                    let partial = |k: u32| (t * 330.0 * k as f32 * std::f32::consts::TAU).sin() / k as f32;
                    [(1..7).map(partial).sum::<f32>() * 0.15; CHANNELS]
                })
                .collect();
            encoder.encode(&pcm, &mut packet).unwrap();
            music_mode += usize::from(packet[0] >> 3 >= 16);
        }
        // It takes the encoder a moment to work out what it's listening to.
        assert!(music_mode > 240, "{music_mode}");
    }

    #[test]
    fn a_corrupt_packet_is_refused_not_played() {
        let mut decoder = Decoder::new(Format::Opus).unwrap();
        let mut out = vec![0.0; OPUS_FRAME * CHANNELS];
        assert!(!decoder.packet(&[0xff, 0xff, 0xff, 0xff, 0xff], &mut out));
        let mut pcm = Decoder::new(Format::Pcm).unwrap();
        let mut short = vec![0.0; PCM_FRAME * CHANNELS];
        assert!(!pcm.packet(&[1, 2, 3], &mut short));
    }

    #[test]
    fn uncompressed_audio_is_exact_and_a_gap_fades_out() {
        let mut decoder = Decoder::new(Format::Pcm).unwrap();
        let samples: Vec<i16> = (0..PCM_FRAME as i16 * 2).map(|i| i * 8).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = vec![0.0; PCM_FRAME * CHANNELS];
        assert!(decoder.packet(&bytes, &mut out));
        for (got, want) in out.iter().zip(&samples) {
            assert_eq!(*got, f32::from(*want) / 32768.0);
        }
        let mut last = f32::MAX;
        for _ in 0..PCM_FADE_FRAMES as usize {
            decoder.conceal(&mut out);
            assert!(level(&out) < last);
            last = level(&out);
        }
        assert!(last < 0.02, "{last}");
        decoder.conceal(&mut out);
        assert_eq!(level(&out), 0.0);
    }
}
