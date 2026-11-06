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
/// Audio from a mesh-kvm of another version is arriving, and being ignored.
pub static AUDIO_RX_OTHER_VERSION: AtomicU64 = AtomicU64::new(0);
/// The output is open and not switched off, so what's received can be heard.
pub static AUDIO_PLAYER_LIVE: AtomicBool = AtomicBool::new(false);

pub static AUDIO_TX_VOLUME_PCT: AtomicU32 = AtomicU32::new(100);
pub static AUDIO_TX_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static AUDIO_TX_PEAK_PCT: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_BITRATE_KBPS: AtomicU32 = AtomicU32::new(DEFAULT_BITRATE);
/// What the other device says it is losing of what's sent, in percent.
pub static AUDIO_TX_LOSS: AtomicU32 = AtomicU32::new(0);
/// The capture of an output is running.
pub static AUDIO_OUTPUT_CAPTURE_LIVE: AtomicBool = AtomicBool::new(false);
pub static AUDIO_PLAY_ONLY_ON_OTHER: AtomicBool = AtomicBool::new(true);
/// Whether the link is up. Stored by the input loops, read by the mute thread.
pub static AUDIO_PEER_ONLINE: AtomicBool = AtomicBool::new(false);

/// How often the receiver tells the sender how it's doing.
const REPORT_EVERY: Duration = Duration::from_millis(250);
/// A wait short enough to send reports on time when nothing is arriving.
const RECEIVE_WAIT: Duration = Duration::from_millis(50);

/// What the sender last heard back, for deciding whether sound is really reaching the other device.
#[derive(Clone, Copy, Default)]
struct HeardBack {
    at: Option<Instant>,
    playing: bool,
}

static HEARD_BACK: Mutex<HeardBack> = Mutex::new(HeardBack { at: None, playing: false });

fn note_heard_back(report: packet::Report, now: Instant) {
    *HEARD_BACK.lock().unwrap_or_else(PoisonError::into_inner) = HeardBack { at: Some(now), playing: report.playing };
    AUDIO_TX_LOSS.store(u32::from(report.loss_percent), Ordering::Relaxed);
}

/// True while the other device confirms that what's sent is arriving and being played.
pub fn reaching_peer(now: Instant, within: Duration) -> bool {
    let heard = *HEARD_BACK.lock().unwrap_or_else(PoisonError::into_inner);
    heard.playing && heard.at.is_some_and(|at| now.duration_since(at) < within)
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

/// Applies the receive volume to `samples` and notes how loud they are.
fn finish_received(samples: &mut [f32]) {
    let volume = AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed);
    AUDIO_RX_PEAK_PCT.store(peak_pct(samples), Ordering::Relaxed);
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

fn publish_received(stream: &mut RxStream, reply: Option<(&UdpSocket, SocketAddr, u16)>) {
    let report = stream.report(AUDIO_PLAYER_LIVE.load(Ordering::Relaxed));
    AUDIO_RX_PACKETS.store(stream.counts().received, Ordering::Relaxed);
    AUDIO_RX_LOSS.store((stream.loss_percent() * 10.0) as u32, Ordering::Relaxed);
    AUDIO_RX_UNREPAIRED.store((stream.unrepaired_percent() * 10.0) as u32, Ordering::Relaxed);
    AUDIO_RX_BUFFER_MS.store(stream.buffered_ms() as u32, Ordering::Relaxed);
    if let Some((socket, to, epoch)) = reply {
        let mut out = Vec::with_capacity(32);
        packet::write_report(&mut out, epoch, report);
        let _ = socket.send_to(&out, to);
    }
}

/// Takes packets off the socket until `stop`, and tells the sender how it's going.
fn receive_loop(socket: &UdpSocket, stream: &Mutex<RxStream>, stop: &AtomicBool) {
    let _ = socket.set_read_timeout(Some(RECEIVE_WAIT));
    let mut buf = [0u8; 2048];
    let mut reply_to = None;
    let mut next_report = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let received = socket.recv_from(&mut buf);
        let now = Instant::now();
        if let Ok((len, from)) = received {
            match packet::parse(&buf[..len]) {
                Ok(packet) if !matches!(packet.body, Body::Report(_)) => {
                    reply_to = Some((from, packet.epoch));
                    stream.lock().unwrap_or_else(PoisonError::into_inner).push(&packet, now);
                }
                Err(Refused::OtherVersion) => {
                    AUDIO_RX_OTHER_VERSION.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        if now >= next_report {
            next_report = now + REPORT_EVERY;
            let mut stream = stream.lock().unwrap_or_else(PoisonError::into_inner);
            let target = reply_to.filter(|_| stream.alive(now)).map(|(to, epoch)| (socket, to, epoch));
            publish_received(&mut stream, target);
        }
    }
}

pub fn start_audio_listener(port: u16) -> Result<(), String> {
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
            receive_loop(&socket, &stream, &AtomicBool::new(false));
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
