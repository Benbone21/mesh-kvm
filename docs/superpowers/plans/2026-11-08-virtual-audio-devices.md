Withdrawn in 0.9.2: microphone sharing and the virtual devices were removed.

# Virtual Mics and Speakers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A microphone sent from one device appears as a microphone on the other. On Debian, a speaker for the laptop appears too. The laptop shows Debian's mic through VB-Cable.

**Architecture:** The audio bus carries two streams each way, sound and mic, told apart by a stream byte in packet version 3. Each stream has its own packetizer, `RxStream` and reports.
- **Linux:** the service loads two `pactl` modules while the peer is linked.
  - A `module-pipe-source` for the mic, fed through a FIFO.
  - A `module-null-sink` for the speaker. Its apps are linked into the existing output capture.
- **Windows:** received mic audio plays into VB-Cable's "CABLE Input" endpoint when it exists.

**Tech Stack:** Rust; PipeWire through `pactl`/`pw-dump`/`pw-link`; WASAPI through `cpal` and `windows`; egui 0.31 app.

**Spec:** `docs/superpowers/specs/2026-11-08-virtual-audio-devices-design.md`

## Global Constraints

- **Packet version:** packet `VERSION` becomes `3`. A version-2 packet is refused as `Refused::OtherVersion`.
- **Stream ids:** `0` is sound and `1` is mic.
- **Linux module names:** source `mesh-kvm-peer-mic` and sink `mesh-kvm-peer-speakers`. Both start with the existing `PREFIX` `mesh-kvm-`, so `is_ours` keeps them out of mesh-kvm's own device lists.
- **Linux descriptions:**
  - Mic: `"<peer name> mic (mesh-kvm)"`.
  - Speaker: `"<peer name> speakers (mesh-kvm)"`.
- **Linux mic FIFO:** `$XDG_RUNTIME_DIR/mesh-kvm-peer-mic.fifo`. Pipe-source format `float32le`, `rate=48000`, `channels=2`.
- **When the Linux devices exist:** only while paired **and** `AUDIO_PEER_ONLINE` is true.
  - **Removed:** on link drop, on unpair, and on service stop.
  - **At service start:** every module whose arguments contain `mesh-kvm-peer-` is unloaded.
- **Windows VB-Cable endpoints:** found by name containing `CABLE Input` (render) and `CABLE Output` (capture). Both are left out of every mesh-kvm device list.
- **Copy, verbatim:**
  - Debian with devices: `"<peer> mic, <peer> speakers — in sound settings"`.
  - Debian without them: `"Appears when linked"`.
  - Windows with the cable: `"Pick CABLE Output as the mic for <peer>"`, hint `"You can rename it in Windows Sound settings."`.
  - Windows without the cable: `"Install VB-Cable to get <peer>'s mic here"`, link `https://vb-audio.com/Cable/`.
  - On the sender: `"<peer> has no virtual mic. Install VB-Cable there."`.
- **Which stream gets what:** send volume, limiter and the both-sending warning apply to the sound stream only. The bitrate setting applies to both.
- **Unchanged:** a received mic is never played on speakers. The existing "Microphone or line-in" choice is unchanged.
- **Version:** 0.9.0.
- **Repo rules:**
  - Commit as bread <jj1nfz2nb@mozmail.com>.
  - Never stage the repo-root `layout.json`.
  - The signing key is never committed.
  - Releases only with the user's OK.

## Review Focus

1. **A peer name with spaces, quotes, `=` or non-ASCII:** the description shows the name, and nothing in it reaches `pactl` as an extra argument. Task 4 tests `module_args` with `Bob's "PC" a=b`.
2. **A service killed with SIGKILL, then started again:** no duplicate devices are left; the old modules are unloaded first. Task 4 has a live test.
3. **A link that flaps up and down every second:** there's never more than one of each device, and none is left behind once it settles. Task 4 tests `VirtualDevices::step` with a flapping sequence.
4. **The virtual speaker set as Debian's default output, with "Output to send" also on the default:** each app is linked into the capture once, not twice. Task 4 tests `plan_links`.
5. **Laptop still on 0.8.1 while Debian is on 0.9.0:** the other-version note shows, nothing plays, and nothing panics. Task 1 tests version-2 refusal; Task 3 tests that the receiver notes it.

---

### Task 1: Packet version 3 with a stream id

**Files:**
- Modify: `src/audio/packet.rs`

**Interfaces:**
- Produces: `pub enum Stream { Sound = 0, Mic = 1 }` (`Copy, Eq, Debug`), and `Packet { epoch, seq, stream: Stream, body }`.
  - `write_audio(out, format, stream, epoch, seq, payload)`, `write_quiet(out, stream, epoch, seq)` and `write_report(out, stream, epoch, report)` all gain a `stream: Stream` parameter.
  - `HEADER` becomes 13: magic, version, kind, stream, epoch, seq.
  - An unknown stream byte is `Refused::Stranger`.

- [ ] **Step 1: Write the tests, update the existing ones to pass `Stream::Sound`, and add these**

```rust
#[test]
fn each_stream_survives_the_round_trip() {
    let mut out = Vec::new();
    for stream in [Stream::Sound, Stream::Mic] {
        write_audio(&mut out, Format::Opus, stream, 7, 300, &[1, 2, 3]);
        assert_eq!(parse(&out).unwrap().stream, stream);
        write_quiet(&mut out, stream, 7, 301);
        assert_eq!(parse(&out).unwrap().stream, stream);
        write_report(&mut out, stream, 7, Report { ready: true, ..Default::default() });
        assert_eq!(parse(&out).unwrap().stream, stream);
    }
}

#[test]
fn version_two_is_turned_away() {
    let mut out = Vec::new();
    write_audio(&mut out, Format::Opus, Stream::Sound, 1, 1, &[1]);
    out[4] = 2;
    assert_eq!(parse(&out), Err(Refused::OtherVersion));
}

#[test]
fn an_unknown_stream_is_a_stranger() {
    let mut out = Vec::new();
    write_audio(&mut out, Format::Opus, Stream::Sound, 1, 1, &[1]);
    out[6] = 9;
    assert_eq!(parse(&out), Err(Refused::Stranger));
}
```

- [ ] **Step 2:** Run `cargo test --release audio::packet`. Expected: compile failure, because `Stream` is missing.
- [ ] **Step 3:** Implement `Stream`, the new header layout and `VERSION = 3`. Fix the callers in `sender.rs` and `mod.rs` to pass `Stream::Sound`, so behaviour is unchanged.
- [ ] **Step 4:** Run `cargo test --release`. Expected: all tests pass.
- [ ] **Step 5:** Commit: `Audio packets carry a stream id (version 3)`.

### Task 2: The sender sends sound and mic as separate streams

**Files:**
- Modify: `src/audio/sender.rs`, `src/audio/mod.rs` (`Hub::capture`, `HEARD_BACK`)

**Interfaces:**
- Consumes: `packet::Stream`, plus the `stream` parameter on the `write_*` functions (Task 1).
- Produces:
  - **Packetizers:** `Packetizer::new(kbps: u32, stream: Stream)`. Each packetizer writes its own stream id and keeps its own `seq`. The two share one epoch per run, and the epoch is passed in.
  - **Sources:** the sound packetizer takes `[&hub.output, &hub.chime]` through its own `Mixer` and `Limiter`. The mic packetizer takes `[&hub.mic]` through its own `Mixer`, with no limiter.
  - **Volume:** `Hub::capture` applies `AUDIO_TX_VOLUME_PCT` only when `source` is not `&self.mic`.
  - **Reports:** a report for `Stream::Sound` goes to the existing `note_heard_back`. A report for `Stream::Mic` goes to a new `note_mic_heard_back(report, now)`.
  - **Mic readiness:** `pub fn peer_mic_ready(now: Instant, within: Duration) -> bool` in `mod.rs`, which reads the mic report's `ready` flag.
  - **Unchanged figures:** `AUDIO_TX_PEAK_PCT` and `AUDIO_TX_SOUND_PACKETS` count the sound stream only.
  - **Quiet markers:** each stream sends its own `QUIET_EVERY` marker only while that stream's sources are selected. The mic stream's sources are selected when the choice's input is not off. The sound stream's sources are selected when the output is not off, or when the chime is playing.

- [ ] **Step 1: Write the failing tests in `sender.rs`.** They use the existing test helpers there, with a fake `send` closure that collects packets:

```rust
#[test]
fn mic_and_sound_go_out_as_separate_streams() {
    // Push one frame into hub.mic and one into hub.output; run one send pass.
    // Parse what went out: exactly one Stream::Mic audio packet and one Stream::Sound audio packet,
    // each with seq 0.
}

#[test]
fn a_mic_alone_sends_no_sound_stream() {
    // Only hub.mic has audio and the output choice is off: every packet is Stream::Mic.
}

#[test]
fn send_volume_leaves_the_mic_alone() {
    // AUDIO_TX_VOLUME_PCT = 50; a mic sample of 0.5 arrives in the mic source as 0.5,
    // and an output sample of 0.5 arrives as 0.25.
}

#[test]
fn a_mic_report_only_moves_mic_readiness() {
    // A Stream::Mic report with ready=true: peer_mic_ready is true and peer_ready (sound) is false.
}
```

  Write each body to match its comment, with real assertions.
- [ ] **Step 2:** Run `cargo test --release audio::sender`. Expected: FAIL.
- [ ] **Step 3:** Implement as described in Interfaces. Restructure `send_loop` around a small `struct Outgoing { packetizer, mixer, limiter: Option<Limiter>, last_sent, said_quiet }` per stream, so the loop body runs once per stream.
- [ ] **Step 4:** Run `cargo test --release`. Expected: PASS.
- [ ] **Step 5:** Commit: `Send the microphone as its own stream`.

### Task 3: The receiver keeps two streams and reports on each

**Files:**
- Modify: `src/audio/mod.rs` (`Receiver`, `receive_loop`, `start_audio_listener`), and both platform modules for the new function signatures.

**Interfaces:**
- Consumes: `Packet.stream` (Task 1).
- Produces:
  - **Routing:** `Receiver` holds `sound: &Mutex<RxStream>` and `mic: &Mutex<RxStream>`, and routes each packet by `packet.stream`. It keeps one epoch per stream.
  - **Reports:** each stream reports every `REPORT_EVERY`, tagged with its stream, while that stream is alive.
  - **Figures:** the sound stream keeps every existing `AUDIO_RX_*` figure. The mic stream sets only a new `pub static AUDIO_RX_MIC_LIVE: AtomicBool`.
  - **Mic readiness:** a mic report's `ready` is `platform::virtual_mic_ready()`, and its `playing` is the same value.
  - **Mic stream with nowhere to play:** while `virtual_mic_ready()` is false, the mic stream is drained with the existing `drain_received` path, so it never grows.
  - **Platform functions:** each platform module gains `pub fn start_mic_playback(stream: Arc<Mutex<RxStream>>)` and `pub fn virtual_mic_ready() -> bool`. Until Tasks 4 and 5 fill them in, both platforms start a thread that drains the stream, and `virtual_mic_ready` returns false.

- [ ] **Step 1: Write the failing tests in `mod.rs` tests.** They use `paired_channels()` and a loopback socket pair, as the existing receive tests do.

```rust
#[test]
fn mic_packets_never_reach_the_sound_stream() {
    // Send 20 Stream::Mic Opus frames: sound.counts().received == 0, mic.counts().received == 20.
}

#[test]
fn each_stream_reports_for_itself() {
    // After both streams have packets, the sender side reads one Stream::Sound report and one
    // Stream::Mic report within 2 * REPORT_EVERY; the mic one has ready == virtual_mic_ready().
}

#[test]
fn an_older_peer_is_noted_not_played() {
    // A version-2 datagram, sealed: other_version_arriving() becomes true, and neither stream grows.
}
```

- [ ] **Step 2:** Run `cargo test --release audio::tests`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --release`. Also build the Windows target and run the Wine tests (`packaging/` has the existing commands; Wine tests use `winemenubuilder.exe=d`). Expected: PASS.
- [ ] **Step 5:** Commit: `Receive the microphone as its own stream`.

### Task 4: Linux virtual mic and speaker

**Files:**
- Create: `src/audio/virtual_linux.rs`, with a `#[cfg(target_os = "linux")] mod virtual_linux;` line in `src/audio/mod.rs`
- Modify: `src/audio/linux.rs` (`engine`, `plan_links`, `apps_running_on`, `start_mic_playback`, `virtual_mic_ready`), `src/audio/live.rs`

**Interfaces:**
- Consumes: `start_mic_playback` and `virtual_mic_ready` (Task 3); `AUDIO_PEER_ONLINE`; and the peer's name and paired state, read from the pairing state that `secure::pairing`'s status uses (`peer_name`).
- Produces, all in `virtual_linux.rs`:
  - **Names:** `pub const MIC_NAME: &str = "mesh-kvm-peer-mic"`, `pub const SPEAKERS_NAME: &str = "mesh-kvm-peer-speakers"`, and `pub fn fifo_path() -> PathBuf`.
  - **Name cleaning:** `pub fn clean_name(peer: &str) -> String` removes `"`, `'`, `\`, `=` and control characters, trims, and falls back to `"Other device"` if the result is empty.
  - **Module arguments:** `pub fn module_args(kind: Kind, peer: &str) -> Vec<String>`, where `enum Kind { Mic, Speakers }`, returns the exact `pactl load-module` argument list:
    - Mic: `module-pipe-source source_name=mesh-kvm-peer-mic file=<fifo> format=float32le rate=48000 channels=2 source_properties=device.description="<clean> mic (mesh-kvm)"`.
    - Speakers: `module-null-sink sink_name=mesh-kvm-peer-speakers sink_properties=device.description="<clean> speakers (mesh-kvm)"`.
  - **Wanted state:** `pub fn wanted(paired: bool, online: bool, peer: Option<&str>) -> Option<String>` returns the clean name while the devices should exist.
  - **Lifecycle:** `pub struct VirtualDevices` with `pub fn step(&mut self, wanted: Option<String>, run: &mut dyn FnMut(&[String]) -> Option<String>)`. `run` executes one `pactl` call and returns its stdout.
    - It loads when wanted and nothing is loaded.
    - It unloads both modules by the ids it stored when not wanted.
    - It unloads and reloads when the name changes.
    - After a failed load, it waits `RETRY_AFTER = 5 s` before trying again.
  - **Startup cleanup:** `pub fn clear_leftovers(run)` unloads every module in `pactl list short modules` whose line contains `mesh-kvm-peer-`.
  - **Status:** `pub fn present() -> Option<String>` gives the clean name while both modules are loaded. The status JSON reads it, and `virtual_mic_ready` is `present().is_some()`.
  - **Errors:** `pub fn last_error() -> Option<String>` gives the first line of `pactl`'s stderr from the latest failed load, and is cleared on success.
- **Feeding the mic.** `start_mic_playback` runs a thread that does three things while `present()` is some:
  1. Opens `fifo_path()` with `O_WRONLY | O_NONBLOCK`.
  2. Every 10 ms by the clock, fills 480 frames from the mic `RxStream`.
  3. Writes them as `f32` little-endian, and drops the frame on `EAGAIN`.

  When nothing is present, it drains the stream as Task 3 does.
- **Sending the speaker.** `plan_links` takes `sinks: &[&str]` in place of `sink: Option<&str>`. The engine passes the chosen sink plus `SPEAKERS_NAME` when present, de-duplicated. The output capture runs when the output to send is not off **or** `apps_running_on(SPEAKERS_NAME)` is true.
- **Engine.** It calls `clear_leftovers` once at start. It calls `step` on every look, and also on a `WAKE` from link changes. It calls `step(None, …)` from the service's existing shutdown path.

- [ ] **Step 1: Write the failing unit tests in `virtual_linux.rs`**

```rust
#[test]
fn names_cannot_smuggle_arguments() {
    let args = module_args(Kind::Mic, &clean_name("Bob's \"PC\" a=b"));
    assert_eq!(args.last().unwrap(), "source_properties=device.description=\"Bobs PC ab mic (mesh-kvm)\"");
    assert_eq!(clean_name("  \u{7}  "), "Other device");
}

#[test]
fn devices_exist_only_while_paired_and_linked() {
    assert_eq!(wanted(true, true, Some("SACBOOKPRO")).as_deref(), Some("SACBOOKPRO"));
    assert_eq!(wanted(true, false, Some("SACBOOKPRO")), None);
    assert_eq!(wanted(false, true, None), None);
}

#[test]
fn a_flapping_link_never_leaves_two_of_anything() {
    // Fake `run`: load-module returns increasing ids, and the fake records the loaded set.
    // Run step() with Some, None, Some, None, Some, Some("Renamed"), None:
    // at most 2 loaded after every step, 0 at the end, and every id loaded was unloaded.
}

#[test]
fn leftovers_from_a_crash_are_cleared() {
    // Fake `list short modules` output with two mesh-kvm-peer- lines and one other module:
    // exactly those two ids are unloaded.
}
```

  Add these to the `linux.rs` tests:

```rust
#[test]
fn an_app_on_the_virtual_speaker_is_captured_once_even_if_it_is_the_default_too() {
    // Graph: app stream linked to mesh-kvm-peer-speakers; sinks = ["mesh-kvm-peer-speakers", "mesh-kvm-peer-speakers"]:
    // one link per channel into the capture, no duplicates.
}

#[test]
fn the_mic_feed_is_never_captured() {
    // A node named mesh-kvm-peer-mic and its pipe-source are ignored by apps_on and lists().
}
```

- [ ] **Step 2:** Run `cargo test --release audio::virtual_linux audio::linux`. Expected: FAIL.
- [ ] **Step 3:** Implement as in Interfaces.
- [ ] **Step 4: Add a live test to `live.rs`, gated the way that file's other tests are.** It does this:
  1. Starts `VirtualDevices` with the name `Live Test`.
  2. Checks that `pactl list short sources` has `mesh-kvm-peer-mic` and that `pactl list short sinks` has `mesh-kvm-peer-speakers`.
  3. Feeds a 440 Hz marked tone through the FIFO path and records it with `parecord -d mesh-kvm-peer-mic`. The recorded peak must be at least 0.2.
  4. Calls `step(None)`; both devices must be gone.
  5. Loads the modules, drops `VirtualDevices` without stepping, and runs `clear_leftovers`; both must be gone.
- [ ] **Step 5:** Run `cargo test --release`, live tests included. Expected: PASS. Then run `pactl list short modules | grep mesh-kvm-peer`, which must print nothing.
- [ ] **Step 6:** Commit: `Linux: a mic and a speaker for the other device while linked`.

### Task 5: Windows plays Debian's mic into VB-Cable

**Files:**
- Modify: `src/audio/wasapi.rs`

**Interfaces:**
- Consumes: `start_mic_playback` and `virtual_mic_ready` (Task 3).
- Produces:
  - **Cable detection:** `fn cable(found: &[Device]) -> Cable`, with `struct Cable { input: Option<Device>, output: Option<Device> }`, matches by name containing `CABLE Input` and `CABLE Output`.
  - **List filtering:** `sort_out` leaves both cable endpoints out of the lists it returns, and stores the cable in a `static CABLE: Mutex<Cable>` on every `read_lists`.
  - **Mic readiness:** `virtual_mic_ready()` is true when `CABLE.input` is some.
  - **Mic playback:** `start_mic_playback` uses the same playback worker as `start_playback`, aimed at the cable's input endpoint. It re-checks on each `LOOK_EVERY`, and drains the stream while there's no cable.
  - **Status:** `pub fn cable_found() -> bool` for the status JSON.
  - **Echo safety:** process-exclusion loopback already leaves out this process's sound, which covers the cable feed. Check this in the code and note it in a comment; don't add logic.

- [ ] **Step 1: Write the failing tests, under Wine with the existing Windows test setup**

```rust
#[test]
fn the_cable_is_found_by_name_and_kept_out_of_the_lists() {
    let found = vec![dev("{a}", "Speakers (Realtek)"), dev("{b}", "CABLE Input (VB-Audio Virtual Cable)"), dev("{c}", "CABLE Output (VB-Audio Virtual Cable)")];
    let cable = cable(&found);
    assert_eq!(cable.input.unwrap().id, "{b}");
    assert_eq!(cable.output.unwrap().id, "{c}");
    assert!(sort_out(found).all_names().iter().all(|n| !n.contains("CABLE")));
}

#[test]
fn no_cable_means_no_virtual_mic() {
    assert!(cable(&[dev("{a}", "Speakers (Realtek)")]).input.is_none());
}
```

  `dev` and `all_names` are small test helpers; write them if `wasapi.rs` has none.
- [ ] **Step 2:** Build for Windows and run under Wine. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --release` on Linux, plus the Windows build and the Wine tests. Expected: PASS, with no warnings in either build.
- [ ] **Step 5:** Commit: `Windows: play the other device's mic into VB-Cable`.

### Task 6: Status and the Virtual devices row

**Files:**
- Modify: `src/settings.rs` (`audio_status_json`), `src/app/model.rs` (audio status parsing), and `src/app/rows.rs` (audio rows and tests)

**Interfaces:**
- Consumes:
  - From Task 4, `virtual_linux::present()` and `virtual_linux::last_error()`.
  - From Task 5, `wasapi::cable_found()`.
  - From Task 2, `peer_mic_ready`.
  - From Task 3, `AUDIO_RX_MIC_LIVE`.
- Produces:
  - **New status keys** in `audio_status_json`:
    - `"virtual": { "kind": "linux" | "cable" | "no_cable", "present": bool, "error": string | null }`.
    - `"tx_mic_ready": bool`, true while the peer reported a ready mic within 2 s.
    - `"rx_mic_live": bool`.
  - **Model:** `AudioStatus` gains `virtual_kind: VirtualKind { Linux, Cable, NoCable }` and `virtual_present: bool`, each defaulting to absent for older peers, plus `tx_mic_ready: bool`.
  - **New row:** "Virtual devices", placed after "Play received audio on". Each device's cell text comes from the copy in Global Constraints. `<peer>` is the *other* column's device name, the same name the table header shows.
    - **Debian:** `Linux` with `present` true gives the in-sound-settings text; with `error` set, `"Couldn't add the devices: <error>"`; otherwise "Appears when linked".
    - **Windows:** `Cable` gives the pick text, with the rename hint as the tooltip. `NoCable` gives the install text, with the link opened by the cell's action.
  - **Mic note:** on the sending device, when its input is not off and the other device's `virtual_present` is false, the "Microphone or line-in" cell carries the note `"<peer> has no virtual mic. Install VB-Cable there."`, but only when the peer's `virtual_kind` is `NoCable`.
  - **Accessibility:** every new cell has an accessible name made of the row label and the cell text, as the other cells do.

- [ ] **Step 1: Write the failing tests in `rows.rs`.** They build `DeviceStatus` values with the existing test helpers.

```rust
#[test]
fn virtual_devices_row_says_what_is_in_sound_settings() {
    // Debian present, laptop Cable: Debian cell "SACBOOKPRO mic, SACBOOKPRO speakers — in sound settings",
    // laptop cell "Pick CABLE Output as the mic for DEBIAN".
}

#[test]
fn virtual_devices_row_without_link_or_cable() {
    // Debian not present: "Appears when linked"; laptop NoCable: "Install VB-Cable to get DEBIAN's mic here".
}

#[test]
fn a_failed_device_load_is_shown() {
    // Debian error "No such module": cell "Couldn't add the devices: No such module".
}

#[test]
fn a_mic_sent_to_a_device_without_a_cable_is_called_out() {
    // Debian input = a mic, laptop NoCable: the Debian "Microphone or line-in" cell note is
    // "SACBOOKPRO has no virtual mic. Install VB-Cable there."; with laptop Cable there is no note.
}

#[test]
fn an_older_peer_shows_no_virtual_devices_row_cell_text() {
    // A peer status without the "virtual" key: its cell is blank, nothing panics.
}
```

  Also update the existing row-order test, which lists every audio row label, to include "Virtual devices".
- [ ] **Step 2:** Run `cargo test --release app::rows`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --release`, plus the Wine `app::` tests. Expected: PASS.
- [ ] **Step 5:** Commit: `App: show the virtual devices on each side`.

### Task 7: Version 0.9.0 and the README

**Files:**
- Modify: `Cargo.toml` (version `0.9.0`), `Cargo.lock`, and `README.md` (the audio section)

- [ ] **Step 1:** Set the version to `0.9.0`. In the README, describe the virtual mic and speaker in a short paragraph: what appears on each device, and when. Add the VB-Cable install note with the link `https://vb-audio.com/Cable/`, and say that both devices must update together.
- [ ] **Step 2:** Run `cargo test --release`, the Windows build and the Wine tests. Expected: all pass with no warnings.
- [ ] **Step 3:** Commit: `0.9.0: virtual mics and speakers`.
- [ ] **Step 4: Check on the real devices.** This is the user's step after release, so list it in the hand-off; don't do it here.
  - On Debian, pick "SACBOOKPRO mic (mesh-kvm)" in an app, and it records the laptop's mic.
  - Play into "SACBOOKPRO speakers (mesh-kvm)", and the sound comes out of the laptop.
  - With VB-Cable installed on the laptop, pick CABLE Output as the mic there, and it records Debian's mic.
  - Nothing sent on one stream is heard on the other.
