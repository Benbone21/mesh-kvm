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
/// Of those, the ones with real sound in them, not silence or word that the stream is alive.
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
/// A wait short enough to send reports on time when nothing is arriving.
const RECEIVE_WAIT: Duration = Duration::from_millis(50);

/// What the sender last heard back.
#[derive(Clone, Copy, Default)]
struct HeardBack {
    at: Option<Instant>,
    playing: bool,
    sounding: bool,
}

static HEARD_BACK: Mutex<HeardBack> = Mutex::new(HeardBack { at: None, playing: false, sounding: false });

fn note_heard_back(report: packet::Report, now: Instant) {
    *HEARD_BACK.lock().unwrap_or_else(PoisonError::into_inner) =
        HeardBack { at: Some(now), playing: report.playing, sounding: report.sounding };
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
    AUDIO_RX_LIVE.store(alive, Ordering::Relaxed);
    AUDIO_RX_PACKETS.store(stream.counts().received, Ordering::Relaxed);
    AUDIO_RX_LOSS.store((stream.loss_percent() * 10.0) as u32, Ordering::Relaxed);
    AUDIO_RX_UNREPAIRED.store((stream.unrepaired_percent() * 10.0) as u32, Ordering::Relaxed);
    AUDIO_RX_BUFFER_MS.store(if alive { stream.buffered_ms() as u32 } else { 0 }, Ordering::Relaxed);
    report
}

/// When audio of another version last came from the other device.
static OTHER_VERSION_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// How long the app keeps saying so after the last of it.
const OTHER_VERSION_FOR: Duration = Duration::from_secs(10);

fn seen_within(at: Option<Instant>, now: Instant, within: Duration) -> bool {
    at.is_some_and(|at| now.duration_since(at) < within)
}

/// True while audio from a mesh-kvm of another version is arriving, and being ignored.
pub fn other_version_arriving(now: Instant) -> bool {
    seen_within(*OTHER_VERSION_AT.lock().unwrap_or_else(PoisonError::into_inner), now, OTHER_VERSION_FOR)
}

/// Takes the other device's packets off the socket until `stop`, and tells it how it's going.
/// Anyone can send to a UDP port, so what comes from elsewhere is dropped before it's looked at.
fn receive_loop(socket: &UdpSocket, stream: &Mutex<RxStream>, peer: SocketAddr, stop: &AtomicBool) {
    let _ = socket.set_read_timeout(Some(RECEIVE_WAIT));
    let mut buf = [0u8; 2048];
    let mut reply_to = None;
    let mut next_report = Instant::now();
    let mut out = Vec::with_capacity(32);
    while !stop.load(Ordering::Relaxed) {
        let received = socket.recv_from(&mut buf);
        let now = Instant::now();
        if let Some((len, from)) = received.ok().filter(|(_, from)| crate::input::from_peer(*from, peer)) {
            match packet::parse(&buf[..len]) {
                Ok(packet) if !matches!(packet.body, Body::Report(_)) => {
                    reply_to = Some((from, packet.epoch));
                    stream.lock().unwrap_or_else(PoisonError::into_inner).push(&packet, now);
                }
                Err(Refused::OtherVersion) => {
                    *OTHER_VERSION_AT.lock().unwrap_or_else(PoisonError::into_inner) = Some(now);
                }
                _ => {}
            }
        }
        if now >= next_report {
            next_report = now + REPORT_EVERY;
            // The socket waits for no lock: playback takes this one on every buffer.
            let (report, alive) = {
                let mut stream = stream.lock().unwrap_or_else(PoisonError::into_inner);
                (publish_received(&mut stream, now), stream.alive(now))
            };
            if let Some((to, epoch)) = reply_to.filter(|_| alive) {
                packet::write_report(&mut out, epoch, report);
                let _ = socket.send_to(&out, to);
            }
        }
    }
}

pub fn start_audio_listener(port: u16, peer: SocketAddr) -> Result<(), String> {
    let stream = Arc::new(Mutex::new(RxStream::default()));
    platform::start_playback(Arc::clone(&stream));
    std::thread::Builder::new()
        .name("mesh-kvm-audio-rx".to_string())
        .spawn(move || {
            let bind_addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], port));
            let socket = match UdpSocket::bind(bind_addr) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[Audio-RX] Failed to bind UDP {}: {}", bind_addr, e);
                    return;
                }
            };
            println!("[Audio-RX] Listening on UDP {}", bind_addr);
            receive_loop(&socket, &stream, peer, &AtomicBool::new(false));
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn start_audio_sender(target_addr: SocketAddr) -> Result<(), String> {
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
            sender::send_loop(&socket, target_addr, &HUB, &AtomicBool::new(false));
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the receive loop for a moment on a socket of its own, with `127.0.0.1` as the peer.
    fn received_from(source: &str, data: &[u8]) -> (u64, Option<Instant>) {
        let listen = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = listen.local_addr().unwrap();
        let stream = Mutex::new(RxStream::default());
        let stop = AtomicBool::new(false);
        *OTHER_VERSION_AT.lock().unwrap() = None;
        std::thread::scope(|scope| {
            scope.spawn(|| receive_loop(&listen, &stream, SocketAddr::from(([127, 0, 0, 1], 24851)), &stop));
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
        packet::write_audio(&mut out, codec::Format::Opus, 1, 0, &[0xfc, 0xff, 0xfe]);
        out
    }

    #[test]
    fn only_the_other_device_is_listened_to() {
        // These share one test, as they share the note of another version.
        assert_eq!(received_from("127.0.0.1", &audio_packet()).0, 1);
        assert_eq!(received_from("127.0.0.2", &audio_packet()).0, 0, "a stranger's audio");
        let mut old = Vec::from(*b"MKVA");
        old.extend_from_slice(&[0; 20]);
        assert!(received_from("127.0.0.1", &old).1.is_some());
        assert!(received_from("127.0.0.2", &old).1.is_none(), "a stranger can't raise the note");
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
        late[8..12].copy_from_slice(&100u32.to_le_bytes());
        stream.report(true);
        stream.push(&packet::parse(&late).unwrap(), t);
        publish_received(&mut stream, t);
        assert!(stream.loss_percent() > 10.0);
        publish_received(&mut stream, t + Duration::from_secs(3));
        assert_eq!(stream.loss_percent(), 0.0);
    }
}
