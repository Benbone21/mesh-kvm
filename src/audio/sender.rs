//! Mixes what's being sent and puts it on the wire.

use super::codec::{Format, OpusEncoder, MAX_PACKET};
use super::mix::{Limiter, Mixer};
use super::packet::{self, Body};
use super::{devices, note_heard_back, note_other_version, peak_pct, Hub, AUDIO_BITRATE_KBPS, AUDIO_TX_LOSS, AUDIO_TX_PACKETS, AUDIO_TX_PEAK_PCT, AUDIO_TX_SOUND_PACKETS};
use super::{CHANNELS, OPUS_FRAME, PCM_FRAME};
use crate::secure::datagram::{Channel, Opened, OVERHEAD};
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
/// `tick` runs after this, so it stays well inside the 250 ms that handshakes go by.
const WAIT_WHEN_IDLE: Duration = Duration::from_millis(100);
/// Reports come four times a second. Anything past this is a flood, and waits for the next pass so
/// that it can't hold up sound or `tick`.
const MOST_READ_PER_PASS: usize = 64;

/// Everything put on the wire, sealed, for working out what a stream really costs.
pub static BYTES_SENT: AtomicU64 = AtomicU64::new(0);

/// Tells this run's packets from the last run's: a restarted sender numbers from the start again.
fn new_epoch() -> u16 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (now.subsec_nanos() >> 8) as u16 ^ now.as_secs() as u16
}

/// What became of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    Nothing,
    /// Something went out, with nothing to hear in it.
    Silence,
    Sound,
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

    /// Sends one 10 ms frame, unless it's part of a silence.
    pub fn frame(&mut self, pcm: &[f32], send: &mut dyn FnMut(&[u8])) -> Sent {
        debug_assert_eq!(pcm.len(), OPUS_FRAME * CHANNELS);
        let silent = pcm.iter().all(|sample| sample.abs() < SILENCE);
        self.silent_frames = if silent { self.silent_frames.saturating_add(1) } else { 0 };
        if self.silent_frames > SILENCE_HOLD_FRAMES {
            // Said at once, or the far side would take the gap for a stall and wait for it.
            if self.silent_frames == SILENCE_HOLD_FRAMES + 1 {
                self.quiet(send);
                return Sent::Silence;
            }
            return Sent::Nothing;
        }
        if self.kbps == 0 {
            self.send_pcm(pcm, send);
        } else {
            self.send_opus(pcm, send);
        }
        if silent {
            Sent::Silence
        } else {
            Sent::Sound
        }
    }

    /// Says the stream is alive with nothing in it.
    pub fn quiet(&mut self, send: &mut dyn FnMut(&[u8])) {
        packet::write_quiet(&mut self.out, self.epoch, self.seq);
        send(&self.out);
    }
}

/// Takes in what the other device says, answering a handshake if that's what came. Reports decide
/// whether speakers get muted, so only one from the session and from the very address audio goes
/// to counts.
fn read_reports(socket: &UdpSocket, target: SocketAddr, channel: &Channel, now_ms: u64) -> u32 {
    let mut buf = [0u8; 256];
    let mut opened = Vec::new();
    let mut taken = 0;
    for _ in 0..MOST_READ_PER_PASS {
        let Ok((len, from)) = socket.recv_from(&mut buf) else { break };
        if from != target {
            continue;
        }
        match channel.open(&buf[..len], now_ms, &mut opened) {
            Opened::Data(plain) => {
                if let Ok(packet::Packet { body: Body::Report(report), .. }) = packet::parse(plain) {
                    note_heard_back(report, Instant::now());
                    taken += 1;
                }
            }
            Opened::Reply(reply) => {
                put(socket, reply, from);
            }
            Opened::OldPeer => note_other_version(Instant::now()),
            Opened::Heard | Opened::Refused => {}
        }
    }
    taken
}

/// Puts a datagram on the network, and says if it went.
fn put(socket: &UdpSocket, data: &[u8], to: SocketAddr) -> bool {
    let went = socket.send_to(data, to).is_ok();
    if went {
        BYTES_SENT.fetch_add(data.len() as u64, Ordering::Relaxed);
    }
    went
}

/// Where audio goes out, encrypted.
struct Wire<'a> {
    socket: &'a UdpSocket,
    target: SocketAddr,
    channel: &'a Channel,
    sealed: Vec<u8>,
}

impl<'a> Wire<'a> {
    fn new(socket: &'a UdpSocket, target: SocketAddr, channel: &'a Channel) -> Self {
        Self { socket, target, channel, sealed: Vec::with_capacity(packet::HEADER + MAX_PACKET + OVERHEAD) }
    }

    /// With no session yet this is a lost packet, and nothing was sent.
    fn send(&mut self, data: &[u8]) -> bool {
        let went = self.channel.seal(data, &mut self.sealed) && put(self.socket, &self.sealed, self.target);
        if went {
            AUDIO_TX_PACKETS.fetch_add(1, Ordering::Relaxed);
        }
        went
    }
}

/// Sends one frame. `Sent::Nothing` if none of it went, as nothing else may think sound is leaving:
/// the mute decision reads what comes of this.
fn send_frame(packetizer: &mut Packetizer, wire: &mut Wire, pcm: &[f32]) -> Sent {
    let mut went = false;
    let sent = packetizer.frame(pcm, &mut |data| went |= wire.send(data));
    if went {
        sent
    } else {
        Sent::Nothing
    }
}

/// Runs until `stop`. Frames go out the moment a source has one, so this adds no delay of its own.
pub fn send_loop(socket: &UdpSocket, target: SocketAddr, channel: &Channel, hub: &Hub, stop: &AtomicBool) {
    let _ = socket.set_nonblocking(true);
    let clock = Instant::now();
    let mut packetizer = Packetizer::new(AUDIO_BITRATE_KBPS.load(Ordering::Relaxed));
    let (mut mixer, mut limiter) = (Mixer::default(), Limiter::default());
    let mut frame = vec![0.0f32; OPUS_FRAME * CHANNELS];
    let mut last_sent = Instant::now();
    let mut said_quiet = false;
    let sources = [&hub.output, &hub.mic, &hub.chime];
    let mut wire = Wire::new(socket, target, channel);
    while !stop.load(Ordering::Relaxed) {
        let sending = devices::sending();
        hub.signal.wait(if sending { WAIT_WHEN_SENDING } else { WAIT_WHEN_IDLE });
        let now_ms = clock.elapsed().as_millis() as u64;
        read_reports(socket, target, channel, now_ms);
        // Every pass, sound or none, so the session is ready before the first of it.
        channel.tick(now_ms, &mut |packet| {
            put(socket, packet, target);
        });
        packetizer.set_quality(AUDIO_BITRATE_KBPS.load(Ordering::Relaxed), AUDIO_TX_LOSS.load(Ordering::Relaxed));
        let mut now = Instant::now();
        while mixer.ready(&sources, now) {
            mixer.mix(&sources, &mut frame);
            limiter.process(&mut frame);
            AUDIO_TX_PEAK_PCT.store(peak_pct(&frame), Ordering::Relaxed);
            let sent = send_frame(&mut packetizer, &mut wire, &frame);
            if sent != Sent::Nothing {
                last_sent = now;
                said_quiet = false;
            }
            if sent == Sent::Sound {
                AUDIO_TX_SOUND_PACKETS.fetch_add(1, Ordering::Relaxed);
            }
            now = Instant::now();
        }
        let wait = if said_quiet { QUIET_EVERY } else { QUIET_AFTER };
        if now.duration_since(last_sent) >= wait {
            AUDIO_TX_PEAK_PCT.store(0, Ordering::Relaxed);
            if sending {
                packetizer.quiet(&mut |data| {
                    wire.send(data);
                });
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
    use crate::audio::tests::{due, figures_to_myself, linked_channels, paired_channels, sealed};
    use crate::secure::datagram::LinkSecurity;

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
    fn only_a_frame_with_something_in_it_counts_as_sound() {
        let mut packetizer = Packetizer::new(96);
        let mut ignore = |_: &[u8]| {};
        assert_eq!(packetizer.frame(&tone(0.3), &mut ignore), Sent::Sound);
        let silence = vec![0.0; OPUS_FRAME * CHANNELS];
        assert_eq!(packetizer.frame(&silence, &mut ignore), Sent::Silence, "a tail going out");
        for _ in 0..SILENCE_HOLD_FRAMES {
            assert_eq!(packetizer.frame(&silence, &mut ignore), Sent::Silence);
        }
        assert_eq!(packetizer.frame(&silence, &mut ignore), Sent::Nothing);
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

    fn report() -> Vec<u8> {
        let mut report = Vec::new();
        packet::write_report(&mut report, 1, packet::Report::default());
        report
    }

    fn receiving() -> (UdpSocket, SocketAddr) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let to = socket.local_addr().unwrap();
        (socket, to)
    }

    #[test]
    fn only_a_report_from_the_address_audio_goes_to_counts() {
        let (socket, to) = receiving();
        let (channel, far_end) = linked_channels();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stranger = UdpSocket::bind("127.0.0.2:0").unwrap();
        stranger.send_to(&sealed(&far_end, &report()), to).unwrap();
        peer.send_to(&sealed(&far_end, &report()), to).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(read_reports(&socket, peer.local_addr().unwrap(), &channel, 1), 1);
    }

    #[test]
    fn reports_only_count_from_the_session() {
        let _figures = figures_to_myself();
        let (socket, to) = receiving();
        let (channel, far_end) = linked_channels();
        let (_, other_end) = linked_channels();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target = peer.local_addr().unwrap();

        peer.send_to(&report(), to).unwrap();
        peer.send_to(&sealed(&other_end, &report()), to).unwrap();
        // A keepalive is real, and says nothing of how the far side is doing.
        peer.send_to(&sealed(&far_end, &[]), to).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(read_reports(&socket, target, &channel, 1), 0);

        peer.send_to(&sealed(&far_end, &report()), to).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(read_reports(&socket, target, &channel, 2), 1);
    }

    #[test]
    fn a_handshake_answer_gets_its_keepalive_back() {
        let _figures = figures_to_myself();
        let (socket, to) = receiving();
        let (channel, far_end) = paired_channels();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let hello = due(&channel, 0).remove(0);
        let mut answer = Vec::new();
        assert!(matches!(far_end.open(&hello, 0, &mut answer), Opened::Reply(_)));
        peer.send_to(&answer, to).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(read_reports(&socket, peer.local_addr().unwrap(), &channel, 1), 0);
        assert_eq!(channel.security(), LinkSecurity::Encrypted);
        let mut buf = [0u8; 256];
        let (len, _) = peer.recv_from(&mut buf).unwrap();
        assert_eq!(far_end.open(&buf[..len], 1, &mut Vec::new()), Opened::Heard);
    }

    #[test]
    fn a_flood_cant_hold_up_a_pass() {
        let (socket, to) = receiving();
        let (channel, _) = linked_channels();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        for _ in 0..MOST_READ_PER_PASS + 1 {
            peer.send_to(&[0; 8], to).unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
        read_reports(&socket, peer.local_addr().unwrap(), &channel, 1);
        assert!(socket.recv_from(&mut [0u8; 16]).is_ok(), "one was left for the next pass");
    }

    #[test]
    fn only_what_went_out_is_counted() {
        let _figures = figures_to_myself();
        let (socket, target) = receiving();
        let sending = UdpSocket::bind("127.0.0.1:0").unwrap();
        let counts = || (AUDIO_TX_PACKETS.load(Ordering::SeqCst), BYTES_SENT.load(Ordering::SeqCst));
        let before = counts();

        let (no_session, _) = paired_channels();
        let mut wire = Wire::new(&sending, target, &no_session);
        let mut packetizer = Packetizer::new(96);
        assert_eq!(send_frame(&mut packetizer, &mut wire, &tone(0.3)), Sent::Nothing, "dropped, as a lost packet is");
        assert_eq!(counts(), before);
        assert!(socket.recv_from(&mut [0u8; 16]).is_err());

        let (channel, far_end) = linked_channels();
        let mut wire = Wire::new(&sending, target, &channel);
        assert_eq!(send_frame(&mut packetizer, &mut wire, &tone(0.3)), Sent::Sound);
        std::thread::sleep(Duration::from_millis(10));
        let mut buf = [0u8; 2048];
        let (len, _) = socket.recv_from(&mut buf).unwrap();
        assert_eq!(counts(), (before.0 + 1, before.1 + len as u64), "what's on the wire, sealed");
        let mut plain = Vec::new();
        let Opened::Data(bytes) = far_end.open(&buf[..len], 1, &mut plain) else { panic!("not sealed") };
        assert!(matches!(parse(bytes).unwrap().body, Body::Audio(Format::Opus, _)));
    }

    #[test]
    fn each_run_gets_its_own_epoch() {
        let first = Packetizer::new(96).epoch;
        std::thread::sleep(Duration::from_millis(2));
        assert_ne!(first, Packetizer::new(96).epoch);
    }
}
