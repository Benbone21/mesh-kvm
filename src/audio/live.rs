//! Tests of the real audio threads, run from `tests/audio_measure.py` and `tests/audio_devices.py`,
//! which set up what they need: `cargo test` skips them. The scripts run them with a private sound
//! server and network, as these would otherwise use the real ones.

use super::jitter::RxStream;
use super::*;
use std::io::Write;
use std::sync::atomic::AtomicBool;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn number(name: &str, default: u64) -> u64 {
    env(name).and_then(|value| value.parse().ok()).unwrap_or(default)
}

fn write_samples(path: &str, samples: &[f32]) {
    let bytes: Vec<u8> = samples.iter().flat_map(|sample| sample.to_le_bytes()).collect();
    std::fs::write(path, bytes).expect("write the samples");
}

/// Plays a file of samples through the real sender and receiver, over the real network stack,
/// with this test standing in for the sound card at each end.
#[test]
#[ignore = "run by tests/audio_measure.py"]
fn packet_path() {
    let input_path = env("MK_INPUT_FILE").expect("MK_INPUT_FILE");
    let output_path = env("MK_OUTPUT_FILE").expect("MK_OUTPUT_FILE");
    let port = number("MK_PORT", 24951) as u16;
    AUDIO_BITRATE_KBPS.store(number("MK_KBPS", 96) as u32, Ordering::SeqCst);
    devices::set_choice(devices::Choice { input: String::new(), output: "default".into(), play: "default".into() });

    let bytes = std::fs::read(input_path).expect("read the samples");
    let signal: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();

    let hub: &'static Hub = Box::leak(Box::default());
    let stop: &'static AtomicBool = Box::leak(Box::default());
    let stream = Arc::new(Mutex::new(RxStream::default()));

    let listen = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    let for_receiver = Arc::clone(&stream);
    let receiver = std::thread::spawn(move || receive_loop(&listen, &for_receiver, SocketAddr::from(([127, 0, 0, 1], 0)), stop));
    AUDIO_PLAYER_LIVE.store(true, Ordering::SeqCst);
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sender = std::thread::spawn(move || {
        sender::send_loop(&socket, SocketAddr::from(([127, 0, 0, 1], port)), hub, stop);
    });

    let frame = OPUS_FRAME * CHANNELS;
    let start = Instant::now();
    let tick = Duration::from_millis(10);
    let mut output = Vec::with_capacity(signal.len());
    let mut pulled = vec![0.0f32; frame];
    let mut most_buffered = 0.0f64;
    let mut buffered_total = 0.0f64;
    let frames = signal.len() / frame;
    for index in 0..frames {
        let due = start + tick * index as u32;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        hub.output.push(&signal[index * frame..(index + 1) * frame], &hub.signal);
        let now = Instant::now();
        let mut stream = stream.lock().unwrap();
        stream.fill(&mut pulled, now);
        most_buffered = most_buffered.max(stream.buffered_ms());
        buffered_total += stream.buffered_ms();
        drop(stream);
        output.extend_from_slice(&pulled);
    }
    stop.store(true, Ordering::SeqCst);
    sender.join().unwrap();
    receiver.join().unwrap();
    write_samples(&output_path, &output);

    let stream = stream.lock().unwrap();
    let counts = stream.counts();
    println!(
        "MEASURED {}",
        serde_json::json!({
            "received": counts.received,
            "repaired": counts.repaired,
            "concealed": counts.concealed,
            "late": counts.late,
            "trimmed": counts.trimmed,
            "tx_packets": AUDIO_TX_PACKETS.load(Ordering::SeqCst),
            "tx_bytes": sender::BYTES_SENT.load(Ordering::SeqCst),
            "buffered_mean_ms": buffered_total / frames as f64,
            "buffered_most_ms": most_buffered,
            "reported_loss": AUDIO_TX_LOSS.load(Ordering::SeqCst),
        })
    );
}

/// One whole device, minus input sharing: real capture, real playback, real device handling.
#[test]
#[ignore = "run by tests/audio_devices.py, beside a private sound server"]
fn whole_device() {
    let listen_port = number("MK_LISTEN", 24951) as u16;
    let peer_port = number("MK_PEER", 24952) as u16;
    AUDIO_BITRATE_KBPS.store(number("MK_KBPS", 96) as u32, Ordering::SeqCst);
    let choice = |name: &str| env(name).unwrap_or_default();
    devices::set_choice(devices::Choice {
        input: choice("MK_CHOOSE_INPUT"),
        output: choice("MK_CHOOSE_OUTPUT"),
        play: env("MK_CHOOSE_PLAY").unwrap_or_else(|| "default".into()),
    });
    AUDIO_PEER_ONLINE.store(true, Ordering::SeqCst);
    let peer = SocketAddr::from(([127, 0, 0, 1], peer_port));
    start_audio_listener(listen_port, peer).unwrap();
    start_audio_sender(peer).unwrap();
    if env("MK_MUTE").is_some() {
        crate::mute::start();
    }

    // Each line on stdin is an action, as the app would send it; each status goes to stdout.
    let (lines, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });
    let end = Instant::now() + Duration::from_secs(number("MK_SECONDS", 10));
    while Instant::now() < end {
        if let Ok(line) = read.recv_timeout(Duration::from_millis(200)) {
            let action: serde_json::Value = serde_json::from_str(&line).unwrap();
            let act = action["action"].as_str().unwrap_or("").to_string();
            match act.as_str() {
                "set_audio_device" => {
                    let mut choice = devices::choice();
                    let id = action["id"].as_str().unwrap_or("").to_string();
                    match action["role"].as_str() {
                        Some("input") => choice.input = id,
                        Some("output") => choice.output = id,
                        _ => choice.play = id,
                    }
                    devices::set_choice(choice);
                    choices_changed();
                }
                "chime" => trigger_test_chime(false, true),
                _ => {}
            }
        }
        println!("STATUS {}", crate::settings::audio_status_json());
        let _ = std::io::stdout().flush();
    }
    if env("MK_MUTE").is_some() {
        crate::mute::shutdown();
    }
}
