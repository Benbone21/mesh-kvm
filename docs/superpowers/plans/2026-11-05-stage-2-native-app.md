# Stage 2: Native App and Toggles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the web page with a real app and tray icon on both devices, with toggles for what each device shares and which device is main.

**Architecture:** One `mesh-kvm` program with three modes: the service (no flag), `--app` (the egui window), and `--tray` (the tray icon). The window and tray hold no state. They read and write the service through the existing API on 127.0.0.1:24880, and reach the other device through `/api/peer`.

**Tech Stack:** Rust 2021, `eframe` 0.31 (features `glow`, `wayland`, `x11`, `default_fonts`, no default features), `ksni` 0.3 on Linux, `tray-icon` 0.19 on Windows. A spike confirmed `eframe` and both tray crates build for Linux and `x86_64-pc-windows-gnu` from this machine.

**Spec:** `docs/superpowers/specs/2026-11-05-universal-mesh-design.md` (build stage 2).

## Global Constraints

- The app and tray hold no state and never touch input. If they crash or close, sharing carries on.
- Closing the window ends only the `--app` process. Quit in the tray ends only the `--tray` process. Neither stops the service.
- Toggles per device: mouse sharing, keyboard sharing, audio send and receive, clipboard send and receive, pointer speed, start at boot. Every toggle persists in `LocalSettings`.
- Changing the main device needs the link up and applies to both devices.
- First-run defaults: all sharing on, Debian main, laptop pointer speed 1.0.
- Link loss, Ctrl+Alt+F12, and the router's safety rules are unchanged.
- Copy is sentence case, in plain words a user would say. Errors say what to do next.
- Colours carry over from the Studio: deep `#15242a`, panel `#20353c`, raised `#2b464e`, edge `#41636c`, ink `#eee9da`, dim `#a5bcbc`, amber `#ffb22e`, green `#63e08e`, red `#ff7a66`, Debian screens `#3f8c8c`.
- Never run a real service or build into `target/` during development. Use `CARGO_TARGET_DIR=/tmp/claude-1000/mesh-kvm-check`. The user's live service is on this machine, and `--app` and `--tray` can't be shown here without a user session, so verification is unit tests plus both builds.
- Every task leaves `cargo test` and `cargo check --target x86_64-pc-windows-gnu` passing with no warnings.
- `layout.json` is changed by the running service: never stage it.
- Commit with `git -c user.name=bread -c user.email=jj1nfz2nb@mozmail.com commit`, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. Turning off mouse or keyboard sharing while that input is being forwarded: blocking must end at once, and nothing may stick. Tested in Task 1.
2. Changing the main device while the link is up: both devices must settle on one active device. Tested in Task 1.
3. A corrupt or older `local-settings.json`: the service starts with defaults and local input. Tested in Task 2.
4. The service or the other device being unreachable while the app is open: the app shows what's wrong and never freezes. Tested in Task 3 and Task 4.
5. Dragging the laptop while a poll arrives: the drag must not jump. Tested in Task 3.

---

### Task 1: Live router toggles

**Files:** Modify `src/router.rs`.

**Interfaces:**
- Produces: `Event::SetSharing(Sharing)`, `Event::SetMain(DeviceId)`, `Router::sharing() -> Sharing`, `Router::main() -> DeviceId`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn turning_off_sharing_stops_blocking()      // forwarding: SetSharing{false,false} -> SetBlocking{false,false}; next LocalKey not swallowed
#[test] fn sharing_off_releases_forwarded_keys()     // held forwarded key gets ForwardKey{pressed:false}; later physical release is local
#[test] fn turning_sharing_back_on_resumes()         // SetSharing{true,true} while peer active -> SetBlocking{true,true}
#[test] fn set_main_swaps_view_duty()                // router(1) SetMain(1): sends views on Tick; router(0) SetMain(1): stops sending and follows views
#[test] fn both_routers_settle_after_main_change()   // two routers exchange views after SetMain: active() equal
```

- [ ] **Step 2: Run** `cargo test router`. Expected: compile failure.
- [ ] **Step 3: Implement.** Reuse `control_lost`'s release forwarding. `SetMain` clears `view_heard`.
- [ ] **Step 4: Run** `cargo test router`. Expected: all pass.
- [ ] **Step 5: Commit**, message `Add live sharing and main device toggles to the router`.

---

### Task 2: Toggles in the service

**Files:** Modify `src/settings.rs`, `src/clipboard.rs`, `src/spatial.rs`, `src/input/linux.rs`, `src/input/windows.rs`.

**Interfaces:**
- Consumes: Task 1 events.
- Produces in `LocalSettings`, all `#[serde(default)]`: `mouse_sharing: bool`, `keyboard_sharing: bool`, `clipboard_send: bool`, `clipboard_receive: bool`, `pointer_speed: f32`, `main_device: u8`.
- Produces in `/api/local`: `"sharing": {"mouse","keyboard"}`, `"clipboard": {"send","receive"}`, `"pointer_speed"`, `"main_device"`.
- Produces in `POST /api/local/action`: `set_sharing {mouse, keyboard}`, `set_clipboard {send, receive}`, `set_pointer_speed {speed}` (clamped 0.05 to 2.0), `set_main_device {device}`.

- [ ] **Step 1: Write the failing tests** in `src/settings.rs`:

```rust
#[test] fn old_settings_file_gets_defaults()         // JSON with only audio fields: sharing on, clipboard on, speed 1.0, main 0
#[test] fn corrupt_settings_file_gives_defaults()    // "{nope" parses to defaults, no panic
#[test] fn pointer_speed_is_clamped()                // set_pointer_speed 9.0 stores 2.0
#[test] fn unknown_main_device_is_ignored()          // set_main_device 7 leaves it unchanged
```

- [ ] **Step 2: Run** `cargo test settings`. Expected: compile failure.
- [ ] **Step 3: Implement the settings and actions.** Add `CLIPBOARD_SEND` and `CLIPBOARD_RECEIVE` atomics in `clipboard.rs`, gating outgoing and incoming sync. Debian's pointer speed stays `mouse_speed` from the layout, so `set_pointer_speed` on Debian writes the layout value.
- [ ] **Step 4: Feed the router.** Add atomics for pending sharing and main changes in `settings.rs`. Each input loop tick takes them and feeds `SetSharing` and `SetMain`. Load saved values into `RouterConfig` at startup.
- [ ] **Step 5: Run** `cargo test` and the Windows check. Expected: pass.
- [ ] **Step 6: Commit**, message `Add persisted toggles to the service and API`.

---

### Task 3: App model

**Files:** Create `src/app/mod.rs`, `src/app/model.rs`. Modify `src/main.rs`, `Cargo.toml`.

**Interfaces:**
- Produces in `model.rs`, with no egui types:

```rust
pub struct DeviceStatus { /* deserialized /api/local */ }
pub struct LayoutState { /* config + telemetry from /api/state */ }
pub struct Snapshot { pub local: Option<DeviceStatus>, pub peer: Option<DeviceStatus>, pub layout: Option<LayoutState> }
pub struct Placement { pub label: String, pub x: i32, pub y: i32, pub side: String }
pub struct MapView { pub scale: f32, pub offset_x: f32, pub offset_y: f32 }
pub fn placements(cfg: &LayoutConfig) -> Vec<Placement>;
pub fn snap(cfg: &LayoutConfig, x: i32, y: i32) -> Option<Placement>;
pub fn fit(cfg: &LayoutConfig, width: f32, height: f32) -> MapView;
pub fn merge_poll(current: Option<LayoutState>, polled: Option<LayoutState>, dragging: bool) -> Option<LayoutState>;
pub fn status_message(snapshot: &Snapshot) -> Option<String>;
```

`placements`, `snap`, and `fit` port the same functions from `src/ui/index.html`.

- [ ] **Step 1: Write the failing tests** in `model.rs`, using the real two-monitor layout from `layout.json`:

```rust
#[test] fn offers_below_each_monitor_and_outer_sides()  // Below DP-2, Below DP-3, Left of DP-3, Right of DP-2 present; no Left of DP-2
#[test] fn snap_lands_on_nearest_edge()                 // drag near DP-2's bottom gives side Bottom, y 1440
#[test] fn fit_keeps_all_screens_in_view()              // every screen rect inside width x height after transform
#[test] fn poll_does_not_move_a_dragged_laptop()        // merge_poll with dragging true keeps current
#[test] fn service_down_message_says_what_to_do()       // local None mentions starting mesh-kvm
#[test] fn peer_down_message_names_firewall_port()      // peer None mentions TCP port 24880
#[test] fn parses_real_api_json()                       // fixture strings for /api/local and /api/state deserialize
```

- [ ] **Step 2: Run** `cargo test app`. Expected: compile failure.
- [ ] **Step 3: Implement.** Add the dependencies from the Tech Stack line, with `ksni` and `tray-icon` under target-specific tables. Set `strip = true` in the release profile to limit size.
- [ ] **Step 4: Run** `cargo test app` and the Windows check. Expected: pass.
- [ ] **Step 5: Commit**, message `Add app model and layout logic`.

---

### Task 4: API client

**Files:** Create `src/app/client.rs`.

**Interfaces:**
- Consumes: `webui::http_call`, `model::Snapshot`.
- Produces:

```rust
pub struct Client { /* background poll thread, latest Snapshot behind a Mutex */ }
impl Client {
    pub fn start(on_update: Box<dyn Fn() + Send>) -> Self;
    pub fn snapshot(&self) -> Snapshot;
    pub fn local_action(&self, device: Device, action: serde_json::Value);
    pub fn master_action(&self, action: &str);
    pub fn save_layout(&self, cfg: &LayoutConfig);
}
```

Polls every 200 ms on its own thread. Sends go through a queue on a worker thread, so the UI thread never touches the network. `Device` is `Debian` or `Windows`, routed to local or `/api/peer` by comparing with `snapshot.local.node`.

- [ ] **Step 1: Write the failing tests** against a tiny `TcpListener` stub on an ephemeral port. `webui::http_call` needs a port argument, so add `http_call_port(host, port, ...)` and keep `http_call` as a wrapper.

```rust
#[test] fn unreachable_service_gives_empty_snapshot()  // no listener: local None within 1 s, no panic
#[test] fn peer_502_gives_no_peer()                    // stub 502 on /api/peer: peer None, local Some
#[test] fn actions_route_to_the_right_device()         // Windows client: Debian action hits /api/peer/api/local/action
```

- [ ] **Step 2: Run** `cargo test client`. Expected: compile failure.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test client`. Expected: pass.
- [ ] **Step 5: Commit**, message `Add app API client`.

---

### Task 5: App window

**Files:** Create `src/app/ui.rs`, `src/app/theme.rs`. Modify `src/app/mod.rs`, `src/main.rs`.

**Interfaces:**
- Consumes: `Client`, `model::*`.
- Produces: `pub fn run_window() -> Result<(), String>`, and the `--app` flag in `Cli`. `--studio` stays as an alias.

- [ ] **Step 1: Build the window** with `eframe::run_native`, title `mesh-kvm`, size 1100 by 760, minimum 760 by 560. `theme.rs` sets the Global Constraints colours.
- [ ] **Step 2: Top bar.** Both devices with status lights, and a slide switch showing who has control, disabled when not linked. A notice strip shows `status_message`.
- [ ] **Step 3: Screen map.** Painted rectangles from `fit`, draggable laptop using `snap`, red cursor dot. Saves on release through `save_layout`.
- [ ] **Step 4: Side panel.** Laptop position, laptop resolution, edge push, game lock toggles, Reset to native resolutions, Release all input, and Main device.
- [ ] **Step 5: Device panels.** One per device: mouse and keyboard sharing, pointer speed, audio send and receive with levels, clipboard send and receive, test sound, start at boot. Unreachable devices show the message from `status_message`.
- [ ] **Step 6: Failure reporting.** If `eframe` can't start, for example no OpenGL on the laptop, write the error to `mesh-kvm.log` through `log_fatal`, moved to `settings.rs` and used on both platforms.
- [ ] **Step 7: Verify.** Run `cargo test`, then Linux and Windows release builds. Expected: pass, no warnings.
- [ ] **Step 8: Commit**, message `Add native app window`.

---

### Task 6: Tray and single instance

**Files:** Create `src/app/tray.rs`, `src/app/single.rs`. Modify `src/main.rs`.

**Interfaces:**
- Produces: `pub fn run_tray() -> Result<(), String>`, the `--tray` flag, and:

```rust
pub enum Instance { First(TcpListener), AlreadyRunning }
pub fn claim(port: u16) -> Instance;   // binds 127.0.0.1:port
pub fn signal_show(port: u16);          // asks the running window to focus
```

Ports: 24881 window, 24882 tray.

- [ ] **Step 1: Write the failing tests** in `single.rs`:

```rust
#[test] fn second_claim_reports_already_running()
#[test] fn claim_works_after_first_is_dropped()
#[test] fn signal_show_reaches_the_listener()
```

- [ ] **Step 2: Run** `cargo test single`. Expected: compile failure.
- [ ] **Step 3: Implement `single.rs`.** The window listens on a thread and raises itself with `ViewportCommand::Focus`.
- [ ] **Step 4: Implement `tray.rs`.** Menu: Open, Switch device, Release all input, Quit. Linux uses `ksni` with a pixmap icon generated in code. Windows uses `tray-icon` with a `GetMessageW` loop. Open spawns `current_exe --app`. Tray state comes from one `Client`, shown in the tooltip.
- [ ] **Step 5: Verify.** Run `cargo test` and both release builds. Expected: pass.
- [ ] **Step 6: Commit**, message `Add tray icon and single-instance handling`.

---

### Task 7: Launchers

**Files:** Modify `src/input/windows.rs`, `src/main.rs`. Create `src/app/launch.rs`, `src/app/icon.rs`.

**Interfaces:**
- Produces: `pub fn windows_vbs(exe: &str, peer: &str, edge: &str) -> String`, `pub fn linux_desktop_entry(exe: &str, args: &str) -> String`, and an `--install-launchers` flag on Linux.

- [ ] **Step 1: Write the failing tests** in `launch.rs`:

```rust
#[test] fn login_script_starts_service_and_tray()   // two Run lines; one has --tray
#[test] fn start_menu_opens_the_app()               // --app, not --studio
#[test] fn desktop_entry_is_valid()                 // [Desktop Entry], Type=Application, Exec has the path
```

- [ ] **Step 2: Run** `cargo test launch`. Expected: compile failure.
- [ ] **Step 3: Windows.** The login script starts the service and `--tray`. The Start Menu entry becomes `mesh-kvm.vbs` running `--app`, and the old `mesh-kvm Studio.vbs` is deleted. A bare launch from `main.rs` starts the service if it isn't running, then spawns `--tray` and `--app`, replacing `open_studio_when_ready`.
- [ ] **Step 4: Linux.** `--install-launchers` writes `~/.local/share/applications/mesh-kvm.desktop` and `~/.config/autostart/mesh-kvm-tray.desktop`.
- [ ] **Step 5: Embed the icon** in the exe and desktop entry, generated by `icon.rs`.
- [ ] **Step 6: Verify.** Run `cargo test`, the Windows check, and the Wine test with the built exe. Expected: all pass.
- [ ] **Step 7: Commit**, message `Add app launchers for login and menus`.

---

### Task 8: Retire the web page

**Files:** Delete `src/ui/index.html`, `mesh-kvm-ui.py`. Modify `src/webui.rs`, `README.md`.

- [ ] **Step 1: Remove** `INDEX_HTML` and `open_app_window` and the Firefox and Edge code. `GET /` returns 404. The `/api/*` routes and peer proxy stay.
- [ ] **Step 2: Add a test** `root_serves_no_page` using `handle_conn` on an ephemeral listener.
- [ ] **Step 3: Update `README.md`** for the app, tray, toggles, `--install-launchers`, and that the API port stays open until the encryption stage.
- [ ] **Step 4: Verify.** Run `cargo test`, both release builds, and the Wine test. Expected: all pass. `grep -rn 'index.html\|mesh-kvm-ui' src README.md` prints nothing.
- [ ] **Step 5: Commit**, message `Remove the web page and old Python UI`.
- [ ] **Step 6: Hand over.** Ask the user to update the laptop first, then Debian, run `mesh-kvm --install-launchers`, and check: the app opens on both devices, closing the window keeps sharing and the tray, each toggle takes effect and survives a restart, Main device changes who gets control, and the laptop's OpenGL works.
