//! Tests of the real audio threads, run from `tests/audio_measure.py` and `tests/audio_devices.py`,
//! which set up what they need: `cargo test` skips them. The scripts run them with a private sound
//! server and network, as these would otherwise use the real ones.

use super::jitter::RxStream;
use super::*;
use crate::secure::datagram::{Label, LinkSecurity, Role};
use crate::secure::keys::{Loaded, Pairing, PeerKey};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn number(name: &str, default: u64) -> u64 {
    env(name).and_then(|value| value.parse().ok()).unwrap_or(default)
}

/// This device's two audio channels, from the file the script made: sending, then receiving.
fn channels_from_test_pairing() -> (Arc<Channel>, Arc<Channel>) {
    let path = env("MK_PAIRING").expect("MK_PAIRING names this device's pairing file");
    match Pairing::load_from(Path::new(&path)) {
        Loaded::Ready(pairing) => (
            Arc::new(Channel::new(&pairing, Label::Audio, Role::Initiator)),
            Arc::new(Channel::new(&pairing, Label::Audio, Role::Responder)),
        ),
        Loaded::Damaged(problem) => panic!("{problem}"),
    }
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

    // Both ends are here, so no file is needed for the two to know each other.
    let (mut near, mut far) = (Pairing::fresh(), Pairing::fresh());
    near.peer = Some(PeerKey { public: far.identity.public, name: "Far".to_string(), paired_at: 1 });
    far.peer = Some(PeerKey { public: near.identity.public, name: "Near".to_string(), paired_at: 1 });
    let out = Arc::new(Channel::new(&near, Label::Audio, Role::Initiator));
    let incoming = Channel::new(&far, Label::Audio, Role::Responder);

    let listen = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    let for_receiver = Arc::clone(&stream);
    let for_mic = Arc::new(Mutex::new(RxStream::default()));
    let receiver = std::thread::spawn(move || receive_loop(&listen, &for_receiver, &for_mic, SocketAddr::from(([127, 0, 0, 1], 0)), &incoming, stop));
    AUDIO_PLAYER_LIVE.store(true, Ordering::SeqCst);
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let for_sender = Arc::clone(&out);
    let sender = std::thread::spawn(move || {
        sender::send_loop(&socket, SocketAddr::from(([127, 0, 0, 1], port)), &for_sender, hub, stop);
    });
    // The measure starts with the first frame, and a real device has a session long before sound.
    let ready_by = Instant::now() + Duration::from_secs(5);
    while out.security() != LinkSecurity::Encrypted {
        assert!(Instant::now() < ready_by, "no session");
        std::thread::sleep(Duration::from_millis(5));
    }

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
    let peer_host = env("MK_PEER_HOST").and_then(|host| host.parse().ok()).unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
    let peer = SocketAddr::new(peer_host, peer_port);
    let (out, incoming) = channels_from_test_pairing();
    start_audio_listener(listen_port, peer, incoming).unwrap();
    start_audio_sender(peer, out).unwrap();
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

/// Lists what `pactl list short <what>` shows.
fn pactl_list(what: &str) -> String {
    virtual_linux::pactl(&["list".to_string(), "short".to_string(), what.to_string()]).expect("pactl list")
}

/// Whatever happens in the test, nothing of ours is left in the sound server.
struct ClearAtEnd;

impl Drop for ClearAtEnd {
    fn drop(&mut self) {
        virtual_linux::clear_leftovers(&mut virtual_linux::pactl);
    }
}

/// Records `seconds` from the virtual mic, as an app would, while a tone is fed to its FIFO.
fn record_tone_from_virtual_mic(seconds: f32) -> Vec<f32> {
    use std::io::Read;
    let stop = Arc::new(AtomicBool::new(false));
    let feeding = Arc::clone(&stop);
    // A 440 Hz tone at half scale, fed the way the mic feed does: 10 ms at a time by the clock.
    let feeder = std::thread::spawn(move || {
        let mut fifo = linux::open_mic_fifo().expect("the mic source's FIFO");
        let start = Instant::now();
        let mut phase = 0usize;
        let mut sent = 0u32;
        while !feeding.load(Ordering::SeqCst) {
            let mut bytes = Vec::with_capacity(OPUS_FRAME * CHANNELS * 4);
            for _ in 0..OPUS_FRAME {
                let sample = (phase as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.5;
                phase += 1;
                bytes.extend(sample.to_le_bytes());
                bytes.extend(sample.to_le_bytes());
            }
            assert!(linux::write_mic_frame(&mut fifo, &bytes), "the FIFO went away");
            sent += 1;
            if let Some(wait) = (start + Duration::from_millis(10) * sent).checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
    });
    std::thread::sleep(Duration::from_millis(200));
    let mut recorder = std::process::Command::new("parecord")
        .args(["-d", virtual_linux::MIC_NAME, "--raw", "--format=float32le", "--rate=48000", "--channels=2"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("parecord");
    let mut stdout = recorder.stdout.take().unwrap();
    let wanted = (seconds * RATE as f32) as usize * CHANNELS * 4;
    let (done, recorded) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = vec![0u8; wanted];
        let read = stdout.read_exact(&mut bytes).map(|_| bytes);
        let _ = done.send(read);
    });
    let bytes = recorded.recv_timeout(Duration::from_secs(10));
    let _ = recorder.kill();
    let _ = recorder.wait();
    stop.store(true, Ordering::SeqCst);
    feeder.join().unwrap();
    let bytes = bytes.expect("parecord gave nothing in time").expect("parecord ended early");
    bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

/// The other device's mic and speakers, in this machine's own sound server. It loads and unloads
/// only modules named `mesh-kvm-peer-`, and clears them whatever happens.
#[test]
#[ignore = "uses this machine's sound server: cargo test --release -- --ignored audio::live::virtual_devices"]
fn virtual_devices() {
    let _clear = ClearAtEnd;
    virtual_linux::clear_leftovers(&mut virtual_linux::pactl);
    let mut devices = virtual_linux::VirtualDevices::default();
    devices.step(Some("Live Test".to_string()), &mut virtual_linux::pactl);
    assert_eq!(devices.present(), Some("Live Test"), "{:?}", devices.error());
    assert!(pactl_list("sources").contains(virtual_linux::MIC_NAME));
    assert!(pactl_list("sinks").contains(virtual_linux::SPEAKERS_NAME));

    // Half a second in, so the source has settled.
    let recorded = record_tone_from_virtual_mic(1.5);
    let peak = recorded[recorded.len() / 3..].iter().fold(0.0f32, |most, sample| most.max(sample.abs()));
    println!("recorded peak {peak:.3}");
    assert!(peak >= 0.2, "the tone didn't come through: peak {peak}");

    devices.step(None, &mut virtual_linux::pactl);
    assert_eq!(devices.present(), None);
    assert!(!pactl_list("sources").contains(virtual_linux::MIC_NAME));
    assert!(!pactl_list("sinks").contains(virtual_linux::SPEAKERS_NAME));

    // A service killed with its devices up leaves them behind; the next start clears them.
    let mut killed = virtual_linux::VirtualDevices::default();
    killed.step(Some("Live Test".to_string()), &mut virtual_linux::pactl);
    assert!(killed.present().is_some());
    drop(killed);
    assert!(pactl_list("modules").contains("mesh-kvm-peer-"));
    virtual_linux::clear_leftovers(&mut virtual_linux::pactl);
    assert!(!pactl_list("modules").contains("mesh-kvm-peer-"));
    assert!(!pactl_list("sources").contains(virtual_linux::MIC_NAME));
    assert!(!pactl_list("sinks").contains(virtual_linux::SPEAKERS_NAME));
}
