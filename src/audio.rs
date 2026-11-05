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

pub static AUDIO_TX_ENABLED: AtomicBool = AtomicBool::new(false);
pub static AUDIO_TX_VOLUME_PCT: AtomicU32 = AtomicU32::new(100);
pub static AUDIO_TX_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static AUDIO_TX_PEAK_PCT: AtomicU32 = AtomicU32::new(0);
/// 0 = Desktop System Audio (PipeWire Sink Monitor), 1 = Microphone Input (Linux only)
pub static AUDIO_TX_SOURCE_MODE: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_TEST_CHIME_REQ: AtomicBool = AtomicBool::new(false);

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
                .args(["--rate=48000", "--channels=2", "--format=s16", "--latency=10ms", "-"])
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

#[cfg(target_os = "linux")]
pub fn start_audio_listener(port: u16) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

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

            let spawn_player = || {
                Command::new("pw-play")
                    .args(["--rate=48000", "--channels=2", "--format=s16", "--latency=10ms", "-"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()
            };

            let mut child = spawn_player();
            let mut buf = [0u8; 2048];
            let mut last_seq = 0u64;

            loop {
                match socket.recv_from(&mut buf) {
                    Ok((len, _)) if len > 12 && buf[0..4] == AUDIO_MAGIC => {
                        let seq = u64::from_le_bytes(buf[4..12].try_into().unwrap_or([0; 8]));
                        if !accept_seq(seq, &mut last_seq) {
                            continue;
                        }
                        AUDIO_RX_PACKETS.fetch_add(1, Ordering::Relaxed);
                        let pcm = &mut buf[12..len];
                        AUDIO_RX_PEAK_PCT.store(peak_pct_s16(pcm), Ordering::Relaxed);
                        if AUDIO_RX_MUTED.load(Ordering::Relaxed) {
                            continue;
                        }
                        apply_gain_s16(pcm, AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed));
                        let ok = child
                            .as_mut()
                            .and_then(|c| c.stdin.as_mut())
                            .map(|s| s.write_all(pcm).is_ok())
                            .unwrap_or(false);
                        if !ok {
                            if let Some(mut c) = child.take() {
                                let _ = c.kill();
                                let _ = c.wait();
                            }
                            child = spawn_player();
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let prev = AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed);
                        AUDIO_RX_PEAK_PCT.store(prev / 2, Ordering::Relaxed);
                    }
                }
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
                cmd.args(["--rate=48000", "--channels=2", "--format=s16", "--latency=10ms"]);
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
