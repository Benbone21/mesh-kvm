# mesh-kvm: two-way input, native app, and role toggles

Date: 2026-11-05
Status: awaiting review

## Goal

Turn mesh-kvm from a one-way, Debian-and-Windows-specific tool into a two-way
tool with a real app on each device.

Success means:

- The laptop's trackpad can cross into Debian, and the desk keyboard can type on
  the laptop.
- A native app on both devices replaces the web UI. No browser is involved.
- The app sets which device is main and what each device shares.
- Losing the link always gives each device back its own keyboard and mouse.
- Traffic between devices is encrypted, and no control port is open to the network.

## Scope

In scope: two devices, with data structures and routing written around generic
device IDs so more can be added later.

Not in scope for this version: a third device, automatic discovery (peer address
is entered by hand), macOS, and control of the Windows lock screen or UAC prompts.

## Current problems this fixes

- `windows.rs` has a keyboard hook but no mouse hook. It checks screen edges only
  for movement arriving from Debian, so the trackpad can never cross.
- `linux.rs` reads local keyboards only for hotkeys. Typing on one while Windows
  has focus switches focus back to Debian instead of typing on the laptop.
- Windows cannot inject keys at all.
- Focus logic is written twice, once per platform, with 62 hard-coded node references.
- Input, audio, and clipboard travel unencrypted, and port 24880 accepts
  unauthenticated commands from the whole network.

## Architecture

One `mesh-kvm` program per device, run in two modes:

- `mesh-kvm`: background service. Owns input, audio, clipboard, and the link.
- `mesh-kvm --app`: window and tray icon. Holds no state of its own.

| Module | Responsibility | Depends on |
|---|---|---|
| `router.rs` | Pure focus state machine: active device, crossing, game lock, link loss | Nothing platform-specific |
| `input/mod.rs` | `InputBackend` trait: capture, block, inject, cursor position, held-input release | |
| `input/linux.rs` | evdev capture and grab, uinput injection, KWin cursor tracking | `router` events |
| `input/windows.rs` | Low-level mouse and keyboard hooks, `SendInput` | `router` events |
| `link.rs` | Pairing, encryption, UDP input and audio, TCP control channel | |
| `config.rs` | Devices, main device, toggles, layout, migration from `layout.json` | |
| `api.rs` | Local-only API between app and service | `config`, `router` |
| `app/` | egui window and tray | `api.rs` client only |

`router.rs` takes events (local input, peer packet, link timeout, toggle change)
and returns actions (block input, forward, inject, switch, release held input). It
performs no I/O, so all focus behaviour can be unit tested on Linux.

`spatial.rs` is split into `config.rs` and `router.rs`. `webui.rs`, `src/ui/`, and
`mesh-kvm-ui.py` are removed in the final stage.

## Input routing

Definitions:

- **Active device:** the one receiving keyboard and mouse input. Exactly one at a time.
- **Shared input:** physical input on a device with that input's sharing toggle on.

Rules:

1. Shared input always goes to the active device. If that is the other device, it
   is blocked locally and forwarded.
2. Input that is not shared always stays local, and it never changes the active device.
3. Typing never changes the active device.
4. The active device checks screen edges, whichever device's mouse moved the
   cursor. It switches when the cursor pushes past `edge_resistance` on an edge
   that touches the other device, no button is held, and no game lock applies.
5. The new active device places the cursor at the matching layout position.
6. A game lock on the active device blocks crossing. Automatic detection and the
   manual lock work as they do today, on both devices.
7. On every switch, the device losing focus releases anything injected and still held.

Pointer speed is set per source device and applies to forwarded movement. Debian's
current `mouse_speed` of 0.35 becomes its value. The laptop's starts at 1.0.

### Windows additions

- `WH_MOUSE_LL` hook. It ignores injected events. While forwarding, it blocks
  movement and sends the offset from a pinned cursor position.
- Key injection through `SendInput` with `KEYEVENTF_SCANCODE`. The existing hook
  already ignores injected keys, so no feedback loop results.
- The hooks stay on the existing message-loop thread and use a non-blocking socket,
  so they cannot hit the Windows hook timeout.

### Debian additions

- Local keyboards are grabbed and forwarded while the laptop is active. Hotkeys
  keep working because they are read from the same evdev stream.
- A relative uinput mouse for forwarded movement, alongside the existing absolute one.
- The rule that typing returns focus to Debian is removed.

### Key mapping

`keymap.rs` gains evdev-to-Windows mapping. A unit test checks that every mapped
key round-trips in both directions.

## Roles and toggles

Stored per device:

- `mouse_sharing`, `keyboard_sharing`
- `audio_send`, `audio_receive`
- `clipboard_send`, `clipboard_receive`
- `pointer_speed`, `autostart`

Stored once, shared by both devices:

- `main_device`
- Layout: each device's displays in one coordinate space, and `edge_resistance`
- `auto_game_lock`

Main device behaviour:

- It holds the authoritative shared settings. Changes made on the other device are
  sent to it and returned to both.
- When the link comes up, the main device becomes active.
- If it is offline, the other device runs from its last copy. Shared changes are
  disabled in the app until the link returns. Per-device toggles still work.
- Changing the main device applies to both devices immediately and needs the link
  to be up.

`layout.json` is migrated on first run, with Debian as main and all sharing on.

## Link and security

Pairing:

- The app on one device shows a 6-digit code, and the user enters it on the other.
- SPAKE2 turns the code into a shared 32-byte key, so a captured exchange can't be
  used to guess the code offline. The key is stored in a user-only config file.
- Unpairing removes the key from that device.

Transport:

| Channel | Port | Carries |
|---|---|---|
| UDP | 24850 | Input and heartbeats |
| UDP | 24851 | Audio |
| TCP | 24852 | Clipboard, settings, and peer status |

- All three use ChaCha20-Poly1305 with per-direction keys derived from the shared key.
- UDP packets carry a counter used as the nonce, and a replay window rejects old
  counters. The window resets when the link is re-established.
- Packets that fail authentication are dropped and do not count as link activity.
- The new format is not compatible with the old one, so both devices must be updated
  together when this stage ships.

Local API:

- `api.rs` listens on 127.0.0.1 only.
- Every request needs a random token read from a user-only file. A website cannot
  read that file, which blocks browser-based requests.
- Port 24880 stops listening on the network.

## Native app

Built with egui through `eframe`, so it needs no web view.

- Status bar: both devices, which is active, and a switch control.
- Screen map: drag to arrange, as in the current Studio.
- Per-device panels: all toggles, audio levels and volumes, and start at boot.
- Settings: main device, pairing, peer address, and game lock.
- It shows peer data over the control channel, so each app controls both devices.

The tray uses `ksni` on Linux, because it works with KDE over D-Bus with no extra
libraries, and `tray-icon` on Windows. Its menu has Open, Switch device, Release
all input, and Quit. Closing the window leaves the tray running, and sharing runs
in the service either way. Quit closes the app but not the service.

The app starts at login with the tray only. A Start Menu entry and a Linux
`.desktop` file open the window. Opening the app while it is already running
focuses the existing window.

## Failure handling

- **Link loss:** no authenticated packet for 1.35 seconds, on a monotonic clock.
  Each device becomes active for itself, stops blocking input, and
  releases injected keys and buttons. This is decided in `router.rs`, so it is the
  same in both directions.
- **Escape hotkey:** Ctrl+Alt+F12 works locally on both devices with no link, and
  stops all forwarding until the next manual switch.
- **Service crash:** Linux grabs end with the process, and Windows hooks are removed
  with it, so local input returns.
- **App crash or closed:** no effect on sharing.
- **Bad or missing config:** start with local input only and show the problem in
  the app. Never start in a forwarding state.
- **Unpaired:** everything stays local.
- **Lock screen and UAC prompts:** Windows ignores injected input there, so the
  laptop's own keyboard must be used. The service does not try to work around this.

## Testing

Automated:

- `router.rs` unit tests: both crossing directions, each toggle combination, game
  lock, held buttons, link loss during forwarding, and reconnection to the main device.
- `keymap.rs` round-trip test.
- `link.rs` tests: pairing, wrong code, tampered packet, replayed packet, and
  counter reset after reconnecting.
- `config.rs` test: `layout.json` migration.

Under Wine in an isolated network namespace: the Windows service, link drop
recovery, and encrypted packets from a test peer.

On real hardware, by the user:

- Trackpad crossing to Debian and back
- Desk keyboard typing on the laptop
- Leaving Wi-Fi during forwarding, in both directions
- Game lock
- Tray on KDE and Windows 11

## Build stages

Each stage is shipped and working before the next starts.

1. **Two-way input.** Add `router.rs` and the input backends, on the existing
   protocol and web UI. Toggles use config defaults. This fixes the stuck laptop mouse.
2. **Secure link and settings.** Add `link.rs`, `config.rs`, and `api.rs`, with
   pairing done from a temporary `--pair` command. Both devices update together.
3. **Native app.** Add `app/` and the tray, move pairing into the app, then remove
   the web UI, its port, and `mesh-kvm-ui.py`.
