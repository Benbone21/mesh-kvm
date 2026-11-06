#![allow(dead_code)]
//! Bidirectional audio bus (UDP 24851): 48 kHz stereo s16le PCM in 5 ms packets.
//! Linux uses PipeWire (`pw-record` / `pw-play`); Windows uses WASAPI via `cpal`
//! (loopback capture of the default output device + playback to the default output).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

pub const AUDIO_MAGIC: [u8; 4] = *b"MKVA";
// 5ms of 48kHz stereo 16-bit PCM = 240 frames * 2 channels * 2 bytes = 960 bytes per UDP packet
pub const PCM_CHUNK_BYTES: usize = 960;

pub static AUDIO_RX_MUTED: AtomicBool = AtomicBool::new(false);
pub static AUDIO_RX_VOLUME_PCT: AtomicU32 = AtomicU32::new(100);
pub static AUDIO_RX_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static AUDIO_RX_PEAK_PCT: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_RX_DROPPED: AtomicU64 = AtomicU64::new(0);

pub static AUDIO_TX_ENABLED: AtomicBool = AtomicBool::new(false);
pub static AUDIO_TX_VOLUME_PCT: AtomicU32 = AtomicU32::new(100);
pub static AUDIO_TX_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static AUDIO_TX_PEAK_PCT: AtomicU32 = AtomicU32::new(0);
/// 0 = Desktop System Audio (PipeWire Sink Monitor), 1 = Microphone Input (Linux only)
pub static AUDIO_TX_SOURCE_MODE: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_TEST_CHIME_REQ: AtomicBool = AtomicBool::new(false);
pub static AUDIO_MUTE_WHILE_SENDING: AtomicBool = AtomicBool::new(true);
/// Whether the link is up. Stored by the input loops, read by the mute thread.
pub static AUDIO_PEER_ONLINE: AtomicBool = AtomicBool::new(false);

/// `--raw` is required: without it pw-play rejects stdin as an unknown file format and exits,
/// and pw-record wraps the stream in a file header.
#[cfg(target_os = "linux")]
const PW_FORMAT_ARGS: [&str; 5] =
    ["--raw", "--rate=48000", "--channels=2", "--format=s16", "--latency=10ms"];
/// The most audio queued for pw-play, about 85 ms. Beyond this, packets are dropped so delay
/// can't grow. It is this big to ride out Wi-Fi bursts: `PLAYER_TARGET_BYTES` keeps the usual delay.
#[cfg(target_os = "linux")]
const PLAYER_BACKLOG_BYTES: usize = 16384;
/// The queue pw-play settles back to, about 30 ms.
#[cfg(target_os = "linux")]
const PLAYER_TARGET_BYTES: usize = 5760;
/// How long the queue must stay above the target before one packet is dropped.
#[cfg(target_os = "linux")]
const PLAYER_TRIM_AFTER: std::time::Duration = std::time::Duration::from_secs(1);
/// How long a new player has to reach the target, with everything above it discarded. pw-play
/// takes a moment to start reading, and one packet a second would take 10 s to undo that queue.
#[cfg(target_os = "linux")]
const PLAYER_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);
/// A player that won't stay up isn't restarted more often than this.
#[cfg(target_os = "linux")]
const PLAYER_RESPAWN_GAP: std::time::Duration = std::time::Duration::from_secs(1);
/// pw-play blocks in the audio graph while it waits for input, so it only runs during a stream.
#[cfg(target_os = "linux")]
const PLAYER_IDLE_CLOSE: std::time::Duration = std::time::Duration::from_secs(2);

/// Peer audio endpoint (set when the sender starts) so test chimes can be routed across the bus.
static AUDIO_PEER: OnceLock<SocketAddr> = OnceLock::new();

fn peak_pct_s16(pcm: &[u8]) -> u32 {
    let mut max_abs: i32 = 0;
    let n = pcm.len() & !1;
    for i in (0..n).step_by(2) {
        let s = (i16::from_le_bytes([pcm[i], pcm[i + 1]]) as i32).abs();
        if s > max_abs {
            max_abs = s;
        }
    }
    ((max_abs * 100) / 32767).clamp(0, 100) as u32
}

/// Applies a percentage gain in-place to s16le PCM.
fn apply_gain_s16(pcm: &mut [u8], pct: u32) {
    if pct == 100 {
        return;
    }
    let n = pcm.len() & !1;
    for i in (0..n).step_by(2) {
        let s = i16::from_le_bytes([pcm[i], pcm[i + 1]]) as i32;
        let adj = ((s * pct as i32) / 100).clamp(-32768, 32767) as i16;
        let b = adj.to_le_bytes();
        pcm[i] = b[0];
        pcm[i + 1] = b[1];
    }
}

/// Generates a short two-note chime and plays it locally (Linux) and/or sends it across the bus.
pub fn trigger_test_chime(play_local: bool, send_to_peer: bool) {
    std::thread::spawn(move || {
        let total_frames = 48000 * 320 / 1000;
        let mut pcm = Vec::with_capacity(total_frames * 4);
        for i in 0..total_frames {
            let t = i as f32 / 48000.0;
            let freq = if i < total_frames / 2 { 659.25 } else { 987.77 };
            let env = (1.0 - (i as f32 / total_frames as f32)).powf(0.8);
            let s = ((t * freq * std::f32::consts::TAU).sin() * 9000.0 * env) as i16;
            let b = s.to_le_bytes();
            pcm.extend_from_slice(&b);
            pcm.extend_from_slice(&b);
        }

        #[cfg(target_os = "linux")]
        if play_local {
            use std::io::Write;
            use std::process::{Command, Stdio};
            if let Ok(mut child) = Command::new("pw-play")
                .args(PW_FORMAT_ARGS)
                .arg("-")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&pcm);
                }
                let _ = child.wait();
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = play_local;

        if send_to_peer {
            if let (Some(peer), Ok(sock)) = (AUDIO_PEER.get(), UdpSocket::bind("0.0.0.0:0")) {
                // Use a high sequence base so the receiver's reorder filter always accepts it.
                let mut seq = u64::MAX / 2
                    + (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64);
                for chunk in pcm.chunks(PCM_CHUNK_BYTES) {
                    if chunk.len() != PCM_CHUNK_BYTES {
                        continue;
                    }
                    let mut pkt = [0u8; 12 + PCM_CHUNK_BYTES];
                    pkt[0..4].copy_from_slice(&AUDIO_MAGIC);
                    pkt[4..12].copy_from_slice(&seq.to_le_bytes());
                    pkt[12..].copy_from_slice(chunk);
                    seq += 1;
                    let _ = sock.send_to(&pkt, peer);
                    AUDIO_TX_PACKETS.fetch_add(1, Ordering::Relaxed);
                    AUDIO_TX_PEAK_PCT.store(peak_pct_s16(chunk), Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                AUDIO_TX_PEAK_PCT.store(0, Ordering::Relaxed);
            }
        }
    });
}

/// Returns true if a packet sequence should be accepted (monotonic, or a sender restart / jump).
#[inline]
fn accept_seq(seq: u64, last: &mut u64) -> bool {
    if seq > *last || last.abs_diff(seq) > 500 {
        *last = seq;
        true
    } else {
        false
    }
}

// ============================================================================================
// Linux (PipeWire)
// ============================================================================================

/// Decides when to drop a packet to bring the player's queue back down. Input and output run at
/// the same rate, so without this a queue that grew in a burst would never shrink.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct BacklogPolicy {
    started: Option<std::time::Instant>,
    above_since: Option<std::time::Instant>,
}

#[cfg(target_os = "linux")]
impl BacklogPolicy {
    /// True when the incoming packet should be dropped, given the bytes queued now.
    fn should_drop(&mut self, queued: usize, now: std::time::Instant) -> bool {
        // The kernel may round the pipe up or refuse to size it, so its own limit isn't relied on.
        let over_limit = queued + PCM_CHUNK_BYTES > PLAYER_BACKLOG_BYTES;
        let started = *self.started.get_or_insert(now);
        if queued <= PLAYER_TARGET_BYTES {
            self.above_since = None;
            return false;
        }
        if now.duration_since(started) < PLAYER_SETTLE {
            return true;
        }
        let since = *self.above_since.get_or_insert(now);
        let trim = now.duration_since(since) >= PLAYER_TRIM_AFTER;
        if trim {
            self.above_since = Some(now);
        }
        trim || over_limit
    }
}

/// Feeds PCM to a player process through a small non-blocking pipe.
#[cfg(target_os = "linux")]
struct PcmPlayer<F: Fn() -> std::process::Command> {
    command: F,
    child: Option<std::process::Child>,
    last_write: Option<std::time::Instant>,
    last_spawn: Option<std::time::Instant>,
    backlog: BacklogPolicy,
}

#[cfg(target_os = "linux")]
impl<F: Fn() -> std::process::Command> PcmPlayer<F> {
    fn new(command: F) -> Self {
        Self {
            command,
            child: None,
            last_write: None,
            last_spawn: None,
            backlog: BacklogPolicy::default(),
        }
    }

    /// The bytes waiting in the pipe.
    fn queued(&self) -> Option<usize> {
        use std::os::fd::AsRawFd;
        let fd = self.child.as_ref()?.stdin.as_ref()?.as_raw_fd();
        let mut bytes: libc::c_int = 0;
        let read = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut bytes) };
        (read == 0).then_some(bytes as usize)
    }

    fn spawn(&mut self) {
        use std::os::fd::AsRawFd;
        use std::process::Stdio;
        self.child = (self.command)()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        if let Some(stdin) = self.child.as_ref().and_then(|c| c.stdin.as_ref()) {
            let fd = stdin.as_raw_fd();
            unsafe {
                libc::fcntl(fd, libc::F_SETPIPE_SZ, PLAYER_BACKLOG_BYTES as libc::c_int);
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    fn close(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.backlog = BacklogPolicy::default();
    }

    /// Returns true if the packet was queued. A full pipe or the backlog policy drops it; a dead
    /// player is replaced.
    fn write(&mut self, pcm: &[u8], now: std::time::Instant) -> bool {
        use std::io::{ErrorKind, Write};
        self.last_write = Some(now);
        if self.child.is_none() {
            if self.last_spawn.is_some_and(|at| now.duration_since(at) < PLAYER_RESPAWN_GAP) {
                return false;
            }
            self.last_spawn = Some(now);
            self.spawn();
        }
        if self.backlog.should_drop(self.queued().unwrap_or(0), now) {
            return false;
        }
        let Some(stdin) = self.child.as_mut().and_then(|c| c.stdin.as_mut()) else {
            self.close();
            return false;
        };
        match stdin.write(pcm) {
            Ok(written) => written == pcm.len(),
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(_) => {
                self.close();
                false
            }
        }
    }

    fn close_if_idle(&mut self, now: std::time::Instant) {
        let idle = self
            .last_write
            .is_some_and(|last| now.duration_since(last) >= PLAYER_IDLE_CLOSE);
        if idle {
            self.close();
        }
    }
}

/// One turn of the listener. The idle check comes first, as packets can keep arriving with Play
/// off, and an idle pw-play holds up the audio graph.
#[cfg(target_os = "linux")]
fn receive_packet<F: Fn() -> std::process::Command>(
    socket: &UdpSocket,
    buf: &mut [u8; 2048],
    last_seq: &mut u64,
    player: &mut PcmPlayer<F>,
) {
    use std::time::Instant;
    player.close_if_idle(Instant::now());
    match socket.recv_from(buf) {
        Ok((len, _)) if len > 12 && buf[0..4] == AUDIO_MAGIC => {
            let seq = u64::from_le_bytes(buf[4..12].try_into().unwrap_or([0; 8]));
            if !accept_seq(seq, last_seq) {
                return;
            }
            AUDIO_RX_PACKETS.fetch_add(1, Ordering::Relaxed);
            let pcm = &mut buf[12..len];
            AUDIO_RX_PEAK_PCT.store(peak_pct_s16(pcm), Ordering::Relaxed);
            if AUDIO_RX_MUTED.load(Ordering::Relaxed) {
                return;
            }
            apply_gain_s16(pcm, AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed));
            if !player.write(pcm, Instant::now()) {
                AUDIO_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(_) => {}
        Err(_) => {
            let prev = AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed);
            AUDIO_RX_PEAK_PCT.store(prev / 2, Ordering::Relaxed);
        }
    }
}

#[cfg(target_os = "linux")]
pub fn start_audio_listener(port: u16) -> Result<(), String> {
    use std::process::Command;

    std::thread::Builder::new()
        .name("mesh-kvm-audio-rx".to_string())
        .spawn(move || {
            let bind_addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
            let socket = match UdpSocket::bind(bind_addr) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[Audio-RX] Failed to bind UDP {}: {}", bind_addr, e);
                    return;
                }
            };
            let _ = socket.set_read_timeout(Some(std::time::Duration::from_millis(120)));
            println!("[Audio-RX] Listening on UDP {} -> PipeWire 48kHz stereo", bind_addr);

            let mut player = PcmPlayer::new(|| {
                let mut command = Command::new("pw-play");
                command.args(PW_FORMAT_ARGS).arg("-");
                command
            });
            let mut buf = [0u8; 2048];
            let mut last_seq = 0u64;

            loop {
                receive_packet(&socket, &mut buf, &mut last_seq, &mut player);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn start_audio_sender(target_addr: SocketAddr) -> Result<(), String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let _ = AUDIO_PEER.set(target_addr);

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
            println!("[Audio-TX] PipeWire capture ready -> UDP {}", target_addr);

            let mut seq = 1u64;
            loop {
                // Only run pw-record while sharing is enabled (no idle capture stream in PipeWire).
                if !AUDIO_TX_ENABLED.load(Ordering::Relaxed) {
                    AUDIO_TX_PEAK_PCT.store(0, Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    continue;
                }
                let current_mode = AUDIO_TX_SOURCE_MODE.load(Ordering::Relaxed);
                let mut cmd = Command::new("pw-record");
                cmd.args(PW_FORMAT_ARGS);
                if current_mode == 0 {
                    // Capture desktop system audio (default sink monitor) rather than the microphone
                    cmd.args(["-P", "stream.capture.sink=true"]);
                }
                cmd.arg("-").stdout(Stdio::piped()).stderr(Stdio::null());

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[Audio-TX] Failed to start pw-record: {}", e);
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        continue;
                    }
                };
                let mut stdout = match child.stdout.take() {
                    Some(s) => s,
                    None => continue,
                };

                let mut pkt = [0u8; 12 + PCM_CHUNK_BYTES];
                pkt[0..4].copy_from_slice(&AUDIO_MAGIC);

                while stdout.read_exact(&mut pkt[12..]).is_ok() {
                    if !AUDIO_TX_ENABLED.load(Ordering::Relaxed)
                        || AUDIO_TX_SOURCE_MODE.load(Ordering::Relaxed) != current_mode
                    {
                        break;
                    }
                    apply_gain_s16(&mut pkt[12..], AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed));
                    AUDIO_TX_PEAK_PCT.store(peak_pct_s16(&pkt[12..]), Ordering::Relaxed);
                    pkt[4..12].copy_from_slice(&seq.to_le_bytes());
                    seq = seq.wrapping_add(1);
                    let _ = socket.send_to(&pkt, target_addr);
                    AUDIO_TX_PACKETS.fetch_add(1, Ordering::Relaxed);
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ============================================================================================
// Windows (WASAPI via cpal)
// ============================================================================================

#[cfg(target_os = "windows")]
pub fn start_audio_sender(target_addr: SocketAddr) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::{Arc, Mutex};

    let _ = AUDIO_PEER.set(target_addr);

    std::thread::Builder::new()
        .name("mesh-kvm-audio-tx".to_string())
        .spawn(move || {
            let socket = match UdpSocket::bind("0.0.0.0:0") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[Audio-TX] UDP bind error: {}", e);
                    return;
                }
            };

            loop {
                // Wait until sharing is enabled before opening a WASAPI loopback stream.
                while !AUDIO_TX_ENABLED.load(Ordering::Relaxed) {
                    AUDIO_TX_PEAK_PCT.store(0, Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }

                let host = cpal::default_host();
                // On WASAPI, building an input stream on the default *output* device = loopback capture.
                let device = match host.default_output_device() {
                    Some(d) => d,
                    None => {
                        eprintln!("[Audio-TX] No WASAPI output device for loopback capture");
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        continue;
                    }
                };
                let supported = match device.default_output_config() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[Audio-TX] WASAPI config error: {}", e);
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        continue;
                    }
                };
                let channels = supported.channels() as usize;
                let sample_rate = supported.sample_rate().0;
                let sample_format = supported.sample_format();
                let config: cpal::StreamConfig = supported.into();
                println!(
                    "[Audio-TX] WASAPI loopback {} Hz, {} ch, {:?} -> UDP {}",
                    sample_rate, channels, sample_format, target_addr
                );

                let sock = match socket.try_clone() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                // Resample (nearest-neighbour) from device rate to 48 kHz and packetize.
                let state: Arc<Mutex<(Vec<u8>, u64, f64)>> =
                    Arc::new(Mutex::new((Vec::with_capacity(PCM_CHUNK_BYTES * 4), 1, 0.0)));
                let st = Arc::clone(&state);
                let ratio = 48000.0 / sample_rate.max(1) as f64;
                let push = move |frames: &mut dyn Iterator<Item = (f32, f32)>| {
                    if !AUDIO_TX_ENABLED.load(Ordering::Relaxed) {
                        return;
                    }
                    let gain = AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed) as f32 / 100.0;
                    if let Ok(mut g) = st.lock() {
                        for (l, r) in frames {
                            g.2 += ratio;
                            while g.2 >= 1.0 {
                                g.2 -= 1.0;
                                let li = ((l * gain).clamp(-1.0, 1.0) * 32767.0) as i16;
                                let ri = ((r * gain).clamp(-1.0, 1.0) * 32767.0) as i16;
                                g.0.extend_from_slice(&li.to_le_bytes());
                                g.0.extend_from_slice(&ri.to_le_bytes());
                            }
                        }
                        while g.0.len() >= PCM_CHUNK_BYTES {
                            let mut pkt = [0u8; 12 + PCM_CHUNK_BYTES];
                            pkt[0..4].copy_from_slice(&AUDIO_MAGIC);
                            pkt[4..12].copy_from_slice(&g.1.to_le_bytes());
                            g.1 = g.1.wrapping_add(1);
                            pkt[12..].copy_from_slice(&g.0[..PCM_CHUNK_BYTES]);
                            g.0.drain(..PCM_CHUNK_BYTES);
                            AUDIO_TX_PEAK_PCT.store(peak_pct_s16(&pkt[12..]), Ordering::Relaxed);
                            let _ = sock.send_to(&pkt, target_addr);
                            AUDIO_TX_PACKETS.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                };
                let push = Arc::new(Mutex::new(push));
                let err_fn = |e| eprintln!("[Audio-TX] WASAPI stream error: {}", e);
                let ch = channels.max(1);

                let stream = match sample_format {
                    cpal::SampleFormat::F32 => {
                        let p = Arc::clone(&push);
                        device.build_input_stream(
                            &config,
                            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                                let mut it = data
                                    .chunks(ch)
                                    .map(|f| (f[0], if f.len() > 1 { f[1] } else { f[0] }));
                                if let Ok(mut f) = p.lock() {
                                    (&mut *f)(&mut it);
                                }
                            },
                            err_fn,
                            None,
                        )
                    }
                    cpal::SampleFormat::I16 => {
                        let p = Arc::clone(&push);
                        device.build_input_stream(
                            &config,
                            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                                let mut it = data.chunks(ch).map(|f| {
                                    let l = f[0] as f32 / 32768.0;
                                    let r = if f.len() > 1 { f[1] as f32 / 32768.0 } else { l };
                                    (l, r)
                                });
                                if let Ok(mut f) = p.lock() {
                                    (&mut *f)(&mut it);
                                }
                            },
                            err_fn,
                            None,
                        )
                    }
                    other => {
                        eprintln!("[Audio-TX] Unsupported WASAPI sample format: {:?}", other);
                        std::thread::sleep(std::time::Duration::from_secs(5));
                        continue;
                    }
                };

                match stream {
                    Ok(s) => {
                        if s.play().is_ok() {
                            while AUDIO_TX_ENABLED.load(Ordering::Relaxed) {
                                std::thread::sleep(std::time::Duration::from_millis(150));
                            }
                        }
                        drop(s);
                    }
                    Err(e) => {
                        eprintln!("[Audio-TX] Failed to build WASAPI loopback stream: {}", e);
                        std::thread::sleep(std::time::Duration::from_secs(3));
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn start_audio_listener(port: u16) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    std::thread::Builder::new()
        .name("mesh-kvm-audio-rx".to_string())
        .spawn(move || {
            let bind_addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
            let socket = match UdpSocket::bind(bind_addr) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[Audio-RX] Failed to bind UDP {}: {}", bind_addr, e);
                    return;
                }
            };
            let _ = socket.set_read_timeout(Some(std::time::Duration::from_millis(120)));

            let host = cpal::default_host();
            let device = match host.default_output_device() {
                Some(d) => d,
                None => return,
            };
            let supported = match device.default_output_config() {
                Ok(c) => c,
                Err(_) => return,
            };
            let channels = supported.channels() as usize;
            let out_rate = supported.sample_rate().0;
            let config: cpal::StreamConfig = supported.into();

            // Jitter buffer of interleaved stereo f32 frames at the *device* rate.
            let jitter: Arc<Mutex<VecDeque<(f32, f32)>>> =
                Arc::new(Mutex::new(VecDeque::with_capacity(9600)));
            let jb_writer = Arc::clone(&jitter);
            let step = 48000.0 / out_rate.max(1) as f64;

            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let mut last_seq = 0u64;
                let mut phase = 0.0f64;
                loop {
                    match socket.recv_from(&mut buf) {
                        Ok((len, _)) if len > 12 && buf[0..4] == AUDIO_MAGIC => {
                            let seq = u64::from_le_bytes(buf[4..12].try_into().unwrap_or([0; 8]));
                            if !accept_seq(seq, &mut last_seq) {
                                continue;
                            }
                            AUDIO_RX_PACKETS.fetch_add(1, Ordering::Relaxed);
                            let pcm = &buf[12..len];
                            AUDIO_RX_PEAK_PCT.store(peak_pct_s16(pcm), Ordering::Relaxed);
                            if AUDIO_RX_MUTED.load(Ordering::Relaxed) {
                                continue;
                            }
                            let gain = AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed) as f32 / 100.0;
                            let frames: Vec<(f32, f32)> = pcm
                                .chunks_exact(4)
                                .map(|c| {
                                    (
                                        i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0 * gain,
                                        i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0 * gain,
                                    )
                                })
                                .collect();
                            if let Ok(mut q) = jb_writer.lock() {
                                // Keep latency bounded (~50 ms max)
                                let cap = (out_rate as usize / 20).max(960);
                                if q.len() > cap {
                                    let drop_n = q.len() - cap / 2;
                                    q.drain(..drop_n);
                                    let packets = drop_n as f64 * step / frames.len().max(1) as f64;
                                    AUDIO_RX_DROPPED.fetch_add(packets.ceil() as u64, Ordering::Relaxed);
                                }
                                // Resample 48k -> device rate (nearest neighbour)
                                let mut idx = phase;
                                while (idx as usize) < frames.len() {
                                    q.push_back(frames[idx as usize]);
                                    idx += step;
                                }
                                phase = idx - frames.len() as f64;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            let prev = AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed);
                            AUDIO_RX_PEAK_PCT.store(prev / 2, Ordering::Relaxed);
                        }
                    }
                }
            });

            let ch = channels.max(1);
            if let Ok(stream) = device.build_output_stream(
                &config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if let Ok(mut q) = jitter.lock() {
                        for frame in out.chunks_mut(ch) {
                            let (l, r) = q.pop_front().unwrap_or((0.0, 0.0));
                            for (i, s) in frame.iter_mut().enumerate() {
                                *s = match i {
                                    0 => l,
                                    1 => r,
                                    _ => 0.0,
                                };
                            }
                        }
                    }
                },
                |err| eprintln!("[Audio-RX] WASAPI output error: {}", err),
                None,
            ) {
                let _ = stream.play();
                println!("[Audio-RX] Listening on UDP {} -> WASAPI output", bind_addr);
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn command(program: &str, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command.args(args);
        command
    }

    #[test]
    fn pipewire_tools_are_told_the_stream_is_raw() {
        assert!(PW_FORMAT_ARGS.contains(&"--raw"));
    }

    #[test]
    fn a_stalled_player_drops_audio_instead_of_building_delay() {
        let mut player = PcmPlayer::new(|| command("sleep", &["5"]));
        let started = Instant::now();
        player.write(&[0; PCM_CHUNK_BYTES], started);
        let settled = started + PLAYER_SETTLE;
        let accepted = 1 + (0..200).filter(|_| player.write(&[0; PCM_CHUNK_BYTES], settled)).count();
        assert!(accepted * PCM_CHUNK_BYTES <= PLAYER_BACKLOG_BYTES);
        assert!(accepted * PCM_CHUNK_BYTES > 8192, "room for a burst: {accepted}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn the_queue_level_is_read_from_the_pipe() {
        let mut player = PcmPlayer::new(|| command("sleep", &["5"]));
        let now = Instant::now();
        assert_eq!(player.queued(), None, "no player yet");
        for packets in 1..=3 {
            assert!(player.write(&[0; PCM_CHUNK_BYTES], now));
            assert_eq!(player.queued(), Some(packets * PCM_CHUNK_BYTES));
        }
        player.close();
    }

    #[test]
    fn a_queue_stuck_above_the_target_loses_one_packet_a_second() {
        let mut player = PcmPlayer::new(|| command("sleep", &["5"]));
        let start = Instant::now();
        let packet = [0; PCM_CHUNK_BYTES];
        // Up to the target nothing is dropped, however long it stays there.
        for _ in 0..PLAYER_TARGET_BYTES / PCM_CHUNK_BYTES {
            assert!(player.write(&packet, start));
        }
        assert!(player.write(&packet, start + PLAYER_TRIM_AFTER * 5));
        // The queue is above the target from here.
        assert!(player.write(&packet, start + PLAYER_TRIM_AFTER * 6));
        let later = start + PLAYER_TRIM_AFTER * 7;
        assert!(!player.write(&packet, later), "one packet goes");
        assert!(player.write(&packet, later + Duration::from_millis(5)), "and only one");
        player.close();
    }

    #[test]
    fn a_new_player_starts_with_a_new_timer() {
        let mut player = PcmPlayer::new(|| command("sleep", &["5"]));
        let start = Instant::now();
        for _ in 0..8 {
            player.write(&[0; PCM_CHUNK_BYTES], start);
        }
        player.write(&[0; PCM_CHUNK_BYTES], start + PLAYER_SETTLE);
        assert!(player.backlog.above_since.is_some());
        player.close();
        assert!(player.backlog.above_since.is_none());
        assert!(player.backlog.started.is_none());
    }

    #[test]
    fn a_new_player_discards_what_exceeds_the_target() {
        let mut player = PcmPlayer::new(|| command("sleep", &["5"]));
        let start = Instant::now();
        let accepted = (0..20).filter(|_| player.write(&[0; PCM_CHUNK_BYTES], start)).count();
        assert_eq!(accepted, PLAYER_TARGET_BYTES / PCM_CHUNK_BYTES + 1);
        player.close();
    }

    #[test]
    fn a_player_that_keeps_failing_starts_once_a_second_at_most() {
        let spawns = std::cell::Cell::new(0);
        let mut player = PcmPlayer::new(|| {
            spawns.set(spawns.get() + 1);
            command("/nonexistent/pw-play", &[])
        });
        let start = Instant::now();
        for ms in (0..1000).step_by(5) {
            assert!(!player.write(&[0; PCM_CHUNK_BYTES], start + Duration::from_millis(ms)));
        }
        assert_eq!(spawns.get(), 1);
        player.write(&[0; PCM_CHUNK_BYTES], start + PLAYER_RESPAWN_GAP);
        assert_eq!(spawns.get(), 2);
    }

    #[test]
    fn play_off_closes_the_player_while_packets_keep_arriving() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut player = PcmPlayer::new(|| command("sleep", &["5"]));
        let long_ago = Instant::now() - PLAYER_IDLE_CLOSE;
        player.write(&[0; PCM_CHUNK_BYTES], long_ago);
        assert!(player.child.is_some());

        let mut packet = [0u8; 12 + PCM_CHUNK_BYTES];
        packet[0..4].copy_from_slice(&AUDIO_MAGIC);
        packet[4..12].copy_from_slice(&1u64.to_le_bytes());
        sender.send_to(&packet, socket.local_addr().unwrap()).unwrap();
        AUDIO_RX_MUTED.store(true, Ordering::SeqCst);
        let (mut buf, mut last_seq) = ([0u8; 2048], 0u64);
        receive_packet(&socket, &mut buf, &mut last_seq, &mut player);
        AUDIO_RX_MUTED.store(false, Ordering::SeqCst);
        assert_eq!(last_seq, 1, "the packet arrived");
        assert!(player.child.is_none());
    }

    /// A policy for a player that started long enough ago, and the time now.
    fn settled_policy() -> (BacklogPolicy, Instant) {
        let start = Instant::now();
        let mut policy = BacklogPolicy::default();
        policy.should_drop(0, start);
        (policy, start + PLAYER_SETTLE)
    }

    #[test]
    fn the_policy_discards_quickly_only_while_a_player_settles() {
        let (mut policy, t) = (BacklogPolicy::default(), Instant::now());
        let high = PLAYER_TARGET_BYTES + 1;
        assert!(!policy.should_drop(PLAYER_TARGET_BYTES, t));
        assert!(policy.should_drop(high, t));
        assert!(policy.should_drop(high, t + Duration::from_millis(5)));
        assert!(policy.should_drop(high, t + PLAYER_SETTLE - Duration::from_millis(1)));
        assert!(!policy.should_drop(high, t + PLAYER_SETTLE), "back to one a second");
    }

    #[test]
    fn the_policy_leaves_a_queue_at_or_below_the_target_alone() {
        let (mut policy, t) = settled_policy();
        for i in 0..1000 {
            assert!(!policy.should_drop(PLAYER_TARGET_BYTES, t + Duration::from_millis(5 * i)));
        }
    }

    #[test]
    fn the_policy_drops_after_a_second_above_the_target_and_restarts_the_timer() {
        let (mut policy, t) = settled_policy();
        let high = PLAYER_TARGET_BYTES + 1;
        assert!(!policy.should_drop(high, t));
        assert!(!policy.should_drop(high, t + PLAYER_TRIM_AFTER - Duration::from_millis(1)));
        assert!(policy.should_drop(high, t + PLAYER_TRIM_AFTER));
        assert!(!policy.should_drop(high, t + PLAYER_TRIM_AFTER + Duration::from_millis(5)));
        assert!(!policy.should_drop(high, t + PLAYER_TRIM_AFTER * 2 - Duration::from_millis(1)));
        assert!(policy.should_drop(high, t + PLAYER_TRIM_AFTER * 2));
    }

    #[test]
    fn the_policy_needs_the_queue_above_the_target_the_whole_time() {
        let (mut policy, t) = settled_policy();
        let high = PLAYER_TARGET_BYTES + 1;
        policy.should_drop(high, t);
        assert!(!policy.should_drop(PLAYER_TARGET_BYTES, t + Duration::from_millis(900)));
        assert!(!policy.should_drop(high, t + PLAYER_TRIM_AFTER));
        assert!(!policy.should_drop(high, t + Duration::from_millis(1899)));
        assert!(policy.should_drop(high, t + PLAYER_TRIM_AFTER + PLAYER_TRIM_AFTER));
    }

    #[test]
    fn the_policy_holds_the_hard_limit_even_if_the_pipe_is_bigger() {
        // F_SETPIPE_SZ can round up or be refused, which leaves a bigger pipe.
        let (mut policy, t) = settled_policy();
        assert!(!policy.should_drop(PLAYER_BACKLOG_BYTES - PCM_CHUNK_BYTES, t));
        assert!(policy.should_drop(PLAYER_BACKLOG_BYTES - PCM_CHUNK_BYTES + 1, t));
    }

    #[test]
    fn a_dead_player_is_replaced_on_the_next_write() {
        let mut player = PcmPlayer::new(|| command("cat", &[]));
        let now = Instant::now();
        assert!(player.write(&[0; PCM_CHUNK_BYTES], now));
        let first = player.child.as_ref().map(|c| c.id());
        player.child.as_mut().unwrap().kill().unwrap();
        player.child.as_mut().unwrap().wait().unwrap();
        let later = now + PLAYER_RESPAWN_GAP;
        player.write(&[0; PCM_CHUNK_BYTES], later);
        player.write(&[0; PCM_CHUNK_BYTES], later);
        assert_ne!(player.child.as_ref().map(|c| c.id()), first);
    }

    #[test]
    fn an_idle_player_closes_and_returns_on_the_next_packet() {
        let mut player = PcmPlayer::new(|| command("cat", &[]));
        let start = Instant::now();
        player.write(&[0; PCM_CHUNK_BYTES], start);
        player.close_if_idle(start + PLAYER_IDLE_CLOSE - Duration::from_millis(1));
        assert!(player.child.is_some());
        player.close_if_idle(start + PLAYER_IDLE_CLOSE);
        assert!(player.child.is_none());
        assert!(player.write(&[0; PCM_CHUNK_BYTES], start + PLAYER_IDLE_CLOSE));
        assert!(player.child.is_some());
    }
}
