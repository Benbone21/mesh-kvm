//! Mixes what's being sent and puts it on the wire.

use super::codec::{Format, OpusEncoder, MAX_PACKET};
use super::mix::{Limiter, Mixer};
use super::packet::{self, Body};
use super::{devices, note_heard_back, peak_pct, Hub, AUDIO_BITRATE_KBPS, AUDIO_TX_LOSS, AUDIO_TX_PACKETS, AUDIO_TX_PEAK_PCT};
use super::{CHANNELS, OPUS_FRAME, PCM_FRAME};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Below this a frame has nothing in it worth sending: under one step of 16-bit audio.
const SILENCE: f32 = 1.0 / 32768.0;
/// How long silence goes on before packets stop, so a sound's tail isn't cut off.
const SILENCE_HOLD_FRAMES: u32 = 20;
/// The highest loss Opus is told about. Its repair data only exists in its speech modes, and above
/// this it moves music to them too. Measured at 12% loss, that left music clearly worse than the
/// music mode with concealment: twice the spectral error, and six times the frames that were
/// audibly wrong. At this level speech still gets repair data, and music stays as it is.
const MOST_LOSS_HINT: u32 = 5;
/// With nothing to send, how often the other device is told the stream is alive.
const QUIET_EVERY: Duration = Duration::from_millis(100);
/// How long a capture can deliver nothing before that counts as quiet. Captures deliver nothing
/// at all when nothing is playing.
const QUIET_AFTER: Duration = Duration::from_millis(30);
const WAIT_WHEN_SENDING: Duration = Duration::from_millis(20);
const WAIT_WHEN_IDLE: Duration = Duration::from_millis(100);

/// Everything put on the wire, for working out what a stream really costs.
pub static BYTES_SENT: AtomicU64 = AtomicU64::new(0);

/// Tells this run's packets from the last run's: a restarted sender numbers from the start again.
fn new_epoch() -> u16 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (now.subsec_nanos() >> 8) as u16 ^ now.as_secs() as u16
}

/// Turns frames of audio into numbered packets.
pub struct Packetizer {
    epoch: u16,
    seq: u32,
    kbps: u32,
    encoder: Option<OpusEncoder>,
    silent_frames: u32,
    out: Vec<u8>,
    encoded: Vec<u8>,
}

impl Packetizer {
    pub fn new(kbps: u32) -> Self {
        Self {
            epoch: new_epoch(),
            seq: 0,
            kbps,
            encoder: None,
            silent_frames: 0,
            out: Vec::with_capacity(packet::HEADER + MAX_PACKET),
            encoded: vec![0; MAX_PACKET],
        }
    }

    /// Follows the bitrate setting and the loss the other device reports.
    pub fn set_quality(&mut self, kbps: u32, loss_percent: u32) {
        if kbps != self.kbps {
            self.kbps = kbps;
            if let Some(encoder) = &self.encoder {
                if kbps != 0 {
                    encoder.set_bitrate(kbps);
                }
            }
        }
        if let Some(encoder) = &self.encoder {
            encoder.set_loss_percent(loss_percent.min(MOST_LOSS_HINT));
        }
    }

    fn send_opus(&mut self, pcm: &[f32], send: &mut dyn FnMut(&[u8])) {
        if self.encoder.is_none() {
            self.encoder = OpusEncoder::new(self.kbps).ok();
        }
        let Some(encoder) = self.encoder.as_mut() else { return };
        if let Some(len) = encoder.encode(pcm, &mut self.encoded) {
            packet::write_audio(&mut self.out, Format::Opus, self.epoch, self.seq, &self.encoded[..len]);
            self.seq += 1;
            send(&self.out);
        }
    }

    fn send_pcm(&mut self, pcm: &[f32], send: &mut dyn FnMut(&[u8])) {
        for half in pcm.chunks_exact(PCM_FRAME * CHANNELS) {
            self.encoded.clear();
            for sample in half {
                let value = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                self.encoded.extend_from_slice(&value.to_le_bytes());
            }
            packet::write_audio(&mut self.out, Format::Pcm, self.epoch, self.seq, &self.encoded);
            self.seq += 1;
            send(&self.out);
        }
        self.encoded.resize(MAX_PACKET, 0);
    }

    /// Sends one 10 ms frame, unless it's part of a silence. Returns whether anything went out.
    pub fn frame(&mut self, pcm: &[f32], send: &mut dyn FnMut(&[u8])) -> bool {
        debug_assert_eq!(pcm.len(), OPUS_FRAME * CHANNELS);
        let silent = pcm.iter().all(|sample| sample.abs() < SILENCE);
        self.silent_frames = if silent { self.silent_frames.saturating_add(1) } else { 0 };
        if self.silent_frames > SILENCE_HOLD_FRAMES {
            // Said at once, or the far side would take the gap for a stall and wait for it.
            if self.silent_frames == SILENCE_HOLD_FRAMES + 1 {
                self.quiet(send);
                return true;
            }
            return false;
        }
        if self.kbps == 0 {
            self.send_pcm(pcm, send);
        } else {
            self.send_opus(pcm, send);
        }
        true
    }

    /// Says the stream is alive with nothing in it.
    pub fn quiet(&mut self, send: &mut dyn FnMut(&[u8])) {
        packet::write_quiet(&mut self.out, self.epoch, self.seq);
        send(&self.out);
    }
}

/// Takes in what the other device says. Reports decide whether speakers get muted, so only one
/// from the very address audio goes to counts.
fn read_reports(socket: &UdpSocket, target: SocketAddr) -> u32 {
    let mut buf = [0u8; 64];
    let mut taken = 0;
    while let Ok((len, from)) = socket.recv_from(&mut buf) {
        if from != target {
            continue;
        }
        if let Ok(packet::Packet { body: Body::Report(report), .. }) = packet::parse(&buf[..len]) {
            note_heard_back(report, Instant::now());
            taken += 1;
        }
    }
    taken
}

/// Runs until `stop`. Frames go out the moment a source has one, so this adds no delay of its own.
pub fn send_loop(socket: &UdpSocket, target: SocketAddr, hub: &Hub, stop: &AtomicBool) {
    let _ = socket.set_nonblocking(true);
    let mut packetizer = Packetizer::new(AUDIO_BITRATE_KBPS.load(Ordering::Relaxed));
    let (mut mixer, mut limiter) = (Mixer::default(), Limiter::default());
    let mut frame = vec![0.0f32; OPUS_FRAME * CHANNELS];
    let mut last_sent = Instant::now();
    let mut said_quiet = false;
    let sources = [&hub.output, &hub.mic, &hub.chime];
    let mut send = |data: &[u8]| {
        let _ = socket.send_to(data, target);
        AUDIO_TX_PACKETS.fetch_add(1, Ordering::Relaxed);
        BYTES_SENT.fetch_add(data.len() as u64, Ordering::Relaxed);
    };
    while !stop.load(Ordering::Relaxed) {
        let sending = devices::sending();
        hub.signal.wait(if sending { WAIT_WHEN_SENDING } else { WAIT_WHEN_IDLE });
        read_reports(socket, target);
        packetizer.set_quality(AUDIO_BITRATE_KBPS.load(Ordering::Relaxed), AUDIO_TX_LOSS.load(Ordering::Relaxed));
        let mut now = Instant::now();
        while mixer.ready(&sources, now) {
            mixer.mix(&sources, &mut frame);
            limiter.process(&mut frame);
            AUDIO_TX_PEAK_PCT.store(peak_pct(&frame), Ordering::Relaxed);
            if packetizer.frame(&frame, &mut send) {
                last_sent = now;
                said_quiet = false;
            }
            now = Instant::now();
        }
        let wait = if said_quiet { QUIET_EVERY } else { QUIET_AFTER };
        if now.duration_since(last_sent) >= wait {
            AUDIO_TX_PEAK_PCT.store(0, Ordering::Relaxed);
            if sending {
                packetizer.quiet(&mut send);
                last_sent = now;
                said_quiet = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::packet::{parse, Body};

    fn tone(level: f32) -> Vec<f32> {
        (0..OPUS_FRAME).flat_map(|i| [(i as f32 * 0.06).sin() * level; CHANNELS]).collect()
    }

    fn collected(packetizer: &mut Packetizer, pcm: &[f32]) -> Vec<Vec<u8>> {
        let mut sent = Vec::new();
        packetizer.frame(pcm, &mut |data| sent.push(data.to_vec()));
        sent
    }

    #[test]
    fn opus_goes_out_one_numbered_packet_per_frame() {
        let mut packetizer = Packetizer::new(96);
        for expected in 0..5 {
            let sent = collected(&mut packetizer, &tone(0.3));
            assert_eq!(sent.len(), 1);
            let packet = parse(&sent[0]).unwrap();
            assert_eq!(packet.seq, expected);
            assert!(matches!(packet.body, Body::Audio(Format::Opus, _)));
        }
    }

    #[test]
    fn uncompressed_goes_out_in_two_packets_that_fit_a_frame_on_the_network() {
        let mut packetizer = Packetizer::new(0);
        let sent = collected(&mut packetizer, &tone(0.3));
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|data| data.len() < 1400));
        assert_eq!(parse(&sent[1]).unwrap().seq, 1);
        assert!(matches!(parse(&sent[0]).unwrap().body, Body::Audio(Format::Pcm, payload) if payload.len() == 960));
    }

    #[test]
    fn changing_the_bitrate_carries_on_the_numbering_with_the_new_format() {
        let mut packetizer = Packetizer::new(96);
        collected(&mut packetizer, &tone(0.3));
        packetizer.set_quality(0, 0);
        let sent = collected(&mut packetizer, &tone(0.3));
        assert_eq!(parse(&sent[0]).unwrap().seq, 1);
        assert!(matches!(parse(&sent[0]).unwrap().body, Body::Audio(Format::Pcm, _)));
        packetizer.set_quality(64, 0);
        let sent = collected(&mut packetizer, &tone(0.3));
        assert!(matches!(parse(&sent[0]).unwrap().body, Body::Audio(Format::Opus, _)));
    }

    #[test]
    fn silence_stops_the_packets_after_a_moment_and_sound_starts_them_at_once() {
        let mut packetizer = Packetizer::new(96);
        let silence = vec![0.0; OPUS_FRAME * CHANNELS];
        for _ in 0..SILENCE_HOLD_FRAMES {
            assert_eq!(collected(&mut packetizer, &silence).len(), 1, "the tail still goes out");
        }
        let said = collected(&mut packetizer, &silence);
        assert_eq!(parse(&said[0]).unwrap().body, Body::Quiet, "the far side is told at once");
        assert!(collected(&mut packetizer, &silence).is_empty());
        assert_eq!(collected(&mut packetizer, &tone(0.3)).len(), 1);
    }

    #[test]
    fn a_quiet_marker_names_the_packet_that_comes_next() {
        let mut packetizer = Packetizer::new(96);
        collected(&mut packetizer, &tone(0.3));
        let mut sent = Vec::new();
        packetizer.quiet(&mut |data| sent.push(data.to_vec()));
        assert_eq!(parse(&sent[0]).unwrap().seq, 1);
        assert_eq!(parse(&sent[0]).unwrap().body, Body::Quiet);
    }

    #[test]
    fn only_a_report_from_the_address_audio_goes_to_counts() {
        let receiving = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiving.set_nonblocking(true).unwrap();
        let to = receiving.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stranger = UdpSocket::bind("127.0.0.2:0").unwrap();
        let mut report = Vec::new();
        packet::write_report(&mut report, 1, packet::Report::default());
        stranger.send_to(&report, to).unwrap();
        peer.send_to(&report, to).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(read_reports(&receiving, peer.local_addr().unwrap()), 1);
    }

    #[test]
    fn each_run_gets_its_own_epoch() {
        let first = Packetizer::new(96).epoch;
        std::thread::sleep(Duration::from_millis(2));
        assert_ne!(first, Packetizer::new(96).epoch);
    }
}
