//! The audio bus (UDP 24851), the same in both directions on every device.
//!
//! Each device can send a microphone or line-in, the sound of an output, or both mixed. It can play
//! what it receives on an output of its own. On the wire: Opus in 10 ms frames, or plain PCM in 5 ms
//! frames, in the format `packet` describes. `jitter` turns packets back into steady audio.
//!
//! Linux uses PipeWire through its command line tools, Windows uses WASAPI, and each lives in a
//! module of its own. Everything else here is the same on both.

pub mod codec;
pub mod devices;
pub mod jitter;
pub mod mix;
pub mod packet;
// PipeWire converts to and from 48 kHz by itself, so only Windows needs this.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod resample;
pub mod sender;

#[cfg(all(test, target_os = "linux"))]
mod live;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "windows")]
mod wasapi;
#[cfg(target_os = "windows")]
use wasapi as platform;

use crate::secure::datagram::{Channel, Opened};
use jitter::RxStream;
use packet::{Body, Refused};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 10 ms, the length of an Opus packet.
pub const OPUS_FRAME: usize = 480;
/// 5 ms, the length of an uncompressed packet.
pub const PCM_FRAME: usize = 240;

/// Bitrates on offer, in kbit/s. Zero means uncompressed.
pub const BITRATES: [u32; 5] = [64, 96, 128, 256, 0];
pub const DEFAULT_BITRATE: u32 = 96;

pub static AUDIO_RX_MUTED: AtomicBool = AtomicBool::new(false);
pub static AUDIO_RX_VOLUME_PCT: AtomicU32 = AtomicU32::new(100);
pub static AUDIO_RX_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static AUDIO_RX_PEAK_PCT: AtomicU32 = AtomicU32::new(0);
/// Frames lately that were lost on the way, and that couldn't be rebuilt, in tenths of a percent.
pub static AUDIO_RX_LOSS: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_RX_UNREPAIRED: AtomicU32 = AtomicU32::new(0);
/// How much delay the jitter buffer is holding, in ms.
pub static AUDIO_RX_BUFFER_MS: AtomicU32 = AtomicU32::new(0);
/// A stream from the other device is alive, so the figures above mean something.
pub static AUDIO_RX_LIVE: AtomicBool = AtomicBool::new(false);
/// The output is open and not switched off, so what's received can be heard.
pub static AUDIO_PLAYER_LIVE: AtomicBool = AtomicBool::new(false);

pub static AUDIO_TX_VOLUME_PCT: AtomicU32 = AtomicU32::new(100);
pub static AUDIO_TX_PACKETS: AtomicU64 = AtomicU64::new(0);
/// Of those, the ones with real sound from the output's capture in them, not silence, word that the
/// stream is alive, or a microphone alone.
pub static AUDIO_TX_SOUND_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static AUDIO_TX_PEAK_PCT: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_BITRATE_KBPS: AtomicU32 = AtomicU32::new(DEFAULT_BITRATE);
/// What the other device says it is losing of what's sent, in percent.
pub static AUDIO_TX_LOSS: AtomicU32 = AtomicU32::new(0);
/// The capture of an output is running.
pub static AUDIO_OUTPUT_CAPTURE_LIVE: AtomicBool = AtomicBool::new(false);
/// Muting the output won't silence what's captured from it. On Linux that's always so, as capture
/// comes straight from the apps. On Windows, ordinary loopback follows the endpoint's mute and volume
/// exactly, so it's only so while process exclusion is really running.
pub static AUDIO_CAPTURE_SURVIVES_MUTE: AtomicBool = AtomicBool::new(cfg!(not(target_os = "windows")));
pub static AUDIO_PLAY_ONLY_ON_OTHER: AtomicBool = AtomicBool::new(true);
/// Whether the link is up. Stored by the input loops, read by the mute thread.
pub static AUDIO_PEER_ONLINE: AtomicBool = AtomicBool::new(false);

/// How often the receiver tells the sender how it's doing.
const REPORT_EVERY: Duration = Duration::from_millis(250);
/// A wait short enough to send reports on time when nothing is arriving, and to keep `tick` far
/// inside the 250 ms it paces handshakes by.
const RECEIVE_WAIT: Duration = Duration::from_millis(50);

/// What the sender last heard back.
#[derive(Clone, Copy, Default)]
struct HeardBack {
    at: Option<Instant>,
    playing: bool,
    sounding: bool,
    ready: bool,
}

static HEARD_BACK: Mutex<HeardBack> = Mutex::new(HeardBack { at: None, playing: false, sounding: false, ready: false });

fn note_heard_back(report: packet::Report, now: Instant) {
    *HEARD_BACK.lock().unwrap_or_else(PoisonError::into_inner) =
        HeardBack { at: Some(now), playing: report.playing, sounding: report.sounding, ready: report.ready };
    AUDIO_TX_LOSS.store(u32::from(report.loss_percent), Ordering::Relaxed);
}

fn heard_back(now: Instant, within: Duration) -> HeardBack {
    let heard = *HEARD_BACK.lock().unwrap_or_else(PoisonError::into_inner);
    if heard.at.is_some_and(|at| now.duration_since(at) < within) {
        heard
    } else {
        HeardBack::default()
    }
}

/// True while the other device says real sound from here is coming out of its output.
pub fn reaching_peer(now: Instant, within: Duration) -> bool {
    heard_back(now, within).sounding
}

/// True while the other device has an output open for what's sent, sound or no sound.
pub fn peer_listening(now: Instant, within: Duration) -> bool {
    heard_back(now, within).playing
}

/// True while the other device says it would play what's sent: Play is on there, even if its output
/// is shut for now for want of sound.
pub fn peer_ready(now: Instant, within: Duration) -> bool {
    heard_back(now, within).ready
}

/// True while reports are coming from the other device at all.
pub fn peer_reporting(now: Instant, within: Duration) -> bool {
    heard_back(now, within).at.is_some()
}

/// What is being sent out, ready for the sender.
#[derive(Default)]
pub struct Hub {
    pub mic: mix::Source,
    pub output: mix::Source,
    pub chime: mix::Source,
    pub signal: mix::Signal,
}

pub static HUB: LazyLock<Hub> = LazyLock::new(Hub::default);

impl Hub {
    /// Hands over captured audio, at the volume chosen for sending.
    pub fn capture(&self, source: &mix::Source, samples: &mut [f32]) {
        let volume = AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed);
        if volume != 100 {
            let gain = volume as f32 / 100.0;
            samples.iter_mut().for_each(|sample| *sample *= gain);
        }
        source.push(samples, &self.signal);
    }
}

fn peak_pct(samples: &[f32]) -> u32 {
    let peak = samples.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    (peak * 100.0).min(100.0) as u32
}

/// Quieter than this, a player isn't making any sound worth the name.
const SOUND_FLOOR: f32 = 1.0 / 16384.0;
/// Sound played this recently counts as still playing.
const SOUNDING_FOR: Duration = Duration::from_secs(1);
/// When real sound last came out of the output.
static SOUNDED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Applies the receive volume to `samples` and notes how loud they are.
fn finish_received(samples: &mut [f32]) {
    let volume = AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed);
    AUDIO_RX_PEAK_PCT.store(peak_pct(samples), Ordering::Relaxed);
    if volume != 0 && samples.iter().any(|sample| sample.abs() >= SOUND_FLOOR) {
        *SOUNDED_AT.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }
    if volume != 100 {
        let gain = volume as f32 / 100.0;
        samples.iter_mut().for_each(|sample| *sample *= gain);
    }
}

/// Runs received audio through the buffer with nothing to play it on, to keep the meter and the
/// counts honest while Play is off. Call about every `elapsed`.
fn drain_received(stream: &Mutex<RxStream>, elapsed: Duration, now: Instant) {
    let frames = (elapsed.as_secs_f64() * f64::from(RATE)) as usize;
    let mut samples = vec![0.0f32; frames * CHANNELS];
    stream.lock().unwrap_or_else(PoisonError::into_inner).fill(&mut samples, now);
    AUDIO_RX_PEAK_PCT.store(peak_pct(&samples), Ordering::Relaxed);
}

/// How long the test sound lasts, in ms.
const CHIME_MS: usize = 320;

fn chime_samples() -> Vec<f32> {
    let total_frames = RATE as usize * CHIME_MS / 1000;
    let mut pcm = Vec::with_capacity(total_frames * CHANNELS);
    for i in 0..total_frames {
        let t = i as f32 / RATE as f32;
        let freq = if i < total_frames / 2 { 659.25 } else { 987.77 };
        let env = (1.0 - (i as f32 / total_frames as f32)).powf(0.8);
        let s = (t * freq * std::f32::consts::TAU).sin() * 0.27 * env;
        pcm.extend_from_slice(&[s, s]);
    }
    pcm
}

/// Plays a short two-note chime on this device (Linux) and/or across the bus.
pub fn trigger_test_chime(play_local: bool, send_to_peer: bool) {
    std::thread::spawn(move || {
        let pcm = chime_samples();
        if play_local {
            platform::play_chime(&pcm);
        }
        if send_to_peer {
            for frame in pcm.chunks(OPUS_FRAME * CHANNELS) {
                HUB.chime.push(frame, &HUB.signal);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
}

/// Tells the audio engine to look at the choices now.
pub fn choices_changed() {
    platform::choices_changed();
}

/// Reads the stream's figures for the app, and returns what to tell the sender.
fn publish_received(stream: &mut RxStream, now: Instant) -> packet::Report {
    let alive = stream.alive(now);
    if !alive {
        stream.forget_quality();
    }
    let playing = AUDIO_PLAYER_LIVE.load(Ordering::Relaxed);
    let sounded_at = *SOUNDED_AT.lock().unwrap_or_else(PoisonError::into_inner);
    let mut report = stream.report(playing);
    report.sounding = playing && seen_within(sounded_at, now, SOUNDING_FOR);
    report.ready = !AUDIO_RX_MUTED.load(Ordering::Relaxed);
    AUDIO_RX_LIVE.store(alive, Ordering::Relaxed);
    AUDIO_RX_PACKETS.store(stream.counts().received, Ordering::Relaxed);
    AUDIO_RX_LOSS.store((stream.loss_percent() * 10.0) as u32, Ordering::Relaxed);
    AUDIO_RX_UNREPAIRED.store((stream.unrepaired_percent() * 10.0) as u32, Ordering::Relaxed);
    AUDIO_RX_BUFFER_MS.store(if alive { stream.buffered_ms() as u32 } else { 0 }, Ordering::Relaxed);
    report
}

/// When audio of another version last came from the other device. A plain packet sets this and
/// needs no key, so anyone can forge one: this is for a note in the app, and must do nothing else.
static OTHER_VERSION_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// How long the app keeps saying so after the last of it.
const OTHER_VERSION_FOR: Duration = Duration::from_secs(10);

fn seen_within(at: Option<Instant>, now: Instant, within: Duration) -> bool {
    at.is_some_and(|at| now.duration_since(at) < within)
}

fn note_other_version(now: Instant) {
    *OTHER_VERSION_AT.lock().unwrap_or_else(PoisonError::into_inner) = Some(now);
}

/// True while audio from a mesh-kvm of another version is arriving, and being ignored.
pub fn other_version_arriving(now: Instant) -> bool {
    seen_within(*OTHER_VERSION_AT.lock().unwrap_or_else(PoisonError::into_inner), now, OTHER_VERSION_FOR)
}

/// The listening end of the link.
struct Receiver<'a> {
    socket: &'a UdpSocket,
    stream: &'a Mutex<RxStream>,
    peer: SocketAddr,
    channel: &'a Channel,
    /// Where the sender is. Its port can be anything, so this comes from what arrives, and only
    /// from what checked out: anyone can forge a source address.
    last_source: Option<SocketAddr>,
    /// The sender's run, from its audio.
    epoch: Option<u16>,
    opened: Vec<u8>,
    report: Vec<u8>,
    sealed: Vec<u8>,
}

impl<'a> Receiver<'a> {
    fn new(socket: &'a UdpSocket, stream: &'a Mutex<RxStream>, peer: SocketAddr, channel: &'a Channel) -> Self {
        Self { socket, stream, peer, channel, last_source: None, epoch: None, opened: Vec::new(), report: Vec::with_capacity(32), sealed: Vec::new() }
    }

    /// Anyone can send to a UDP port, so what comes from elsewhere is dropped before it's looked at.
    fn take(&mut self, datagram: &[u8], from: SocketAddr, now_ms: u64, now: Instant) {
        if !crate::input::from_peer(from, self.peer) {
            return;
        }
        match self.channel.open(datagram, now_ms, &mut self.opened) {
            Opened::Data(plain) => {
                self.last_source = Some(from);
                match packet::parse(plain) {
                    Ok(packet) if !matches!(packet.body, Body::Report(_)) => {
                        self.epoch = Some(packet.epoch);
                        self.stream.lock().unwrap_or_else(PoisonError::into_inner).push(&packet, now);
                    }
                    Err(Refused::OtherVersion) => note_other_version(now),
                    _ => {}
                }
            }
            // A responder never has a handshake going, so here this is always a keepalive, and so
            // authentic. It says where the sender is and nothing about the stream.
            Opened::Heard => self.last_source = Some(from),
            Opened::Reply(reply) => {
                let _ = self.socket.send_to(reply, from);
            }
            Opened::OldPeer => note_other_version(now),
            Opened::Refused => {}
        }
    }

    /// Sends what the channel has due. With no source yet it has nothing: only data makes it speak.
    fn tick(&self, now_ms: u64) {
        self.channel.tick(now_ms, &mut |packet| {
            if let Some(to) = self.last_source {
                let _ = self.socket.send_to(packet, to);
            }
        });
    }

    /// Publishes the stream's figures and tells the sender how it's going.
    fn report(&mut self, now: Instant) {
        // The socket waits for no lock: playback takes this one on every buffer.
        let (report, alive) = {
            let mut stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
            (publish_received(&mut stream, now), stream.alive(now))
        };
        if let (Some(to), Some(epoch), true) = (self.last_source, self.epoch, alive) {
            packet::write_report(&mut self.report, packet::Stream::Sound, epoch, report);
            if self.channel.seal(&self.report, &mut self.sealed) {
                let _ = self.socket.send_to(&self.sealed, to);
            }
        }
    }
}

/// Takes the other device's packets off the socket until `stop`, and tells it how it's going.
fn receive_loop(socket: &UdpSocket, stream: &Mutex<RxStream>, peer: SocketAddr, channel: &Channel, stop: &AtomicBool) {
    let _ = socket.set_read_timeout(Some(RECEIVE_WAIT));
    let clock = Instant::now();
    let mut receiver = Receiver::new(socket, stream, peer, channel);
    let mut buf = [0u8; 2048];
    let mut next_report = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let received = socket.recv_from(&mut buf);
        let now = Instant::now();
        let now_ms = now.duration_since(clock).as_millis() as u64;
        if let Ok((len, from)) = received {
            receiver.take(&buf[..len], from, now_ms, now);
        }
        receiver.tick(now_ms);
        if now >= next_report {
            next_report = now + REPORT_EVERY;
            receiver.report(now);
        }
    }
}

pub fn start_audio_listener(port: u16, peer: SocketAddr, channel: Arc<Channel>) -> Result<(), String> {
    let stream = Arc::new(Mutex::new(RxStream::default()));
    platform::start_playback(Arc::clone(&stream));
    std::thread::Builder::new()
        .name("mesh-kvm-audio-rx".to_string())
        .spawn(move || {
            let bind_addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], port));
            let socket = match crate::app::launch::bind_patiently(|| UdpSocket::bind(bind_addr)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[Audio-RX] Failed to bind UDP {}: {}", bind_addr, e);
                    return;
                }
            };
            println!("[Audio-RX] Listening on UDP {}", bind_addr);
            receive_loop(&socket, &stream, peer, &channel, &AtomicBool::new(false));
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn start_audio_sender(target_addr: SocketAddr, channel: Arc<Channel>) -> Result<(), String> {
    platform::start_engine();
    std::thread::Builder::new()
        .name("mesh-kvm-audio-tx".to_string())
        .spawn(move || {
            let socket = match UdpSocket::bind("0.0.0.0:0") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[Audio-TX] Failed to bind UDP socket: {}", e);
                    return;
                }
            };
            println!("[Audio-TX] Ready -> UDP {}", target_addr);
            sender::send_loop(&socket, target_addr, &channel, &HUB, &AtomicBool::new(false));
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::datagram::{Label, LinkSecurity, Role};
    use crate::secure::keys::{Pairing, PeerKey};

    static FIGURES: Mutex<()> = Mutex::new(());

    /// For a test that looks at the note about another version or the bytes sent, or that changes
    /// them, as there is one of each.
    pub(super) fn figures_to_myself() -> std::sync::MutexGuard<'static, ()> {
        FIGURES.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A sender and a receiver that know each other, with no session yet.
    pub(super) fn paired_channels() -> (Channel, Channel) {
        let (mut a, mut b) = (Pairing::fresh(), Pairing::fresh());
        a.peer = Some(PeerKey { public: b.identity.public, name: "B".to_string(), paired_at: 1 });
        b.peer = Some(PeerKey { public: a.identity.public, name: "A".to_string(), paired_at: 1 });
        (Channel::new(&a, Label::Audio, Role::Initiator), Channel::new(&b, Label::Audio, Role::Responder))
    }

    /// What a channel has to send at `now_ms`.
    pub(super) fn due(channel: &Channel, now_ms: u64) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        channel.tick(now_ms, &mut |packet| packets.push(packet.to_vec()));
        packets
    }

    pub(super) fn sealed(channel: &Channel, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(channel.seal(plain, &mut out));
        out
    }

    /// A sender and a receiver with a session, each holding it as current.
    pub(super) fn linked_channels() -> (Channel, Channel) {
        let (sender, receiver) = paired_channels();
        let hello = due(&sender, 0).remove(0);
        let (mut answer, mut keepalive) = (Vec::new(), Vec::new());
        assert!(matches!(receiver.open(&hello, 0, &mut answer), Opened::Reply(_)));
        assert!(matches!(sender.open(&answer, 0, &mut keepalive), Opened::Reply(_)));
        assert_eq!(receiver.open(&keepalive, 0, &mut Vec::new()), Opened::Heard);
        (sender, receiver)
    }

    /// Runs the receive loop for a moment on a socket of its own, with `127.0.0.1` as the peer.
    fn received_from(source: &str, channel: &Channel, data: &[u8]) -> (u64, Option<Instant>) {
        let listen = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = listen.local_addr().unwrap();
        let stream = Mutex::new(RxStream::default());
        let stop = AtomicBool::new(false);
        *OTHER_VERSION_AT.lock().unwrap() = None;
        std::thread::scope(|scope| {
            scope.spawn(|| receive_loop(&listen, &stream, SocketAddr::from(([127, 0, 0, 1], 24851)), channel, &stop));
            let sender = UdpSocket::bind((source, 0)).unwrap();
            sender.send_to(data, to).unwrap();
            std::thread::sleep(Duration::from_millis(30));
            stop.store(true, Ordering::SeqCst);
        });
        let received = stream.lock().unwrap().counts().received;
        (received, *OTHER_VERSION_AT.lock().unwrap())
    }

    fn audio_packet() -> Vec<u8> {
        let mut out = Vec::new();
        packet::write_audio(&mut out, codec::Format::Opus, packet::Stream::Sound, 1, 0, &[0xfc, 0xff, 0xfe]);
        out
    }

    #[test]
    fn only_the_other_device_is_listened_to() {
        let _figures = figures_to_myself();
        let (sender, receiver) = linked_channels();
        assert_eq!(received_from("127.0.0.1", &receiver, &sealed(&sender, &audio_packet())).0, 1);
        assert_eq!(received_from("127.0.0.2", &receiver, &sealed(&sender, &audio_packet())).0, 0, "a stranger's audio");
        assert!(received_from("127.0.0.2", &receiver, &audio_packet()).1.is_none(), "a stranger can't raise the note");
    }

    #[test]
    fn a_plain_audio_packet_is_not_played_and_says_old_version() {
        let _figures = figures_to_myself();
        let (_, receiver) = linked_channels();
        let (received, note) = received_from("127.0.0.1", &receiver, &audio_packet());
        assert_eq!(received, 0);
        assert!(note.is_some());
    }

    #[test]
    fn a_session_packet_of_another_version_says_so_too() {
        let _figures = figures_to_myself();
        let (sender, receiver) = linked_channels();
        let mut newer = audio_packet();
        newer[4] = packet::VERSION + 1;
        let (received, note) = received_from("127.0.0.1", &receiver, &sealed(&sender, &newer));
        assert_eq!(received, 0);
        assert!(note.is_some());
    }

    #[test]
    fn audio_with_no_session_is_not_played() {
        let _figures = figures_to_myself();
        let (_, stranger) = linked_channels();
        let (other, _) = linked_channels();
        assert_eq!(received_from("127.0.0.1", &stranger, &sealed(&other, &audio_packet())).0, 0);
    }

    fn bound() -> UdpSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        socket
    }

    fn next(socket: &UdpSocket) -> Option<Vec<u8>> {
        let mut buf = [0u8; 2048];
        socket.recv_from(&mut buf).ok().map(|(len, _)| buf[..len].to_vec())
    }

    /// The receive side, and two addresses on the peer's IP for packets to claim to come from.
    struct Far {
        socket: UdpSocket,
        stream: Mutex<RxStream>,
        one: UdpSocket,
        other: UdpSocket,
    }

    impl Far {
        fn new() -> Far {
            Far { socket: bound(), stream: Mutex::default(), one: bound(), other: bound() }
        }

        fn receiver<'a>(&'a self, channel: &'a Channel) -> Receiver<'a> {
            Receiver::new(&self.socket, &self.stream, SocketAddr::from(([127, 0, 0, 1], 24851)), channel)
        }
    }

    fn from(socket: &UdpSocket) -> SocketAddr {
        socket.local_addr().unwrap()
    }

    #[test]
    fn a_keepalive_is_not_activity() {
        let far = Far::new();
        let (sender, channel) = linked_channels();
        let mut receiver = far.receiver(&channel);
        let keepalive = due(&sender, 1).remove(0);
        let now = Instant::now();
        receiver.take(&keepalive, from(&far.one), 1, now);
        assert_eq!(far.stream.lock().unwrap().counts().received, 0);
        assert!(!far.stream.lock().unwrap().alive(now));
        receiver.report(now);
        assert_eq!(next(&far.one), None, "nothing arrived to report on");
    }

    #[test]
    fn a_handshake_is_answered_at_its_own_source_and_moves_nothing() {
        let _figures = figures_to_myself();
        let far = Far::new();
        let (sender, channel) = paired_channels();
        let mut receiver = far.receiver(&channel);
        let hello = due(&sender, 0).remove(0);
        let now = Instant::now();

        receiver.take(&hello, from(&far.one), 0, now);
        let answer = next(&far.one).expect("answered where it came from");
        assert_eq!(receiver.last_source, None, "a handshake 1 can be a replay");

        // A session starts on the keepalive, which is the first thing to prove the sender is there.
        let mut keepalive = Vec::new();
        assert!(matches!(sender.open(&answer, 0, &mut keepalive), Opened::Reply(_)));
        receiver.take(&keepalive, from(&far.one), 1, now);
        assert_eq!(receiver.last_source, Some(from(&far.one)));

        receiver.take(&sealed(&sender, &audio_packet()), from(&far.one), 2, now);

        // A replay and junk from the peer's IP, on another port.
        receiver.take(&hello, from(&far.other), 1500, now);
        assert!(next(&far.other).is_some(), "the replay gets its answer where it says it's from");
        receiver.take(&[0xE3; 40], from(&far.other), 1500, now);
        receiver.take(&audio_packet(), from(&far.other), 1500, now);
        assert_eq!(receiver.last_source, Some(from(&far.one)));

        receiver.tick(2000);
        assert!(next(&far.one).is_some(), "rule 10's keepalive goes to the last real source");
        assert_eq!(next(&far.other), None);

        receiver.take(&sealed(&sender, &audio_packet()), from(&far.other), 2100, now);
        assert_eq!(receiver.last_source, Some(from(&far.other)), "a sender that moved is followed");
    }

    #[test]
    fn a_report_goes_back_sealed() {
        let far = Far::new();
        let (sender, channel) = linked_channels();
        let mut receiver = far.receiver(&channel);
        let now = Instant::now();
        receiver.take(&sealed(&sender, &audio_packet()), from(&far.one), 1, now);
        receiver.report(now);
        let report = next(&far.one).expect("a stream that's alive gets a report");
        assert!(packet::parse(&report).is_err(), "nothing readable on the wire");
        let mut plain = Vec::new();
        let Opened::Data(bytes) = sender.open(&report, 2, &mut plain) else { panic!("not from the session") };
        assert!(matches!(packet::parse(bytes), Ok(packet::Packet { body: Body::Report(_), .. })));
    }

    #[test]
    fn the_session_is_ready_with_nothing_being_sent() {
        let _figures = figures_to_myself();
        let (out, channel) = paired_channels();
        let (listen, socket) = (bound(), bound());
        let target = listen.local_addr().unwrap();
        let (stream, hub, stop) = (Mutex::default(), Hub::default(), AtomicBool::new(false));
        std::thread::scope(|scope| {
            scope.spawn(|| receive_loop(&listen, &stream, target, &channel, &stop));
            scope.spawn(|| sender::send_loop(&socket, target, &out, &hub, &stop));
            let end = Instant::now() + Duration::from_secs(3);
            let ready = || [&out, &channel].iter().all(|c| c.security() == LinkSecurity::Encrypted);
            while !ready() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, Ordering::SeqCst);
            assert!(ready());
        });
        assert_eq!(stream.lock().unwrap().counts().received, 0);
    }

    #[test]
    fn the_note_about_another_version_clears_once_it_stops_arriving() {
        let t = Instant::now();
        assert!(!seen_within(None, t, OTHER_VERSION_FOR));
        assert!(seen_within(Some(t), t + OTHER_VERSION_FOR - Duration::from_millis(1), OTHER_VERSION_FOR));
        assert!(!seen_within(Some(t), t + OTHER_VERSION_FOR, OTHER_VERSION_FOR));
    }

    #[test]
    fn a_stream_that_ended_leaves_no_figures_behind() {
        let mut stream = RxStream::default();
        let t = Instant::now();
        let data = audio_packet();
        stream.push(&packet::parse(&data).unwrap(), t);
        let mut late = audio_packet();
        late[9..13].copy_from_slice(&100u32.to_le_bytes());
        stream.report(true);
        stream.push(&packet::parse(&late).unwrap(), t);
        publish_received(&mut stream, t);
        assert!(stream.loss_percent() > 10.0);
        publish_received(&mut stream, t + Duration::from_secs(3));
        assert_eq!(stream.loss_percent(), 0.0);
    }
}
