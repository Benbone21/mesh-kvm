# Stage 1: Two-Way Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let either device's mouse and keyboard control either device, which frees the laptop's trackpad and lets the desk keyboard type on the laptop.

**Architecture:** A pure `router.rs` state machine replaces the focus logic written separately in `linux.rs` and `windows.rs`. Platform code becomes an `InputBackend` that reports events and applies the router's actions. The wire protocol stays unencrypted and the web UI stays until Stages 2 and 3.

**Tech Stack:** Rust 2021, `windows-sys` 0.59, `libc` evdev and uinput, bincode 1.3. Linux toolchain plus `x86_64-pc-windows-gnu` target, and Wine for Windows checks.

**Spec:** `docs/superpowers/specs/2026-11-05-universal-mesh-design.md` (this plan covers its build stage 1; stages 2 and 3 get their own plans after this one is proven on real hardware).

## Global Constraints

- Link timeout: no packet for 1.35 seconds (`LINK_TIMEOUT_MS = 1350`), measured on a monotonic clock, on both devices.
- Link loss: each device becomes active for itself, stops blocking input, and releases injected keys and buttons.
- Ctrl+Alt+F12 works locally on both devices with no link, and stops all forwarding until the next manual switch.
- Typing never changes the active device.
- Input that is not shared stays local and never changes the active device.
- Both sharing toggles default to on in this stage. There is no UI for them yet.
- Pointer speed: Debian keeps `mouse_speed` (0.35 today). The laptop's is 1.0.
- `Payload` gains variants only at the end of the enum, so bincode indexes of existing variants don't change.
- Every task must leave both `cargo test` and `cargo check --target x86_64-pc-windows-gnu` passing.
- Never run a real daemon during development. It grabs the user's input. Use unit tests, and Wine inside `unshare -rn`.
- Git has no configured identity. Commit with `git -c user.name=bread -c user.email=jj1nfz2nb@mozmail.com commit`, and end messages with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. A key or button held while focus changes: its release must go where its press went, so nothing sticks. Tested in Task 1.
2. The laptop vanishes while Debian's mouse and keyboard are grabbed and forwarding: Debian must get both back within the timeout. Tested in Task 1.
3. A manual switch on one device at the same moment the other crosses an edge: both must settle on the same active device. Tested in Task 1.
4. A cursor pressed against an edge outside the part that touches the other screen, such as past the end of a smaller monitor: it must not cross. Tested in Task 2.
5. Finishing a drag at an edge: releasing the button must not cross until the user pushes again. Tested in Task 2.

---

### Task 1: Router state machine

**Files:**
- Create: `src/router.rs` (tests in a `#[cfg(test)]` module in the same file, the project's first tests)
- Modify: `src/main.rs` (add `mod router;`)

**Interfaces:**
- Produces:

```rust
pub type DeviceId = u8;
pub const LINK_TIMEOUT_MS: u64 = 1350;
pub const SWITCH_RACE_MS: u64 = 200;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sharing { pub mouse: bool, pub keyboard: bool }

#[derive(Clone, Debug)]
pub struct RouterConfig { pub me: DeviceId, pub peer: DeviceId, pub main: DeviceId,
                          pub sharing: Sharing, pub pointer_speed: f32 }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    LocalMove { dx: i32, dy: i32 }, LocalButton { button: u8, pressed: bool },
    LocalWheel { dx: i32, dy: i32 }, LocalKey { scancode: u16, pressed: bool },
    PeerMove { dx: i32, dy: i32 }, PeerButton { button: u8, pressed: bool },
    PeerWheel { dx: i32, dy: i32 }, PeerKey { scancode: u16, pressed: bool },
    PeerAlive { now_ms: u64 },
    PeerFocus { target: DeviceId, x: i32, y: i32 },
    PeerPanic, Tick { now_ms: u64 }, ManualSwitch, Panic,
    SetCaptive(bool), EdgeCrossed { x: i32, y: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    ForwardMove { dx: i32, dy: i32 }, ForwardButton { button: u8, pressed: bool },
    ForwardWheel { dx: i32, dy: i32 }, ForwardKey { scancode: u16, pressed: bool },
    InjectMove { dx: i32, dy: i32 }, InjectButton { button: u8, pressed: bool },
    InjectWheel { dx: i32, dy: i32 }, InjectKey { scancode: u16, pressed: bool },
    SendFocus { target: DeviceId, x: i32, y: i32 }, SendPanic,
    WarpCursor { x: i32, y: i32 }, ReleaseHeld,
    SetBlocking { mouse: bool, keyboard: bool },
}

#[derive(Debug, Default, PartialEq)]
pub struct Routed { pub swallow: bool, pub actions: Vec<Action> }

impl Router {
    pub fn new(cfg: RouterConfig) -> Self;
    pub fn handle(&mut self, ev: Event) -> Routed;
    pub fn active(&self) -> DeviceId;
    pub fn online(&self) -> bool;
    pub fn panicked(&self) -> bool;
    pub fn captive(&self) -> bool;
    pub fn set_pointer_speed(&mut self, speed: f32);
}
```

`swallow` means the backend must stop this local event from reaching the local OS. `x, y` in focus events are always laptop-local screen coordinates (Debian converts them with `win_x`, `win_y`). A new router starts offline, active for itself.

- [ ] **Step 1: Write the failing tests**

Helpers: `router(me)` builds a config with Debian `0` main, Windows `1`, both sharing on, speed 1.0; `online(r)` feeds `PeerAlive { now_ms: 1000 }`.

```rust
#[test] fn starts_local_and_offline()            // active()==me, !online(), local key not swallowed, no Forward*
#[test] fn link_up_makes_main_device_active()    // router(1) gets PeerAlive: active()==0, SetBlocking{mouse:true,keyboard:true} in actions
#[test] fn shared_input_forwards_to_remote_active() // router(1) online: LocalKey -> swallow, ForwardKey; LocalMove -> swallow, ForwardMove
#[test] fn unshared_input_stays_local()          // sharing.keyboard=false: LocalKey -> !swallow, no Forward, active unchanged; same for mouse
#[test] fn typing_never_changes_active()         // router(0), active 1: LocalKey never yields SendFocus; active()==1
#[test] fn peer_input_injects_only_when_active() // PeerKey on active me -> InjectKey; on inactive me -> no actions
#[test] fn key_up_follows_key_down()             // key down forwarded, focus returns, key up -> swallow + ForwardKey{pressed:false}
#[test] fn locally_pressed_key_releases_locally()// key down local, focus leaves, key up -> !swallow, no Forward
#[test] fn button_up_follows_button_down()       // same two checks for LocalButton
#[test] fn switch_releases_held_on_losing_device()  // EdgeCrossed on active me -> ReleaseHeld and SendFocus{target:peer}
#[test] fn link_loss_restores_local_control()    // router(0) forwarding to 1; Tick{now_ms:1000+1351}: !online(), active()==0, ReleaseHeld, SetBlocking{false,false}
#[test] fn link_survives_within_timeout()        // Tick{now_ms:1000+1350}: online()
#[test] fn panic_stops_forwarding_until_manual_switch() // Panic -> SendPanic, active()==me, LocalKey not swallowed; ManualSwitch -> !panicked()
#[test] fn panic_works_offline()                 // offline router: Panic -> panicked(), no blocking
#[test] fn peer_panic_releases_input()           // PeerPanic same local effect, no SendPanic echo
#[test] fn captive_blocks_edge_crossing()        // SetCaptive(true): EdgeCrossed -> no SendFocus, active unchanged
#[test] fn manual_switch_needs_link()            // offline: ManualSwitch -> active unchanged, no SendFocus
#[test] fn conflicting_switches_settle_on_main_claim() // both sides switch within SWITCH_RACE_MS with opposite targets: main ignores peer's claim, non-main accepts main's; active() equal on both routers
#[test] fn pointer_speed_scales_with_carry()     // speed 0.5: two LocalMove{dx:1} forward a total dx of 1 (fractions carried)
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test router`
Expected: compile failure, `Router` not found.

- [ ] **Step 3: Implement `Router` in `src/router.rs`**

Track forwarded keys and buttons in `HashSet`s so a release is routed by where its press went. Track `last_switch_ms` from the newest `now_ms` for the race rule. No I/O, clocks, or platform code.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test router`
Expected: all pass.

- [ ] **Step 5: Commit** `git add src/router.rs src/main.rs`, message `Add pure focus router`.

---

### Task 2: Edge tracker

**Files:**
- Modify: `src/router.rs`

**Interfaces:**
- Consumes: `crate::protocol::SwitchEdge`
- Produces:

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeSpan { pub edge: SwitchEdge, pub start: i32, pub end: i32 }

pub struct EdgeTracker { /* accumulated push */ }
impl EdgeTracker {
    pub fn new(resistance: i32) -> Self;
    pub fn set_resistance(&mut self, resistance: i32);
    /// True when this movement completes a push across `span`.
    pub fn push(&mut self, span: EdgeSpan, x: i32, y: i32, w: i32, h: i32,
                dx: i32, dy: i32, buttons_held: bool) -> bool;
}
```

`start..=end` runs along the edge in laptop-local pixels. Used by the laptop only; Debian keeps its existing `check_debian_to_windows`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn crosses_after_resistance()      // resistance 55, Left edge, x=0: pushes dx=-30 twice -> false then true
#[test] fn moving_away_resets_push()       // push -30, then dx=+5, then -30 -> false
#[test] fn no_cross_outside_span()         // Bottom edge span 0..=500, y=h-1, x=900: never true
#[test] fn no_cross_with_button_held()     // buttons_held=true: never true
#[test] fn releasing_drag_needs_fresh_push() // pushes with button held, then one dx=-1 with button released -> false
```

- [ ] **Step 2: Run** `cargo test edge`. Expected: compile failure.
- [ ] **Step 3: Implement `EdgeTracker`.** Only outward movement while on the edge pixel accumulates; holding a button clears the accumulator.
- [ ] **Step 4: Run** `cargo test edge`. Expected: all pass.
- [ ] **Step 5: Commit** `src/router.rs`, message `Add edge push tracker`.

---

### Task 3: Evdev-to-Windows key mapping

**Files:**
- Modify: `src/keymap.rs`

**Interfaces:**
- Consumes: `win_vk_scan_to_evdev(vk: u32, scan: u32, extended: bool) -> Option<u16>`
- Produces: `pub fn evdev_to_win_scan(evdev: u16) -> Option<(u16, bool)>` returning the PS/2 set-1 scancode and whether it needs `KEYEVENTF_EXTENDEDKEY`. Compiled on all platforms so Linux can test it.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn letters_map_to_scancodes()      // KEY_A -> (0x1E,false), KEY_ENTER -> (0x1C,false)
#[test] fn extended_keys_are_flagged()     // KEY_RIGHTCTRL -> (0x1D,true), KEY_LEFT -> (0x4B,true), KEY_LEFTMETA -> (0x5B,true), KEY_KPENTER -> (0x1C,true)
#[test] fn every_key_round_trips()         // for each KEY_* in the table: win_vk_scan_to_evdev(vk_for(scan), scan, ext) == Some(key)
#[test] fn unknown_key_is_none()           // evdev_to_win_scan(0xFFFF) == None
```

Where the existing function needs a VK to resolve a key, the round-trip test supplies it from a small test-only table.

- [ ] **Step 2: Run** `cargo test keymap`. Expected: compile failure.
- [ ] **Step 3: Implement `evdev_to_win_scan`** with a `match`. For F13 to F24, and any media keys without a stable set-1 code, return `None` if the round trip cannot hold.
- [ ] **Step 4: Run** `cargo test keymap`. Expected: all pass.
- [ ] **Step 5: Commit** `src/keymap.rs`, message `Add evdev to Windows scancode mapping`.

---

### Task 4: Protocol additions

**Files:**
- Modify: `src/protocol.rs`

**Interfaces:**
- Produces, appended after `PeerDisplayInfo`:

```rust
EdgeMap { edge: SwitchEdge, start: i32, end: i32, resistance: i32 },
FocusSwitchAt { target_node: NodeId, x: i32, y: i32 },
```

`EdgeMap` goes Debian to laptop with each heartbeat, and says which laptop edge touches Debian. `FocusSwitchAt` replaces `FocusSwitch` for all new sends. Its `x, y` are laptop-local. `FocusSwitch` stays in the enum, unused, to hold indexes.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn new_payloads_round_trip()       // encode then decode EdgeMap and FocusSwitchAt
#[test] fn existing_indexes_unchanged()    // Heartbeat payload bytes after the 16-byte header == 7u32 little-endian; PeerDisplayInfo == 8u32
```

- [ ] **Step 2: Run** `cargo test protocol`. Expected: compile failure.
- [ ] **Step 3: Add the variants.**
- [ ] **Step 4: Run** `cargo test protocol`. Expected: all pass.
- [ ] **Step 5: Commit** `src/protocol.rs`, message `Add edge map and positioned focus packets`.

---

### Task 5: Input backend module

**Files:**
- Create: `src/input/mod.rs`
- Move: `src/linux.rs` to `src/input/linux.rs`, `src/windows.rs` to `src/input/windows.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `router::Action`, `protocol::Payload`
- Produces:

```rust
pub trait InputBackend {
    fn set_blocking(&mut self, mouse: bool, keyboard: bool);
    fn inject_move(&mut self, dx: i32, dy: i32);
    fn inject_button(&mut self, button: u8, pressed: bool);
    fn inject_wheel(&mut self, dx: i32, dy: i32);
    fn inject_key(&mut self, scancode: u16, pressed: bool);
    fn warp_cursor(&mut self, x: i32, y: i32);
    fn release_held(&mut self);
}
pub trait PeerSender { fn send(&mut self, payload: Payload); }
pub fn apply(actions: &[Action], backend: &mut dyn InputBackend, peer: &mut dyn PeerSender);
```

`apply` maps `SendFocus` to `FocusSwitchAt` and `SendPanic` to `PanicRelease`, sending each of those three times as today. Other `Forward*` actions send once.

- [ ] **Step 1: Write the failing test** `apply_dispatches_every_action` in `src/input/mod.rs`, using a recording fake backend and sender. Assert each `Action` produces its backend call or payload, and focus and panic are sent 3 times.
- [ ] **Step 2: Run** `cargo test input`. Expected: compile failure.
- [ ] **Step 3: Move the two files with `git mv`, fix `mod` paths, add the traits and `apply`.** No behaviour changes in the moved files.
- [ ] **Step 4: Run** `cargo test` and `cargo check --target x86_64-pc-windows-gnu`. Expected: both pass.
- [ ] **Step 5: Commit** message `Add input backend layer`.

---

### Task 6: Debian on the router

**Files:**
- Modify: `src/input/linux.rs`, `src/spatial.rs` (`apply_master_action`, telemetry)

**Interfaces:**
- Consumes: `Router`, `Event`, `apply`, `InputBackend`, `EdgeMap`, `FocusSwitchAt`
- Produces: `struct LinuxBackend` owning `vkbd`, `vmouse`, a new `VirtualRelMouse`, `mice`, `keyboards`.

- [ ] **Step 1: Add `VirtualRelMouse`** beside `VirtualMouse`: a uinput device with `REL_X`, `REL_Y`, `REL_WHEEL`, `REL_HWHEEL`, and the five `BTN_*` codes, with `move_rel(dx, dy)`, `button(button: u8, pressed: bool)`, `wheel(dx, dy)`, tracking held buttons for `release_held`. Compile check only: uinput is not exercised in tests.
- [ ] **Step 2: Replace focus state in `run_debian_node`.** Remove `active_node`, `panic_mode`, `peer_online`, the On-The-Go block, and `was_peer_online`. Create one `Router` with `me` 0, `peer` 1, `main` 0, `pointer_speed` from `cfg_snapshot.mouse_speed`, refreshed with `set_pointer_speed` when the config changes. Every UDP packet feeds `PeerAlive` then its event. Each loop feeds `Tick` from a monotonic `Instant`.
- [ ] **Step 3: Route keyboards.** Change `for kbd in &keyboards` to mutable iteration. Every key feeds `LocalKey`. Hotkeys are matched first and feed `ManualSwitch`, `Panic`, or the game lock. Delete the block starting `If user types on physical Debian keyboard`. `set_blocking` grabs keyboards as well as mice.
- [ ] **Step 4: Route the mouse.** Physical mice feed `LocalMove`, `LocalButton`, and `LocalWheel`. When Debian is active and the router injects peer movement, feed the updated KWin cursor to `check_debian_to_windows` exactly as for local movement, and call `EdgeCrossed` with the entry point it returns. Delete `check_windows_to_debian` calls and the `win_cursor_x`, `win_cursor_y` crossing logic: the laptop now checks its own edges.
- [ ] **Step 5: Send `EdgeMap` with each heartbeat.** Add `SpatialConfig::laptop_edge_span(&self) -> EdgeSpan` to `spatial.rs`, computing which laptop edge touches Debian from `attach_side` and the overlap in laptop-local pixels.

Test first, in `src/spatial.rs`:

```rust
#[test] fn bottom_attachment_gives_top_span()   // laptop below DP-2: edge Top, span == overlap with DP-2
#[test] fn right_attachment_gives_left_span()   // laptop right of desktop: edge Left
#[test] fn partial_overlap_is_clamped()         // laptop hanging past a monitor: span covers only the touching part
```

- [ ] **Step 6: Handle `FocusSwitchAt`** from the laptop by feeding `PeerFocus` with `x + win_x`, `y + win_y` mapped to a warp, keeping `kwin_x`, `kwin_y` in sync as the current code does.
- [ ] **Step 7: Point the Studio at the router.** `toggle_focus`, `panic`, and telemetry `active_node`, `panic_mode`, `peer_online` read and write router state through the existing pending fields in `SharedSpatialState`.
- [ ] **Step 8: Verify.** Run `cargo test` and `cargo build --release`. Expected: pass, no use of `active_node` left: `grep -n 'active_node\.' src/input/linux.rs` prints nothing.
- [ ] **Step 9: Commit** message `Move Debian input onto the router`.

---

### Task 7: Windows on the router

**Files:**
- Modify: `src/input/windows.rs`

**Interfaces:**
- Consumes: `Router`, `EdgeTracker`, `EdgeSpan`, `evdev_to_win_scan`, `apply`
- Produces: `struct WindowsBackend`, and a global `ROUTER: OnceLock<Mutex<Router>>`.

- [ ] **Step 1: Add the mouse hook.** Install `WH_MOUSE_LL` beside the keyboard hook on the message-loop thread. Ignore events with `LLMHF_INJECTED`. Offset is `pt` minus the last known position.
- [ ] **Step 2: Forward from the pinned position.** When `router.handle` says `swallow`, return 1 from the hook and treat the screen centre as the pin: `SetCursorPos(w / 2, h / 2)` once when blocking starts, then each offset is `pt - centre`. Centre is required because a cursor pinned on an edge would report no outward movement.
- [ ] **Step 3: Route buttons and wheel** from `WM_LBUTTONDOWN` through `WM_XBUTTONUP`, `WM_MOUSEWHEEL`, and `WM_MOUSEHWHEEL` to `LocalButton` and `LocalWheel`, using the `MOUSE_BTN_*` constants.
- [ ] **Step 4: Route the keyboard hook** through `LocalKey`, keeping hotkey detection first, feeding `Panic`, `ManualSwitch`, and the game lock. Remove `SWALLOWED_SCANCODES`: the router tracks this now.
- [ ] **Step 5: Add key injection.** `WindowsBackend::inject_key` uses `SendInput` with `KEYEVENTF_SCANCODE`, adding `KEYEVENTF_EXTENDEDKEY` and `KEYEVENTF_KEYUP` as needed, and records held keys. `release_held` releases held keys and `BUTTONS_HELD_MASK` buttons.
- [ ] **Step 6: Replace the UDP and watchdog logic.** Packets from the peer feed `PeerAlive` then their event. The watchdog thread feeds `Tick` every 100 ms, with `apply` on the result. Remove `ACTIVE_NODE`, `PANIC_MODE`, `is_debian_peer_online`, and the `switch_edge` `hit_edge` block.
- [ ] **Step 7: Check edges on the laptop.** On `EdgeMap`, store the span and call `set_resistance`. After every movement while Windows is active, local or injected, call `EdgeTracker::push` with the cursor position, and on `true` feed `EdgeCrossed { x, y }`. `is_windows_game_captive()` feeds `SetCaptive` from the watchdog.
- [ ] **Step 8: Update `studio_handler`** to read `active_node`, `peer_online`, and `panic_mode` from the router.
- [ ] **Step 9: Verify.** Run `cargo check --target x86_64-pc-windows-gnu` and `cargo test`. Expected: pass; `grep -n 'ACTIVE_NODE\|SWALLOWED_SCANCODES' src/input/windows.rs` prints nothing.
- [ ] **Step 10: Commit** message `Move Windows input onto the router`.

---

### Task 8: Windows end-to-end test under Wine

**Files:**
- Create: `tests/wine_link.py`

**Interfaces:**
- Consumes: built `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe`, Payload indexes from Task 4 (`Heartbeat` 7, `EdgeMap` 9, `FocusSwitchAt` 10).

- [ ] **Step 1: Write the script**, based on this structure: run `wine mesh-kvm.exe --peer 127.0.0.2` with a temporary `WINEPREFIX` and no `DISPLAY`; send UDP from an ephemeral port on `127.0.0.2`; read `/api/local` on 24880. Bincode layout: 4-byte magic `MKVM`, `u64` seq, `u32` source, `u32` variant, fields little-endian.

Assertions, each printed with PASS or FAIL, exit non-zero on any FAIL:

- Packets from `127.0.0.3` leave `peer_online` false.
- Heartbeats from `127.0.0.2` make `peer_online` true and `active_node` `Debian` (link up gives control to main).
- After 2 seconds of silence: `peer_online` false, `active_node` `Windows`.
- Sending `FocusSwitchAt { target_node: Windows, x: 100, y: 200 }` then `cursor` in `/api/local` equals `[100, 200]`.

- [ ] **Step 2: Run** `cargo build --release --target x86_64-pc-windows-gnu`, then `unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_link.py'`. Expected: all PASS.
- [ ] **Step 3: Commit** `tests/wine_link.py`, message `Add Wine link test for the Windows node`.

---

### Task 9: Build and hand over for hardware testing

**Files:**
- Create: `README.md`

- [ ] **Step 1: Write `README.md`:** what mesh-kvm does, how to build both targets, how to update the laptop, the hotkeys (Ctrl+Alt+F11 switch, Ctrl+Alt+F12 release, Ctrl+Alt+G game lock), and the lock screen and UAC limit.
- [ ] **Step 2: Build both release targets.** Run `cargo build --release && cargo build --release --target x86_64-pc-windows-gnu`. Expected: both finish; `cargo test` passes.
- [ ] **Step 3: Commit** `README.md`, message `Add README`.
- [ ] **Step 4: Ask the user to install and run the hardware checks.** Update Debian and the laptop together, then check: trackpad crosses to Debian and back; desk keyboard types on the laptop; a modifier held across a crossing doesn't stick; dropping Wi-Fi while forwarding, in both directions, returns input within 2 seconds; a game keeps the cursor captive on each device; Ctrl+Alt+F12 works on each device. The trackpad's push at a screen edge can only be tested on real Windows, because it depends on what the hook reports at the edge.
