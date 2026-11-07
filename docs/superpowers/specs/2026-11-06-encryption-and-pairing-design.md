# mesh-kvm: encryption and device pairing

Date: 2026-11-06
Status: awaiting review
Replaces: "Link and security" in `2026-11-05-universal-mesh-design.md`

## Goal

Everything between the two devices is encrypted and authenticated. A device that
merely has the right IP address gets nothing.

Success means:

- Input, audio, clipboard, files, and the peer API are encrypted and authenticated.
- Devices pair once with a 6-digit code shown in the app.
- No control port answers an unpaired device, and port 24880 leaves the network.
- Behaviour on bad Wi-Fi from v0.5 and v0.6 does not regress.
- Upgrading from v0.6 is clear, with both devices updated together.

## Threat model

Protected against someone on the same LAN or Wi-Fi who can read, inject, replay,
reorder, or drop packets and who can use the peer's IP address.

Not protected:

- Packet sizes and timing, which still show when someone is typing.
- Jamming or flooding the link.
- A compromised device, or malware running as the same user.
- Other programs on the same device reaching 127.0.0.1:24880.

## Decisions

| Question | Decision | Why |
|---|---|---|
| Handshake | `Noise_IK_25519_ChaChaPoly_BLAKE2s` | One round trip, fresh keys each session, the pattern WireGuard builds on |
| Pairing | SPAKE2 from the code, then `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` | A captured exchange can't be used to guess the code offline, and no hand-made key exchange |
| Long-term keys | One X25519 key pair per device | Reflected packets fail without extra rules, and a third device fits later |
| Sessions | One per channel | Input, audio, and TCP share no state, so no locks or counters cross threads |
| Local API token | Left for later | The goals don't need it, and it touches `src/app/client.rs`. A `Host` check is added instead |
| Peer address | Still entered by hand, and still checked | Roaming to a new address is a separate change |

The 2026-11-05 spec derived channel keys from one stored key and reset the counter on
reconnect. That reuses nonces under the same key, which breaks ChaCha20-Poly1305, so
it is dropped. Keys now come from a fresh handshake every session.

## Crates

```toml
snow = { version = "0.10", default-features = false, features = [
    "default-resolver", "use-curve25519", "use-chacha20poly1305", "use-blake2", "use-getrandom",
] }
spake2 = "0.4"
```

These features leave out `ring` and AES, so all of it is pure Rust. A throwaway build
with them ran on Linux and under Wine for `x86_64-pc-windows-gnu`, with no cmake.

`snow` and `spake2` each say in their README that they have had no formal audit.
`chacha20poly1305` has had one, by NCC Group, with no significant findings.

## Modules

| Module | Does | Depends on |
|---|---|---|
| `secure/keys.rs` | Loads and saves `pairing.json`, makes the fingerprint | `settings::config_dir` |
| `secure/window.rs` | Replay window | Nothing |
| `secure/datagram.rs` | `Channel`: sessions, handshakes, sealing and opening UDP packets | `snow`, `window`, `keys` |
| `secure/stream.rs` | `SecureStream`: an encrypted `Read + Write` over `TcpStream` | `snow`, `keys` |
| `secure/control.rs` | The listener on 24852, and clipboard and API connections to the peer | `stream` |
| `secure/pairing.rs` | The pairing exchange and its time-limited listener | `spake2`, `snow`, `keys` |

Time comes in as `now_ms`, as in `link.rs`, so all of it can be tested without
sleeping. `router.rs` does not change, and `link.rs` gains only `handshake_heard`, for the
one-way note below: they see the same `Packet` values they see today, only fewer forged ones.

Channels are built when the service starts, so pairing or unpairing restarts the service,
through the same `settings` restart that a new peer address uses.

## Pairing

### What the person does

1. Update both devices. Each now shows "Not paired", and everything stays local.
2. Choose "Show a code" on one device. The other device's address must already be set
   on the device that enters the code, as it is today.
3. Choose "Enter a code" on the other device and type it on that device's own keyboard.
4. Both devices show the same fingerprint, and the link comes up.

### The exchange

The device showing the code makes 6 random digits from the system's random source and
opens TCP 24853 for 120 seconds. If it has a peer address, it accepts only that one.

Frames are a big-endian `u16` length and then the bytes.

1. The entering device sends its SPAKE2 message, from `start_b` with identity
   `mesh-kvm enter`. The showing device answers from `start_a`, with identity
   `mesh-kvm show`. The password is the 6 digits.
2. Both run `Noise_XXpsk3` over the same connection, with the 32-byte SPAKE2 key as the
   PSK, their long-term keys as the static keys, and both SPAKE2 messages in the
   prologue. The payloads carry device names. The entering device initiates.
3. The showing device checks message 3, saves the pairing, sends an encrypted `ok`, and
   closes. The entering device saves on reading `ok`.

The first connection that starts the exchange uses up the code, whether or not it
succeeds, and 24853 closes. An attacker therefore gets one guess in a million per
code, and the person sees that the code failed. If the `ok` is lost, one device is
paired and the other reports failure. Pairing again replaces the old pairing on both.

If the showing device had no peer address, it saves the address the pairing came from,
unless the address is pinned.

### Stored data

`pairing.json` in `settings::config_dir()`:

```json
{ "version": 1, "private_key": "<hex>", "public_key": "<hex>",
  "peer": { "public_key": "<hex>", "name": "Laptop", "paired_at": 1794018000 } }
```

- It is written to a temporary file and renamed. On Linux that file is created with
  mode 0600. On Windows it relies on the user-only default of `%LOCALAPPDATA%`.
- The key pair is made the first time it is needed, and survives unpairing.
- A file that can't be read counts as unpaired and shows a problem in the app. It is
  never overwritten without an explicit new pairing.

The fingerprint is the first 4 bytes of BLAKE2s over a fixed label and both public keys
in sorted order, shown as `4F2A-91C3`. It is the same on both devices, and is only there
to check by eye. Security doesn't rest on it.

Unpairing removes `peer` on this device and tells the other device over the control
channel if it can. If it can't, the other device's handshakes are simply refused.

## UDP channels

Input (24850) and audio (24851) each run a `Channel`.

```rust
impl Channel {
    /// Encrypts for the peer. False with no session yet: the packet is dropped, as a lost one would be.
    pub fn seal(&self, plain: &[u8], out: &mut Vec<u8>) -> bool;
    /// Takes in a datagram. Only `Opened::Data` is for the caller.
    pub fn open<'a>(&self, datagram: &[u8], now_ms: u64, out: &'a mut Vec<u8>) -> Opened<'a>;
    /// Handshake packets due now, from the loop that owns the socket.
    pub fn tick(&self, now_ms: u64, send: &mut dyn FnMut(&[u8]));
}
```

### Packets

| First byte | Kind | Rest |
|---|---|---|
| `0xE1` | Handshake 1 | Noise message, carrying the sender's 4-byte session index |
| `0xE2` | Handshake 2 | The index it answers, then the Noise message, carrying the sender's index |
| `0xE3` | Data | The receiver's session index (4), counter (8), ciphertext and tag |

- Data adds 29 bytes: 13 of header and a 16-byte tag. The largest input packet becomes
  1429 bytes and the largest audio packet 1316, so neither fragments at 1472.
- Receive buffers grow to fit: `MAX_DATAGRAM` replaces `MAX_PACKET_SIZE` for sockets,
  and the 64-byte buffer in `read_reports` grows.
- The counter is the Noise nonce through `StatelessTransportState`, so a packet decrypts
  in any order. A changed counter or index fails decryption.
- The Noise prologue names the channel (`mesh-kvm 1 input`, `mesh-kvm 1 audio`,
  `mesh-kvm 1 tcp`), so a handshake can't move between channels.
- A zero-length data packet is a keepalive that confirms a session.

### Replay window

Each session keeps a 2048-packet sliding bitmap, as in RFC 6479. A counter below the
window, or already seen, is dropped. The bit is only set after the tag checks out.

The copies v0.6 sends of presses and reports are separately encrypted with their own
counters, so they all still get through. `link.rs` stays what removes duplicates and
puts input in order.

### Sessions

Each channel holds:

- **current:** sends and receives.
- **previous:** receives only, for 60 seconds after being replaced, so packets in flight
  across a change of keys are not lost.
- **pending:** up to 4, receive only, oldest dropped: a responder's sessions that have not
  yet seen data from the initiator.

An initiator also keeps its last 4 outstanding handshakes, oldest dropped, so an answer slower
than the retries still counts. This covers round trips above 250 ms.

Rules:

1. A device with no current session sends handshake 1 every 250 ms, with up to 50 ms of
   jitter, for 10 seconds, and then once a second.
2. On a valid handshake 2, the initiator makes the session current, sends two keepalives,
   and clears its other outstanding handshakes, so a late answer from the same burst is refused.
3. On a valid handshake 1 from the paired key, the responder adds a pending session, answers,
   and leaves current alone.
4. The first data that opens on a pending session makes it current if it was created after
   current began, and clears the other pending sessions and any outstanding handshakes.
   Data on an older pending session opens, but that session stays receive-only.
5. With `Role::Both`, a device with an outstanding handshake that receives a valid handshake 1
   compares public keys. The lower key ignores it, though it still counts as heard for the
   one-way note, and lets its own handshake win. The higher key abandons its outstanding
   handshakes and answers.
6. A device whose current session is 10 minutes old starts a new handshake and keeps
   using the old one until the new one is confirmed.
7. A session is dropped at 1 hour, so keys can't outlive a failing rotation for long.
8. Handshake 1 is answered at most 10 times a second, as each one costs key operations.
9. A device able to initiate that has sealed something since it last opened anything
   authenticated, and has heard nothing authenticated for 3 seconds, starts a handshake at
   rule 1's pace, keeping current until the new one is confirmed. The 3 seconds also count from
   the first thing sealed after a quiet spell, or that packet would start a needless handshake.
   Data and handshake 2 count as heard. Handshake 1 doesn't, as a replay is one. A handshake
   that silence started is abandoned if the peer is heard first.
10. A device with a current session that has received data and sent nothing for 1 second sends
    a keepalive from `tick`, to the last source address, which the caller's `send` knows.
    Keepalives don't trigger this or rule 9, on either side, or two idle devices would keep
    each other going.

Handshakes cross all the time: a device with only a pending session has no current one, so rule 1
has it start its own, and at rotation a responder's session is older than the initiator's by one
flight of handshake 1, so both reach 10 minutes as the first handshake lands. If each device simply
took the other's session, each would send on what the other holds as previous, and the link would
die when that ran out. Rules 4 and 5 keep them on one session, and rule 9 heals whatever those miss.

Rule 3 is what keeps a replayed handshake 1 from touching current: it can only create a pending
session that nothing can confirm. Replays can evict a real pending session, or make the higher key
abandon a handshake, and then rule 9 or the next retry repairs it, as it would a lost packet.
Audio's sender always initiates and its receiver always answers, so audio is ready before the
first sound. A receiver that restarts can't start a handshake, and rules 9 and 10 are how its sender
finds out, with or without audio reports.

### Behaviour on bad Wi-Fi

- A dropout keeps the session, so nothing needs repeating when the link returns. Sending into
  it starts handshakes after 3 seconds, and current stays in use until one is confirmed.
- A restart costs one round trip, retried every 250 ms. At 30% loss each way, about half
  of tries succeed, so this usually takes under a second. Round trips above 250 ms work too,
  as the last 4 tries are kept on both sides.
- An audio receiver's restart can't be seen from its end, so it takes up to 3 seconds of
  silence and then that round trip.
- Packets that fail any check are dropped silently and never count as link activity:
  `core.route(Event::PeerAlive)` moves after `open`.
- The hooks on Windows stay safe: `seal` takes no lock held across work, only a brief one
  to copy out an `Arc` to the session. Handshakes run on the receive thread.

### One thing that changes

`oneway-start` in `tests/netem_link.py` assumes a device can hear the other from the start.
With authentication, no session can form until both directions work, so the device
stays offline. A valid handshake 1 still shows the device is being heard, so
`link.one_way` is set from that, and the app says the same as today. A replayed handshake
could fake this note, but nothing else depends on it.

The scene is changed to match, keeping its promises: a crossing can't keep the keyboard,
the link says it's one way, and crossing works once it can hear.

## TCP

One listener on 24852 carries clipboard, files, and the peer API.

`SecureStream` runs `Noise_IK` and then sends frames of a big-endian `u16` length and
ciphertext, with up to 65519 bytes of plaintext each.

- The first frame after the handshake holds one byte saying what the connection is for:
  1 for clipboard, 2 for API.
- A zero-length frame ends the stream. EOF without one is an error, so a cut connection
  can't pass for a finished transfer.
- `clipboard.rs` swaps `TcpStream` for `SecureStream`. `finish` and `read_answer` keep
  their meaning through `shutdown`, `set_read_timeout`, and `read`.
- The listener drops any address that isn't the peer's before reading. A connection that
  isn't a valid handshake from the paired key within 3 seconds is closed without a reply.
  At most 4 unauthenticated connections are held at once.

### Peer API

- `webui::bind` moves to 127.0.0.1, so 24880 leaves the network.
- `/api/peer/...` and `spatial::start_sync` go through `secure::control::peer_call`, on one
  persistent connection, as the app polls every 200 ms. Each request is a `u32` length and
  bincode `{ method, path, body }`, and each answer the same with `{ status, body }`.
  One request is in flight at a time, and any failure drops the connection and reconnects.
- The connection is replaced after an hour, so its keys rotate too.
- `HttpRequest.source` becomes `Source::{Local, Browser, Peer}`. Requests that arrive
  from `control` are `Peer`, so an IP address no longer decides anything.
- Requests from the peer can't reach `/api/peer` or any pairing action.
- A request with a `Host` other than `127.0.0.1` or `localhost` is refused, which stops DNS
  rebinding reading status. The `Origin` check stays.

### Local API additions

- `GET /api/pairing`, local only: state, code, seconds left, last error, peer name, and
  fingerprint. It is separate from `/api/local`, because that is readable by the peer and
  the code must not be.
- `POST /api/local/action` gains `pair_show`, `pair_enter` with the code, `pair_cancel`, and
  `unpair`. All are refused from `Peer`.
- `/api/local` gains `secure: "unpaired" | "connecting" | "encrypted"` and `old_peer`.

The existing `paired` field means a peer address is set. It stays as it is for now, and the
app rebuild can rename it.

## App

Kept small, as the rebuild of the window is a separate task:

- `src/app/pairing.rs`: one window with the show, enter, and wrong-code states. Its layout
  follows the boards `ChosenPairShow` and `ChosenPairEnter` on the
  [design canvas](https://claude.ai/artifact/27pLLtNjAcohWVFfjKCCJX), in today's theme.
- A "Pair" entry, the fingerprint, and Unpair in the Settings menu.
- A notice strip for "Not paired" and for an older peer.

`mesh-kvm --pair-show` and `mesh-kvm --pair <code>` do the same through the local API, for
tests and for use without a window.

## Upgrading from v0.6

1. Install the encrypted release on both devices. Its number is set when it merges. Laptops take the new exe the same way as before.
2. Pair once. Until then each device works alone, so the code has to be typed on the
   laptop's own keyboard.

If only one device is updated:

- The new device drops the old one's plaintext. A `MKVM` or `MKAU` packet from the peer's
  address sets `old_peer` for 10 seconds, and the app says to update it. This could be
  forged, and only shows a note.
- The old device drops the new device's packets, as they don't start with its magic.

To roll back, reinstall 0.6.1 on both. It ignores `pairing.json`, and traffic is plaintext
again.

`LINK_PROTOCOL` stays at 2, as what's inside the encryption doesn't change.

## Failure handling

| Case | What happens |
|---|---|
| Not paired | No sessions, listeners refuse everything, each device stays local |
| `pairing.json` unreadable | Same, with a problem shown in the app |
| Peer unpaired or reinstalled | Its handshakes fail, and the app shows the link as down |
| Code expires or fails | 24853 closes, and the window offers a new code |
| Link loss | Unchanged: 1.35 seconds with no authenticated packet |
| Port 24853 in use | Pairing fails with a message, and nothing else is affected |

## Testing

Unit tests, in the style of each module and run on Linux and under Wine:

- `window`: reorder, replay, the edge of the window, large jumps.
- `datagram`: lost handshakes, both devices starting at once, restart, rotation with
  packets in flight, replayed handshake 1 leaving current alone, wrong key, tampering with
  every byte, packets reflected back to their sender.
- `stream`: large transfers, truncation, wrong key, a slow handshake timing out.
- `pairing`: success, wrong code, reuse of a code, expiry, a lost `ok`.
- `keys`: round trip, file mode, corrupt file.
- `webui`: `Source` rules, `Host` check, nothing pairing-related readable by the peer.

End to end:

- `tests/netem_link.py`, `tests/audio_devices.py`, and `tests/wine_link.py` pass.
  `whole_device` in `input::live` and `audio::live` read a test pairing from `MK_PAIRING`.
- `tests/noise_peer.py` is a separate Noise IK and transport written in Python on the
  `cryptography` package. `wine_link.py` uses it to speak to the exe, which also shows the
  implementation agrees with another one.
- A real pairing runs between the Linux build and the exe under Wine.
- `tests/capture_link.py` is new. `tcpdump` isn't installed, so it captures with an
  `AF_PACKET` socket across a veth pair in a network namespace, while known keystrokes,
  clipboard text, and API calls pass. It fails if any of them, or `MKVM` or `MKAU`, show up
  in the capture. It then plays an unpaired device on the peer's IP address, and fails on
  any reply over UDP, any bytes over 24852, or a connection to 24880.

### Latency

Measured and reported, not assumed:

- An ignored test times `seal` and `open` for 40, 160, and 1400 bytes. The throwaway build
  gave 2.0, 2.5, and 3.8 µs for both together, on Linux and under Wine.
- `netem_link.py` clean and mixed scenes are run on the v0.6.1 test binary and on the new
  one, comparing delivery times. Added median latency above 0.1 ms on the clean scene is
  treated as a bug.

## Not in this work

- The local API token.
- The rebuild of the window in the new design.
- Following a peer to a new address.
- More than two devices. Long-term keys leave room for it.
- Key storage in Windows DPAPI.
