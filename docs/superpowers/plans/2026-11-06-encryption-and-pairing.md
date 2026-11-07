# Encryption and Pairing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Encrypt and authenticate all traffic between the two devices, with one-time pairing by a 6-digit code.

**Architecture:** A new `src/secure/` module holds keys, a replay window, a UDP `Channel`, a TCP `SecureStream`, the control listener, and pairing. Existing loops wrap their socket reads and writes with these, so `router.rs` and what `link.rs` decides stay the same.

**Tech Stack:** Rust, `snow` 0.10 (Noise), `spake2` 0.4, Python with `cryptography` for the independent test peer.

**Spec:** `docs/superpowers/specs/2026-11-06-encryption-and-pairing-design.md`. Read it first: wire formats, session rules, and constants are there and are not repeated here.

## Global Constraints

- Work on this worktree's branch, based on `stage-2-native-app` at v0.6.0. Not `master`.
- Every task leaves `cargo test` and `cargo check --target x86_64-pc-windows-gnu` passing with no warnings.
- `snow` is added with exactly the features in the spec, so `cargo tree | grep -E '^. *(ring|aes)'` stays empty. No cmake.
- Don't touch the live service, `~/.config`, real audio routing, or display modes. Tests set `HOME` or `MK_PAIRING` to a temporary path.
- Never stage `layout.json`: add files by name.
- Commit with `git -c user.name=bread -c user.email=jj1nfz2nb@mozmail.com commit`, ending the message with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. No push, tag, or release.
- The release profile has `panic = "abort"`: a panic on network input takes the keyboard with it. No `unwrap`, indexing, or slicing on received bytes without a length check.
- `src/app/` changes stay inside Task 11. Match the surrounding comment style: short, plain, no section banners.
- Windows hooks must never block: `Channel::seal` holds a lock only to clone an `Arc`.

## Review Focus

1. **Garbage datagrams of every length, on every channel:** dropped with no panic. Pinned in Task 3 (`any_bytes_are_dropped_without_panic`).
2. **A laptop waking after hours asleep:** both sessions are past 1 hour, and the link returns within a few handshake retries with no stuck keys. Pinned in Task 3 (`waking_after_hours_starts_a_new_session`).
3. **One device re-paired or reinstalled:** the other shows `connecting`, never `encrypted`, and keeps its local input. Pinned in Task 3 (`a_peer_with_a_new_key_never_shows_encrypted`).
4. **The peer dying part way through a file:** the receiver reports failure, and keeps no half batch as if complete. Pinned in Task 8 (`a_transfer_cut_off_is_not_taken`).
5. **Sealing from a hook while the receive thread changes sessions:** no blocking beyond the `Arc` clone, and no reused nonce. Pinned in Task 3 (`sealing_during_a_session_change_never_repeats_a_nonce`).

---

### Task 1: Keys and pairing file

**Files:**
- Create: `src/secure/mod.rs`, `src/secure/keys.rs`
- Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs` (`mod secure;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct Identity { pub private: [u8; 32], pub public: [u8; 32] }
  pub struct PeerKey { pub public: [u8; 32], pub name: String, pub paired_at: u64 }
  pub struct Pairing { pub identity: Identity, pub peer: Option<PeerKey> }
  pub enum Loaded { Ready(Pairing), Damaged(String) }
  impl Pairing {
      pub fn fresh() -> Pairing;
      pub fn load_from(path: &Path) -> Loaded;   // no file: Ready(fresh()), not saved
      pub fn save_to(&self, path: &Path) -> Result<(), String>;
      pub fn fingerprint(&self) -> Option<String>;
  }
  pub fn pairing_path() -> PathBuf;              // settings::config_dir().join("pairing.json")
  pub const NOISE_IK: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
  ```

- [ ] **Step 1: Write failing tests in `keys.rs`:** `pairing_round_trips`, `a_missing_file_is_fresh`, `a_corrupt_file_is_damaged_and_left_alone` (bytes on disk unchanged), `both_ends_show_the_same_fingerprint` (format matches `^[0-9A-F]{4}-[0-9A-F]{4}$`, same with the keys swapped), and on Linux `the_file_is_user_only` (mode `0o600`, including when the file already existed as `0o644`).
- [ ] **Step 2: Run** `cargo test secure::keys`. Expected: compile failure.
- [ ] **Step 3: Add the crates and implement.** Keys are hex in JSON through `serde`. Generate with `snow::Builder::generate_keypair`. Write through a temporary file in the same folder and rename. Fingerprint: BLAKE2s of `b"mesh-kvm fingerprint 1"` and both keys in sorted order, first 4 bytes.
- [ ] **Step 4: Run** `cargo test secure::keys`, the Windows check, and `cargo tree -e normal --prefix none | grep -E '^(ring|aes)'`. Expected: tests pass, grep prints nothing.
- [ ] **Step 5: Commit.**

### Task 2: Replay window

**Files:**
- Create: `src/secure/window.rs`

**Interfaces:**
- Produces:
  ```rust
  pub const WINDOW: u64 = 2048;
  #[derive(Default)] pub struct ReplayWindow { .. }
  impl ReplayWindow {
      pub fn may_be_new(&self, counter: u64) -> bool;  // before decrypting, changes nothing
      pub fn accept(&mut self, counter: u64) -> bool;  // after the tag checks out
  }
  ```

- [ ] **Step 1: Write failing tests:** `counters_in_any_order_are_each_taken_once`, `a_repeat_is_refused`, `the_oldest_in_the_window_is_taken_and_one_older_is_not` (newest 5000: 2953 taken, 2952 refused), `a_jump_clears_the_bits_it_passes`, `a_jump_of_a_billion_is_cheap`, `counter_zero_is_taken_once`.
- [ ] **Step 2: Run** `cargo test secure::window`. Expected: compile failure.
- [ ] **Step 3: Implement** a `[u64; 32]` bitmap indexed by `counter % 2048`, clearing passed words, with a jump of at least 2048 clearing everything once.
- [ ] **Step 4: Run** `cargo test secure::window`. Expected: pass.
- [ ] **Step 5: Commit.**

### Task 3: UDP channel

**Files:**
- Create: `src/secure/datagram.rs`

**Interfaces:**
- Consumes: `Pairing`, `NOISE_IK`, `ReplayWindow`.
- Produces:
  ```rust
  pub const OVERHEAD: usize = 29;
  pub enum Label { Input, Audio }      // prologues in the spec
  pub enum Role { Both, Initiator, Responder }
  pub enum Opened<'a> {
      Data(&'a [u8]),      // for the caller
      Reply(&'a [u8]),     // send this straight back to where it came from
      Heard,               // keepalive, or a handshake needing no reply
      Refused,             // failed a check: must not count as activity
      OldPeer,             // starts `MKVM` or `MKAU`
  }
  pub enum LinkSecurity { Unpaired, Connecting, Encrypted }
  impl Channel {
      pub fn new(pairing: &Pairing, label: Label, role: Role) -> Channel;
      pub fn seal(&self, plain: &[u8], out: &mut Vec<u8>) -> bool;
      pub fn open<'a>(&self, datagram: &[u8], now_ms: u64, out: &'a mut Vec<u8>) -> Opened<'a>;
      pub fn tick(&self, now_ms: u64, send: &mut dyn FnMut(&[u8]));
      pub fn security(&self) -> LinkSecurity;
      pub fn handshake_heard_at(&self) -> Option<u64>;  // the last valid handshake 1
  }
  ```
  `Channel` is `Send + Sync`, shared through `Arc`.

Tests drive two channels through a test helper `Wire` that passes `tick` and `Reply` output between them, with chosen drops, delays, and reordering. No sockets, no sleeping.

- [ ] **Step 1: Write failing tests:**
  - `two_paired_channels_connect_and_carry_data`
  - `nothing_is_sealed_before_a_session`
  - `an_unpaired_channel_never_sends_or_answers`
  - `a_stranger_key_gets_no_reply`
  - `handshakes_retry_every_250ms_then_every_second` (the 10-second change and the jitter bound)
  - `lost_handshakes_at_30_percent_still_connect` (seeded, 1000 runs, 95% within 2 seconds)
  - `both_starting_at_once_works_both_ways`
  - `a_restarted_peer_is_taken_without_losing_the_old_session_first`
  - `a_replayed_handshake_leaves_current_alone`
  - `a_handshake_for_another_channel_is_refused`
  - `packets_reflected_to_their_sender_are_refused`
  - `changing_any_byte_of_a_data_packet_refuses_it`
  - `data_across_a_key_rotation_is_not_lost` (rotation at 10 minutes, in-flight data opens on `previous`)
  - `a_session_is_dropped_at_one_hour`
  - `handshake_answers_are_limited_to_10_a_second`
  - `reordered_and_repeated_packets_open_once_each`
  - `an_old_packet_is_reported_as_old_peer`
  - `any_bytes_are_dropped_without_panic` (seeded random, every length 0 to 1500, plus each valid packet cut at every length)
  - `waking_after_hours_starts_a_new_session` (`now_ms` jumps 6 hours)
  - `a_peer_with_a_new_key_never_shows_encrypted`
  - `sealing_during_a_session_change_never_repeats_a_nonce` (threads sealing while another rotates, every `(index, counter)` pair unique)
- [ ] **Step 2: Run** `cargo test secure::datagram`. Expected: compile failure.
- [ ] **Step 3: Implement** the packets and rules in the spec's "UDP channels" section. Use `HandshakeState` and `into_stateless_transport_mode`. Hold `current` as `Arc<Session>` in a `Mutex<Slots>`, with the send counter an `AtomicU64` and `ReplayWindow` in a `Mutex` inside `Session`. Noise work happens outside the slots lock. Session indexes are random `u32`s, carried in the Noise payloads.
- [ ] **Step 4: Run** `cargo test secure::datagram`. Expected: pass.
- [ ] **Step 5: Add an ignored test** `seal_and_open_timing`, printing µs per packet at 40, 160, and 1400 bytes. Run with `cargo test --release secure::datagram::tests::seal_and_open_timing -- --ignored --nocapture`.
- [ ] **Step 6: Commit.**

### Task 4: TCP stream

**Files:**
- Create: `src/secure/stream.rs`

**Interfaces:**
- Consumes: `Pairing`, `NOISE_IK`.
- Produces:
  ```rust
  pub const MAX_PLAIN: usize = 65519;
  pub enum Purpose { Clipboard = 1, Api = 2 }
  pub trait Wire: Read + Write + Send {
      fn end_writing(&mut self) -> io::Result<()>;
      fn set_read_wait(&mut self, wait: Option<Duration>) -> io::Result<()>;
  }
  impl Wire for TcpStream {}
  impl Wire for SecureStream {}
  impl SecureStream {
      pub fn connect(target: SocketAddr, pairing: &Pairing, purpose: Purpose, wait: Duration) -> Result<SecureStream, String>;
      pub fn accept(tcp: TcpStream, pairing: &Pairing, wait: Duration) -> Result<(SecureStream, Purpose), String>;
  }
  ```

- [ ] **Step 1: Write failing tests, over loopback:** `bytes_arrive_intact_across_many_frames` (5 MB, reads and writes of odd sizes), `the_end_frame_reads_as_eof`, `a_connection_cut_without_an_end_frame_is_an_error`, `a_changed_frame_is_an_error`, `a_stranger_key_is_closed_without_a_reply`, `a_silent_connection_times_out_in_the_wait_given`, `an_oversized_length_is_an_error`, `a_read_timeout_keeps_partial_frames` (a timed-out read loses no bytes and returns `WouldBlock` or `TimedOut`).
- [ ] **Step 2: Run** `cargo test secure::stream`. Expected: compile failure.
- [ ] **Step 3: Implement** the spec's "TCP" framing with `snow::TransportState`. Keep a read buffer so a partly read frame survives a timeout.
- [ ] **Step 4: Run** `cargo test secure::stream`. Expected: pass.
- [ ] **Step 5: Commit.**

### Task 5: Pairing exchange

**Files:**
- Create: `src/secure/pairing.rs`

**Interfaces:**
- Consumes: `Pairing`, `PeerKey`.
- Produces:
  ```rust
  pub const PAIRING_PORT: u16 = 24853;
  pub const CODE_SECONDS: u64 = 120;
  pub enum PairError { WrongCode, Expired, Unreachable(String), Failed(String) }
  pub fn new_code() -> String;  // exactly 6 digits, uniform, system random
  pub fn show(code: &str, me: &Pairing, my_name: &str, listener: TcpListener, only_from: Option<IpAddr>, until: Instant)
      -> Result<(PeerKey, IpAddr), PairError>;
  pub fn enter(code: &str, me: &Pairing, my_name: &str, target: SocketAddr) -> Result<PeerKey, PairError>;
  ```

- [ ] **Step 1: Write failing tests, over loopback:** `a_right_code_swaps_keys_and_names`, `a_wrong_code_fails_on_both_sides_and_saves_nothing`, `the_first_try_uses_up_the_code` (`show` returns after one failed attempt), `an_address_other_than_the_peer_is_ignored`, `nobody_connecting_expires`, `a_lost_ok_fails_the_entering_side_and_pairing_again_works`, `a_stranger_that_sends_junk_uses_up_the_code_without_panic`, `codes_are_six_digits_with_leading_zeros_kept`.
- [ ] **Step 2: Run** `cargo test secure::pairing`. Expected: compile failure.
- [ ] **Step 3: Implement** the spec's exchange: `Spake2::<Ed25519Group>::start_a` and `start_b`, then `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` with both SPAKE2 messages in the prologue. Reject peer names over 64 bytes.
- [ ] **Step 4: Run** `cargo test secure::pairing`. Expected: pass.
- [ ] **Step 5: Commit.**

### Task 6: Input on the channel

**Files:**
- Modify: `src/protocol.rs`, `src/input/linux.rs`, `src/input/windows.rs`, `src/input/live.rs`, `src/input/link.rs`, `src/main.rs`, `tests/netem_link.py`

**Interfaces:**
- Consumes: `Channel`, `Opened`, `OVERHEAD`.
- Produces:
  ```rust
  pub const MAX_DATAGRAM: usize = MAX_PACKET_SIZE + secure::datagram::OVERHEAD;  // protocol.rs
  impl PeerLink { pub fn handshake_heard(&mut self, now_ms: u64); }
  ```
  `secure::services()` in `secure/mod.rs` returns `&'static Services { pairing: Option<Arc<Pairing>>, input: Arc<Channel>, audio_out: Arc<Channel>, audio_in: Arc<Channel> }`, built once at start.

- [ ] **Step 1: Write failing test in `link.rs`:** `a_heard_handshake_alone_says_one_way` (with no session, `handshake_heard` makes `one_way` true, with no round trip).
- [ ] **Step 2: Run** `cargo test input::link`. Expected: compile failure.
- [ ] **Step 3: Implement `handshake_heard`.**
- [ ] **Step 4: Wrap the loops.** In `linux.rs`, `windows.rs`, and `live.rs`, the receive path is: check `from_peer`, call `open`, send any `Reply`, turn `OldPeer` into `link.unreadable`, and only on `Data` decode, route `PeerAlive`, and carry on as before. Every send goes through `seal`. Call `tick` once per loop. Call `link.handshake_heard` when `handshake_heard_at` moves. Socket buffers become `MAX_DATAGRAM`. `live.rs` reads its pairing from `MK_PAIRING`.
- [ ] **Step 5: Update `tests/netem_link.py`:** `Device` writes a matching pair of `pairing.json` files, made with the `cryptography` package, and passes `MK_PAIRING`. In `one_way_from_the_start`, wait for `link.one_way` in place of `online`, and keep the other checks as the spec says.
- [ ] **Step 6: Run** `cargo test --release --no-run`, then `unshare -rnpf --kill-child --mount-proc python3 tests/netem_link.py <binary>` with every scene. Expected: every check passes, including `restart`, `restart-receiver`, `flapping`, and `lossy`.
- [ ] **Step 7: Commit.**

### Task 7: Audio on the channel

**Files:**
- Modify: `src/audio/mod.rs`, `src/audio/sender.rs`, `src/audio/live.rs`, `src/main.rs`, `tests/audio_devices.py`

**Interfaces:**
- Consumes: `Services.audio_out` (`Role::Initiator`), `Services.audio_in` (`Role::Responder`).
- Produces: `start_audio_listener(port, peer, channel: Arc<Channel>)`, `start_audio_sender(target, channel: Arc<Channel>)`, `receive_loop(socket, stream, peer, channel, stop)`, `send_loop(socket, target, channel, hub, stop)`.

- [ ] **Step 1: Update the tests in `audio/mod.rs` and `sender.rs`** that send raw packets so they go through a paired `Channel`, and add `a_plain_audio_packet_is_not_played_and_says_old_version` and `reports_only_count_from_the_session`.
- [ ] **Step 2: Run** `cargo test audio`. Expected: compile failure.
- [ ] **Step 3: Implement.** `read_reports` opens each datagram, its buffer grows to 256, and it answers handshakes. Call `tick` every pass of both loops, including when nothing is being sent, so the session is ready before the first sound. `OldPeer` sets `OTHER_VERSION_AT`.
- [ ] **Step 4: Run** `cargo test audio`, then `tests/audio_devices.py` as its header says, with `MK_PAIRING` set by the harness. Expected: pass.
- [ ] **Step 5: Commit.**

### Task 8: Control listener and clipboard

**Files:**
- Create: `src/secure/control.rs`
- Modify: `src/clipboard.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `SecureStream`, `Purpose`, `Wire`.
- Produces:
  ```rust
  pub type Incoming = Arc<dyn Fn(SecureStream, Purpose) + Send + Sync>;
  pub fn start(port: u16, peer: IpAddr, pairing: Arc<Pairing>, incoming: Incoming) -> Result<(), String>;
  pub fn open(target: SocketAddr, purpose: Purpose, wait: Duration) -> Result<SecureStream, String>;
  ```
  `clipboard::take_incoming(stream: SecureStream)` is what `control` hands clipboard connections to. `finish`, `receive`, `take_copy`, and `read_answer` become generic over `Wire`.

- [ ] **Step 1: Write failing tests:** in `control.rs`, `only_the_peer_address_is_read`, `a_fifth_silent_connection_is_closed_at_once`, `a_stranger_key_gets_no_bytes`. In `clipboard.rs`, `text_goes_through_a_secure_stream` and `a_transfer_cut_off_is_not_taken` (cut mid-file: answer failed, no batch on the clipboard, no complete folder in the inbox).
- [ ] **Step 2: Run** `cargo test clipboard` and `cargo test secure::control`. Expected: compile failure.
- [ ] **Step 3: Implement.** The existing plain-TCP clipboard tests keep working through `Wire`, with a `#[cfg(test)]` `connect` where they need it. In the product, `connect` uses `control::open(.., Purpose::Clipboard, CONNECT_WAIT)`. The bind and peer check in `start_clipboard_sync` move to `control::start`.
- [ ] **Step 4: Run** `cargo test`. Expected: pass.
- [ ] **Step 5: Commit.**

### Task 9: Peer API and local-only port

**Files:**
- Modify: `src/secure/control.rs`, `src/webui.rs`, `src/spatial.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `control::open`, `Purpose::Api`, `webui::Handler`.
- Produces:
  ```rust
  pub enum Source { Local, Browser, Peer }           // webui.rs, replaces `source` and `from_browser`
  pub fn peer_call(method: &str, path: &str, body: &str) -> Result<(u16, String), String>;  // control.rs
  pub fn serve_api(stream: SecureStream, handler: Handler);                                 // control.rs
  ```
  `HttpRequest.from_app_here` stays, true only for `Local`, so `spatial.rs` callers don't change. `HttpRequest::for_test` takes a `Source`.

- [ ] **Step 1: Write failing tests:** in `webui.rs`, `a_peer_request_may_change_state_and_a_browser_may_not`, `a_peer_request_cannot_use_the_proxy`, `a_wrong_host_is_refused` (`Host: evil.example` on GET, `127.0.0.1:24880` and `localhost:24880` allowed), `the_port_is_bound_to_loopback_only`. In `control.rs`, `calls_share_one_connection`, `a_dropped_connection_reconnects_on_the_next_call`, `a_body_over_4_mib_is_refused`, `a_connection_is_replaced_after_an_hour`. Update the `for_test` calls in `spatial.rs`.
- [ ] **Step 2: Run** `cargo test webui` and `cargo test secure::control`. Expected: compile failure.
- [ ] **Step 3: Implement** the spec's "Peer API" section. Keep today's waits: 350 ms to connect, 1200 ms for an answer. `pull_layout` in `spatial::start_sync` and the `/api/peer` branch of `handle_conn` call `peer_call`. Wire `control::start` in `main.rs`, handing `Purpose::Api` to `serve_api`. It needs a peer, so it starts next to `start_sync`, after the `chosen` check.
- [ ] **Step 4: Run** `cargo test` and `cargo test client`. Expected: pass. `tests/fake_service.py` still serves the app unchanged.
- [ ] **Step 5: Commit.**

### Task 10: Pairing service, API, and command line

**Files:**
- Modify: `src/secure/pairing.rs`, `src/spatial.rs`, `src/settings.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `show`, `enter`, `Pairing`, `Source`, `settings` restart.
- Produces:
  ```rust
  impl PairingService {   // pairing.rs, one per service
      pub fn status_json(&self) -> String;   // state, code, seconds_left, error, peer_name, fingerprint
      pub fn show_code(&self) -> Result<(), String>;
      pub fn enter_code(&self, code: &str) -> Result<(), String>;
      pub fn cancel(&self);
      pub fn unpair(&self) -> Result<(), String>;
  }
  ```
  `GET /api/pairing`, actions `pair_show`, `pair_enter`, `pair_cancel`, `unpair`, `/api/local` fields `secure` and `old_peer`, and flags `--pair-show` and `--pair <code>`, as in the spec.

- [ ] **Step 1: Write failing tests:** `the_code_is_never_in_the_status_the_peer_can_read` (`/api/local` JSON holds no code while showing), `pairing_is_refused_from_the_peer` (`pair_show`, `pair_enter`, `pair_cancel`, `unpair`, and `GET /api/pairing` from `Source::Peer`), `a_finished_pairing_saves_and_asks_for_a_restart`, `the_showing_device_learns_the_address_unless_pinned`, `unpair_removes_the_peer_and_keeps_the_key_pair`, `a_damaged_file_is_not_replaced_by_status_or_unpair`, `a_second_show_replaces_the_first_code`, `a_port_in_use_is_reported`, `unpair_tells_the_peer_if_it_can` (through `peer_call` with a `peer_unpaired` action, the one pairing action `Source::Peer` may use, which removes that peer here; failure to reach it still unpairs).
- [ ] **Step 2: Run** `cargo test secure::pairing` and `cargo test spatial`. Expected: compile failure.
- [ ] **Step 3: Implement.** Pairing runs on its own thread. It works whether or not a peer address is known, so it is set up before the `chosen` check in `main.rs`. On success, save the file, set the address where the spec says, and use the `settings` restart. `--pair-show` prints the code and waits for the result, and `--pair` prints the result, both through the local API.
- [ ] **Step 4: Run** `cargo test`. Expected: pass.
- [ ] **Step 5: Commit.**

### Task 11: App window

**Files:**
- Create: `src/app/pairing.rs`
- Modify: `src/app/mod.rs`, `src/app/ui.rs` (settings menu, notice, one call in `show`), `src/app/client.rs` (poll `/api/pairing`, send actions), `src/app/model.rs` (`status_message`)

**Interfaces:**
- Consumes: `GET /api/pairing`, the four actions, `secure` and `old_peer`.
- Produces: `pairing::window(ctx, &PairingView, &mut PairingInput) -> Option<PairingRequest>`, pure of network calls so that it can be tested as `ui.rs` tests do.

- [ ] **Step 1: Write failing tests,** in the `drawn_text` style: `an_unpaired_device_offers_both_choices`, `showing_draws_the_code_and_time_left`, `a_wrong_code_says_it_cannot_be_used_again`, `the_settings_menu_shows_the_fingerprint_and_unpair_when_paired`, `an_old_peer_shows_the_update_notice`, `the_window_draws_at_the_smallest_size`, and `the_code_box_takes_digits_only_with_or_without_a_space`.
- [ ] **Step 2: Run** `cargo test app`. Expected: compile failure.
- [ ] **Step 3: Implement** using today's theme, with the copy from the boards `ChosenPairShow` and `ChosenPairEnter`, and "Not paired yet. Keyboard, mouse, audio and clipboard stay on each device." in the notice.
- [ ] **Step 4: Run** `cargo test app` and the Windows check. Expected: pass.
- [ ] **Step 5: Commit.**

### Task 12: Independent Python peer and Wine test

**Files:**
- Create: `tests/noise_peer.py`
- Modify: `tests/wine_link.py`

**Interfaces:**
- Produces, in `noise_peer.py`: `Keys.make()`, `Keys.write_pairing(path, peer_public, name)`, `Datagrams(keys, peer_public, label)` with `handshake()`, `seal(bytes)`, `open(bytes)`, and `Stream(sock, keys, peer_public, purpose)` with `send(bytes)`, `recv()`, `close()`.

- [ ] **Step 1: Write `noise_peer.py`** from the Noise specification, on `cryptography` and `hashlib.blake2s`, with no Noise library. Include a `__main__` self-test against the published `Noise_IK_25519_ChaChaPoly_BLAKE2s` test vector from the cacophony or snow vectors file.
- [ ] **Step 2: Run** `python3 -I tests/noise_peer.py`. Expected: vector passes.
- [ ] **Step 3: Change `wine_link.py`:** the fake peer's `send` and receive go through `Datagrams`, its API calls through `Stream` with `Purpose::Api` framing, and the exe gets a pairing file in its Wine profile. Plain packets become the "refused" cases: step a, the "version this one can't read" case, and the audio `MKVA` case now assert no effect. Add a step running a real pairing between the Linux build's `--pair-show` and the exe's `--pair`.
- [ ] **Step 4: Run** the unit tests under Wine: `cargo test --target x86_64-pc-windows-gnu --no-run`, then `wine <test exe> secure::`. Expected: pass.
- [ ] **Step 5: Run** `unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_link.py <exe>'`. Expected: every step passes.
- [ ] **Step 6: Commit.**

### Task 13: Capture test, latency, and release notes

**Files:**
- Create: `tests/capture_link.py`
- Modify: `Cargo.toml` (version `0.7.0`), `Cargo.lock`, `README.md`, `packaging/` if it names the version

**Interfaces:**
- Consumes: `noise_peer.py`, `whole_device`, the `--pair` flags.

- [ ] **Step 1: Write `tests/capture_link.py`** as the spec's "Testing" section says, in a network namespace pair with a veth. The sniffer is `socket.AF_PACKET`. Markers are a clipboard string, a run of key events as their exact bincode bytes, and an API path. To prove the sniffer works, it first sees the same markers in a plaintext run through a throwaway UDP echo. The refusal half assigns the peer's IP in a third namespace, sends plaintext `MKVM`, a handshake from a wrong key, and a TCP connection to 24852, 24853, and 24880.
- [ ] **Step 2: Run** `unshare -rnpf --kill-child --mount-proc python3 tests/capture_link.py <binary>`. Expected: no markers in the capture, and no reply to the stranger.
- [ ] **Step 3: Measure latency.** Build the v0.6.0 test binary from tag `v0.6.0` in a temporary worktree, and run the `clean` and `mixed` scenes three times on each. Record median delivery times and the Task 3 timing in the plan's results section below. Expected: added median on `clean` is at most 0.1 ms.
- [ ] **Step 4: Update `README.md`** with pairing, the upgrade steps, and what is and isn't protected. Set the version to `0.7.0`. Don't tag.
- [ ] **Step 5: Run** all of it once more: `cargo test`, the Windows check, Linux and Windows release builds, and all four scripts. Expected: pass, no warnings.
- [ ] **Step 6: Commit.**

## Results

Filled in by Task 13: seal and open timing, and v0.6.0 against 0.7.0 delivery times.
