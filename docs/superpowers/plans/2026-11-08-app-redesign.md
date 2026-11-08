# App Redesign ("Readout") Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild the mesh-kvm app window as the flat, dark Readout layout: status bar, live link figures with sparklines, every setting for both devices in one table, and the screen layout beside it.

**Architecture:** Decisions stay out of egui. New pure modules (`view.rs` for the status sentence, stats, notices and window breakpoints; `rows.rs` for the settings table; `sparkline.rs`; `motion.rs`) are unit-tested on plain data. Drawing modules (`top.rs`, `table.rs`, `dialog.rs`, restyled `canvas.rs` and `pairing.rs`) render what those return and hand back request enums that `ui.rs` applies through the existing `Client`, `Edits` and `LayoutDraft`. The service API, the tray and the process model (service, `--tray` and `--app` as separate processes) don't change.

**Tech Stack:** Rust 2021, eframe/egui 0.31 (glow, wayland, x11, plus the `accesskit` feature added in Task 10), Inter 4.1 and JetBrains Mono 2.304 (OFL, bundled), Python 3 for `tests/fake_service.py`.

**Spec:** `docs/superpowers/specs/2026-11-08-app-redesign-design.md`, and the boards in `docs/superpowers/specs/2026-11-08-app-redesign-boards/` (`ChosenApp`, `ChosenPairShow`, `ChosenPairEnter`). Read both before starting. Token values, sizes and copy are taken from there and repeated here only where a test pins them. Where this plan and the spec disagree, the spec wins, except for the rulings listed under "Rulings on the spec" below.

## Global Constraints

- Work on branch `stage-2-native-app`, starting from `470f86e` (version 0.8.0). No push, tag or release. The controller releases.
- Commit with `env GIT_AUTHOR_NAME=bread GIT_AUTHOR_EMAIL=jj1nfz2nb@mozmail.com GIT_COMMITTER_NAME=bread GIT_COMMITTER_EMAIL=jj1nfz2nb@mozmail.com git commit`, and end every message with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Add files by name. Never stage `layout.json`.
- Every task leaves both of these passing with no warnings:
  - Linux: `env -u CARGO_TARGET_DIR cargo test --release`
  - Windows (cross-compiled, unit tests run under Wine): `exe=$(env -u CARGO_TARGET_DIR cargo test --release --target x86_64-pc-windows-gnu --no-run 2>&1 | grep -o 'target/x86_64-pc-windows-gnu/release/deps/mesh_kvm-[0-9a-f]*\.exe' | head -n1) && WINEDEBUG=-all wine "$exe" app::`
- The service API (`/api/local`, `/api/state`, `/api/pairing`, `/api/layout`, the actions) doesn't change. No file outside `src/app/`, `src/licenses.rs`, `src/main.rs`, `third_party/fonts/`, `tests/fake_service.py`, `packaging/build-deb.sh`, `Cargo.toml`/`Cargo.lock`, `README.md` changes.
- Every behaviour behind the current panels and Settings menu stays reachable. Task 6's `every_setting_has_a_row` test is the checklist.
- Colours come only from the named tokens in `src/app/theme.rs`. After Task 12, `grep -n 'from_rgb' src/app/*.rs` matches only `theme.rs`.
- Radius 0 and 1px borders everywhere. No shadows or gradients. Mono (JetBrains Mono) for figures, values and labels. Inter for sentences.
- Copy follows the project style: plain sentences, no exclamation marks, say what the person can do. Errors use the service's or the client's existing sentence, never wrapped in "Error:".
- Nothing blocks the UI thread. Pairing, main device and address changes stay on background threads, as they are now.
- Dependencies: the only new one is eframe's `accesskit` feature (Task 10). The fonts are files, not crates.
- Match the existing code style: short plain doc comments, no section banners, tests named as sentences.

## Rulings on the spec

These are the places where the spec is silent or ambiguous. Each task follows them.

1. **Window close.** `--app` is already its own process. X ends only that process, so the service (sharing) and the `--tray` process keep running, and reopening starts `--app`, which raises the existing window through `single.rs` if there is one. winit can't hide a window on Wayland (`set_visible` does nothing there), so "X hides it" is met by ending the window process. Task 11 adds what was missing: opening the window starts the tray if none is running, so there is always a way back after X. On Windows, a second `--app` grants foreground rights before it asks the first window to come forward. KDE gets larger icon sizes. The app id is one constant shared by the viewport and `mesh-kvm.desktop`.
2. **Notices.** `status_message`, `start_notice`, screen-change notes and the client's last error appear as notice lines in the update line's slot, below it, each with a `warn` or `danger` square. A line the status sentence already says (not paired, no service) is left out. With no service, the body shows the reason, as the spec's edge case says.
3. **The status sentence** for states the spec doesn't word: `Link down.` + `Each device is using its own keyboard and mouse.` (`danger`), `Connecting to` + **Other** (`text-dim`), `One way.` + `<Other> can't hear this device.` (`warn`), `Not linked.` + `<Other> needs an update before it can pair.` (`warn`), `Starting.` + `Starting mesh-kvm on this device.` (`text-dim`), `Not running.` + `mesh-kvm isn't running on this device.` (`danger`).
4. **LINK words** beyond the spec's list: `One way` in `warn`. `Link down` is `danger`, `Connecting` is `text-dim`, `Secure` is `text`. A service that doesn't encrypt shows `Linked` or `Link down`.
5. **Settings outside the spec's groups.** App size goes in BOTH DEVICES (this device's column), as it is the one group that isn't per device. Version, Check now, the update note and Licences go in UPDATES. Device names are a `Name` row at the top of BOTH DEVICES. Audio level meters are `Level sent` and `Level received` rows. The test sound is a `Test sound` row.
6. **Notes under groups.** Per-device lines (clipboard note, audio notes and hint, sending and receiving quality, the game lock hint, the old-audio warning) are captions under their group, starting with the device name: `Laptop: Not reaching Debian yet.` They use `warn` when they say something is wrong, else `text-dim`.
7. **Screens.** When each device has one screen, the rows are `Resolution` and `Refresh rate`. Otherwise they are numbered by the layout's order: `Screen 1 resolution`, `Screen 1 refresh rate`, `Screen 2 resolution`, and so on, with a blank cell where a device has no such screen. Each cell's tooltip names the monitor.
8. **Game locks** apply to this device only (as now), so the other device's cells are blank. The board shows both, as an illustration.
9. **Main device.** The main device's cell reads `MAIN` in `accent`. The other's reads `Make main` in `text-dim` and makes it main when clicked. The board leaves that cell blank, but the choice has to stay reachable.
10. **Pairing choice** (not on the boards): title `PAIR WITH <OTHER>`, the existing sentence and address caption, then `Cancel`, `Enter a code` (secondary) and `Show a code` (primary). The `here` is dropped from both button labels to match the spec's `choose Enter a code`. A failure on the showing side shows the service's text in `danger` with `Cancel` and a primary `Show a new code`. The paired screen has no button and closes after `PAIRED_SHOWN`.
11. **Code input.** The space appears when the fourth digit is typed (`482` becomes `482 9`). A space shown after three digits would make Backspace delete only the space, and the reformat would put it straight back.
12. **Arrow keys on the canvas** move the selected screen's device by 10 desktop pixels (Shift: 1), through the same snapping as a drag.
13. **Focus order** follows the drawing order, so the update line's button comes right after the status bar's actions, where the line sits. The spec's order was written before the update line had a place.
14. **Per-row refusals** apply to the table's switches and numbers, which go through `Control`. Dropdown picks and other actions keep the existing window-wide error line. The row error uses the client's existing sentence from `send_problem`.
15. **Sparklines** take one sample a second and keep 60. A gap with no repaint is filled with the last value, as nothing changed. They are hidden while a figure has never been known, frozen with a hollow last point when it stops being known, and cleared when the devices are unpaired. LINK plots 1 while linked and 0 while down.
16. **Fonts.** Four static files: Inter Regular and SemiBold, JetBrains Mono Regular and Medium. Their real size (likely 1–1.4 MB, more than the spec's ~600 KB estimate) goes in the Task 1 commit message. The spec accepted bundling, and nothing is subset.
17. **Reduced motion** is read from the OS: on Windows `SPI_GETCLIENTAREAANIMATION`, on Linux `AnimationDurationFactor=0` in `~/.config/kdeglobals`. If neither says so, motion stays on.
18. **The tray** is unchanged. Its menu is native and has nothing to restyle.
19. **Names past 24 characters** keep their first 24 and gain `…`. The table header shows them upper-cased, as on the board.
20. **The dimmed app behind a dialog** is `BG` at 60% opacity. The boards show no backdrop.

## Review Focus

1. **The window at its smallest (760×560) and largest (3840×2160), in every snapshot state:** nothing overlaps, the table scrolls and no text is cut off. Pinned in Task 7 (`draws_every_state_from_the_smallest_window_to_full_screen`, carried over and extended to the stacked layout).
2. **A toggle the service refuses:** the value snaps back at once and the refusal shows under that row, not as a second window-wide line. Pinned in Task 4 (`a_refused_control_is_kept_for_its_row_and_not_the_window`) and Task 7 (`a_refused_toggle_snaps_back_and_says_why_under_its_row`).
3. **A device name of 32 characters, with a long monitor name and a long audio device name:** the header and status bar cut at 24 with `…`, cells cut with `…`, and each has the full name in a tooltip. Pinned in Task 2 (`names_past_24_characters_are_cut_with_an_ellipsis`) and Task 7 (`long_names_never_widen_a_device_column`).
4. **A code pasted as `482 917`, `482917` or with stray characters, and Backspace through the space:** the box always holds `### ###` digits and Pair turns on only at six. Pinned in Task 9 (`the_code_box_formats_typing_and_pasting` and `backspace_goes_back_through_the_space`).
5. **The link dropping and coming back while the window is open with nothing else changing:** the sparklines freeze with a hollow last point, then continue, with no jump in time. Pinned in Task 3 (`a_gap_with_no_repaint_repeats_the_last_value` and `a_figure_that_goes_unknown_freezes_the_line`).

---

### Task 1: Fonts and design tokens

**Files:**
- Create: `third_party/fonts/Inter-Regular.ttf`, `third_party/fonts/Inter-SemiBold.ttf`, `third_party/fonts/JetBrainsMono-Regular.ttf`, `third_party/fonts/JetBrainsMono-Medium.ttf`, `third_party/fonts/Inter-OFL.txt`, `third_party/fonts/JetBrainsMono-OFL.txt`, `third_party/fonts/SOURCES.txt`
- Modify: `src/app/theme.rs` (new tokens, fonts and type; the old palette moves to `pub mod legacy` inside the same file), `src/licenses.rs`, `packaging/build-deb.sh` (install the two OFL files beside `opus-COPYING`), and the `use super::theme::…` lines in `src/app/{ui,panels,canvas,pairing,widgets}.rs` (import the old names from `theme::legacy`)

**Interfaces:**
- Produces, in `src/app/theme.rs`:
  - Colour tokens `BG`, `SURFACE`, `RAISED`, `LINE`, `LINE_FAINT`, `LINE_STRONG`, `TEXT`, `TEXT_DIM`, `ACCENT`, `ON_ACCENT`, `WARN`, `DANGER: Color32`, with the spec's hex values.
  - `pub enum Tone { Text, Dim, Accent, Warn, Danger }` and `pub fn tone(t: Tone) -> Color32`.
  - Spacing `SPACE_1..=SPACE_6: f32` = 4, 8, 12, 16, 20, 24. Sizes: `ROW_MIN_HEIGHT = 26.0`, `STATUS_BUTTON_HEIGHT = 36.0`, `DIALOG_BUTTON_HEIGHT = 40.0`, `DIALOG_BUTTON_MIN_WIDTH = 96.0`, `STATUS_SQUARE = 10.0`, `CROSSING_EDGE = 4.0`, `COUNTDOWN_HEIGHT = 4.0`, `SETTINGS_WIDTH = 700.0`, `DEVICE_COLUMN = 130.0`, `DIALOG_WIDTH = 460.0`, `SPARKLINE_SIZE: [f32; 2] = [210.0, 36.0]`, `SPARKLINE_STROKE = 1.5`.
  - Font family names `INTER_SEMIBOLD: &str = "Inter SemiBold"` and `MONO_MEDIUM: &str = "JetBrains Mono Medium"`. `pub fn fonts() -> egui::FontDefinitions`.
  - `pub enum Type { Body, Status, StatusStrong, Caption, Label, Head, Value, Stat, CodeShow, CodeEnter }` with `pub fn font(self) -> FontId`, `pub fn letter_spacing(self) -> f32` and `pub fn job(self, text: &str, colour: Color32) -> egui::text::LayoutJob` (Label upper-cases its text).
  - `pub fn contrast(a: Color32, b: Color32) -> f32` (WCAG ratio), `pub fn hovered(fill: Color32) -> Color32` (8% towards white), `pub fn pressed(fill: Color32) -> Color32` (8% towards black), `pub fn tint(base: Color32, border: Color32, amount: f32) -> Color32`, `pub fn tile_fill(first_device: bool) -> Color32` (= `mix(RAISED, screen_colour(first_device), 0.12)`), and the existing `mix` and `screen_colour`.
  - `pub fn apply(ctx)` now sets the fonts and the new visuals: radius 0 everywhere, `panel_fill = BG`, `window_fill = RAISED`, `window_stroke = 1px LINE_STRONG`, `selection.stroke = 2px ACCENT`, `hyperlink_color = ACCENT`, `error_fg_color = DANGER`, `warn_fg_color = WARN`, and widgets drawn flat with `LINE_STRONG` borders.
  - `pub mod legacy` keeps today's `DEEP, PANEL, RAISED, EDGE, INK, DIM, AMBER, GREEN, RED, AMBER_GROOVE, UNLIT, MAT, BEZEL, GLASS, ON_SCREEN, SCREEN_FIRST, SCREEN_SECOND, RADIUS, SMALL_RADIUS, MARGIN, ROW_HEIGHT, BUTTON_HEIGHT, SMALL_SIZE, BODY_SIZE, HEADING_SIZE, TITLE_SIZE, SECTION_GAP, HEADING_GAP` until Task 12 removes it.

- [ ] **Step 1: Download the fonts into a fresh, empty directory and record where they came from**

```bash
dl=$(mktemp -d) && cd "$dl"
curl -fLO https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip
curl -fLO https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip
sha256sum Inter-4.1.zip JetBrainsMono-2.304.zip
unzip -l Inter-4.1.zip | grep -E 'extras/ttf/Inter-(Regular|SemiBold)\.ttf|LICENSE'
unzip -l JetBrainsMono-2.304.zip | grep -E 'fonts/ttf/JetBrainsMono-(Regular|Medium)\.ttf|OFL'
```

Take the four static TTFs and the two licence files out with `unzip -j` (Inter's `LICENSE.txt` becomes `Inter-OFL.txt`, JetBrains Mono's `OFL.txt` becomes `JetBrainsMono-OFL.txt`) and copy them into `third_party/fonts/`. If a path differs from the one above, take the static (not variable) TTF of that weight from the listing. Write `third_party/fonts/SOURCES.txt` with, for each font, the release URL, the version, the zip's SHA-256, the path inside the zip, and the SHA-256 of each extracted file (`sha256sum third_party/fonts/*.ttf`).

- [ ] **Step 2: Write the failing tests in `src/app/theme.rs` and `src/licenses.rs`**

```rust
#[test]
fn the_tokens_meet_the_contrast_the_spec_promises() {
    let near = |got: f32, want: f32| assert!((got - want).abs() < 0.05, "{got} vs {want}");
    near(contrast(TEXT, BG), 15.4);
    near(contrast(TEXT_DIM, SURFACE), 6.82);
    near(contrast(ACCENT, BG), 14.45);
    near(contrast(ON_ACCENT, ACCENT), 14.45);
    near(contrast(WARN, RAISED), 9.5);
    near(contrast(DANGER, RAISED), 7.07);
    assert!(contrast(LINE_STRONG, BG) >= 3.0);
}

#[test]
fn each_devices_tiles_have_their_own_muted_tint() {
    assert_ne!(tile_fill(true), tile_fill(false));
    assert_ne!(tile_fill(true), RAISED);
    for first in [true, false] {
        assert!(contrast(TEXT, tile_fill(first)) >= 7.0);
        assert!(contrast(TEXT_DIM, tile_fill(first)) >= 4.5);
    }
}

#[test]
fn both_fonts_are_bundled_with_their_weights() {
    let fonts = fonts();
    for name in ["Inter", INTER_SEMIBOLD, "JetBrains Mono", MONO_MEDIUM] {
        assert!(fonts.font_data.contains_key(name), "{name}");
    }
    assert_eq!(fonts.families[&FontFamily::Proportional][0], "Inter");
    assert_eq!(fonts.families[&FontFamily::Monospace][0], "JetBrains Mono");
    // egui's own fonts stay behind them for ▾, × and anything else they lack.
    assert!(fonts.families[&FontFamily::Proportional].len() > 1);
}

#[test]
fn the_type_scale_is_the_specs() {
    assert_eq!(Type::Body.font().size, 14.0);
    assert_eq!(Type::Status.font().size, 15.0);
    assert_eq!(Type::Caption.font().size, 13.0);
    assert_eq!((Type::Label.font().size, Type::Label.letter_spacing()), (11.0, 1.0));
    assert_eq!(Type::Head.font().family, FontFamily::Name(MONO_MEDIUM.into()));
    assert_eq!(Type::Stat.font().size, 26.0);
    assert_eq!((Type::CodeShow.font().size, Type::CodeShow.letter_spacing()), (54.0, 4.0));
    assert_eq!((Type::CodeEnter.font().size, Type::CodeEnter.letter_spacing()), (44.0, 4.0));
    assert_eq!(Type::Label.job("lost", TEXT_DIM).text, "LOST");
}
```

In `src/licenses.rs`: `fn both_font_licences_are_there_in_full()` asserts `TEXT` contains `"SIL OPEN FONT LICENSE Version 1.1"` twice, `"Inter"` and `"JetBrains Mono"`.

- [ ] **Step 3: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release theme:: licenses::`
Expected: FAIL (`contrast`, `fonts`, `Type` not found).

- [ ] **Step 4: Implement the tokens, `fonts()`, `Type`, `apply()` and the licence text**

`fonts()` starts from `FontDefinitions::default()`, inserts each file as `Arc::new(FontData::from_static(include_bytes!("../../third_party/fonts/…")))`, puts `"Inter"` first in `Proportional` and `"JetBrains Mono"` first in `Monospace`, and adds `FontFamily::Name` families for the two heavier weights, each followed by egui's defaults. `Type::job` sets `TextFormat { font_id, color, extra_letter_spacing, line_height: Some(size * 1.4) for Body/Status/Caption }`. `src/licenses.rs` appends `"\nmesh-kvm includes the Inter and JetBrains Mono fonts, under this licence:\n\n"` and both OFL texts with `include_str!`. Move the old constants into `pub mod legacy` and point each old `use` line at it, so nothing else changes this task.

- [ ] **Step 5: Run the tests and the Windows check**

Run: `env -u CARGO_TARGET_DIR cargo test --release`, then the Windows check from Global Constraints.
Expected: PASS. Note `ls -l target/release/mesh-kvm` before and after, for the commit message.

- [ ] **Step 6: Commit**

```bash
git add third_party/fonts src/app/theme.rs src/licenses.rs packaging/build-deb.sh src/app/ui.rs src/app/panels.rs src/app/canvas.rs src/app/pairing.rs src/app/widgets.rs
git commit -m "Readout tokens and bundled Inter and JetBrains Mono (binary +<N> KB)"
```

---

### Task 2: The window's view model

**Files:**
- Create: `src/app/view.rs`, `src/app/fixtures.rs` (`#[cfg(test)]`, the shared snapshots)
- Modify: `src/app/mod.rs` (`mod view; #[cfg(test)] mod fixtures;`), `src/app/model.rs` (`LinkStatus::loss` becomes `pub`), `src/app/ui.rs` (its tests use `fixtures`)

**Interfaces:**
- Consumes: `Snapshot`, `ServiceStart`, `ControlsEnabled`, `LinkStatus::loss`, `status_message`, `start_notice`, `notice_is_serious` from `model.rs`; `theme::Tone`.
- Produces, in `src/app/view.rs` (no egui types):
  - `pub struct StatusLine { pub square: Tone, pub lead: String, pub rest: String, pub rest_strong: bool, pub switch_to: Option<Device>, pub show_release: bool }` and `pub fn status_line(snapshot: &Snapshot, start: &ServiceStart, on: &ControlsEnabled) -> StatusLine`.
  - `pub enum Stat { RoundTrip, Lost, AudioLost, AudioBuffer, Link }`, `pub const STATS: [Stat; 5]`, with `label(self) -> &'static str` (`ROUND TRIP`, `LOST`, `AUDIO LOST`, `AUDIO BUFFER`, `LINK`), `spoken(self) -> &'static str` and `floor(self) -> f32` (20, 5, 5, 100, 1).
  - `pub struct StatValue { pub text: String, pub tone: Tone, pub sample: Option<f32>, pub spoken: String }` and `pub fn stat_value(stat: Stat, snapshot: &Snapshot) -> StatValue`.
  - `pub fn link_word(snapshot: &Snapshot) -> (&'static str, Tone)` and `pub fn degraded(snapshot: &Snapshot) -> bool` (≥ 150 ms or ≥ 5%, from `model.rs`'s `POOR_ROUND_TRIP_MS` and `POOR_LOSS_PERCENT`, made `pub(super)`).
  - `pub fn percent(value: f32) -> String` (one decimal, `.0` dropped).
  - `pub struct Notice { pub text: String, pub tone: Tone, pub dismiss: bool }` and `pub fn notices(snapshot: &Snapshot, start: &ServiceStart, last_error: Option<String>, updating: bool) -> Vec<Notice>`.
  - `pub struct FrameLayout { pub settings_width: f32, pub stacked: bool, pub stat_rows: [usize; 2], pub body_scrolls: bool }`, `pub const STACKED_CANVAS_HEIGHT: f32 = 320.0`, `pub fn frame_layout(width: f32, height: f32) -> FrameLayout` and `pub fn label_width(settings_width: f32) -> f32` (= settings_width − 2 × `SPACE_5` − 2 × `DEVICE_COLUMN`).
  - `pub const MAX_SHOWN_NAME: usize = 24` and `pub fn fit_name(name: &str) -> (String, Option<String>)` (the shown text, and the full name for a tooltip when cut).
- Produces, in `src/app/fixtures.rs`: `pub fn device(node: &str, tx_source: Option<u32>) -> DeviceStatus`, `pub fn linked() -> Snapshot`, `pub fn unpaired() -> Snapshot`, `pub fn states() -> Vec<Snapshot>`, `pub fn update_states() -> Vec<Snapshot>`, `pub fn secure(snapshot: Snapshot, secure: &str) -> Snapshot`, `pub const STATE: &str`, moved from `ui.rs`'s tests unchanged, plus `pub fn link(snapshot: Snapshot, round_trip_ms: u32, loss: f32) -> Snapshot`, `pub fn link_down(snapshot: Snapshot) -> Snapshot` and `pub fn audio_live(snapshot: Snapshot, rx_loss: f32, rx_buffer_ms: u32) -> Snapshot`.

- [ ] **Step 1: Move the fixtures** out of `ui.rs`'s test module into `fixtures.rs`, change `ui.rs`'s tests to `use crate::app::fixtures::*`, and run `env -u CARGO_TARGET_DIR cargo test --release app::ui`. Expected: PASS, no change in behaviour.

- [ ] **Step 2: Write the failing tests in `src/app/view.rs`**

```rust
#[test]
fn the_status_line_says_who_has_control_and_offers_the_switch() {
    let s = secure(linked(), "encrypted");
    let line = status_line(&s, &ServiceStart::Done, &model::enabled(&s));
    assert_eq!((line.square, line.lead.as_str(), line.rest.as_str(), line.rest_strong), (Tone::Accent, "Control", "Debian", true));
    assert_eq!(line.switch_to, Some(Device::Windows));
    assert!(line.show_release);
}

#[test]
fn unpaired_hides_the_switch_and_keeps_release_all() {
    let s = unpaired();
    let line = status_line(&s, &ServiceStart::Done, &model::enabled(&s));
    assert_eq!((line.square, line.lead.as_str(), line.rest.as_str()), (Tone::Warn, "Not paired.", "Everything stays on each device."));
    assert_eq!((line.switch_to, line.show_release), (None, true));
}

#[test]
fn every_link_state_has_its_square_and_words() {
    // link down while paired, connecting, one way, old peer, degraded, starting, failed start
    // each as in "Rulings on the spec" 3, with square Danger / Dim / Warn / Warn / Warn / Dim / Danger.
}

#[test]
fn a_figure_is_dashes_until_it_is_known() {
    for stat in STATS[..4].iter() {
        let v = stat_value(*stat, &unpaired());
        assert_eq!((v.text.as_str(), v.tone, v.sample), ("--", Tone::Dim, None));
    }
    assert_eq!(link_word(&unpaired()), ("Not paired", Tone::Warn));
}

#[test]
fn figures_stand_out_at_the_readme_thresholds() {
    let at = |ms, loss| link(secure(linked(), "encrypted"), ms, loss);
    assert_eq!(stat_value(Stat::RoundTrip, &at(6, 0.4)).text, "6 ms");
    assert_eq!(stat_value(Stat::RoundTrip, &at(149, 0.4)).tone, Tone::Text);
    assert_eq!(stat_value(Stat::RoundTrip, &at(150, 0.4)).tone, Tone::Warn);
    assert_eq!(stat_value(Stat::Lost, &at(6, 0.4)).text, "0.4%");
    assert_eq!(stat_value(Stat::Lost, &at(6, 4.9)).tone, Tone::Text);
    assert_eq!(stat_value(Stat::Lost, &at(6, 5.0)).tone, Tone::Warn);
    let audio = audio_live(secure(linked(), "encrypted"), 0.0, 42);
    assert_eq!(stat_value(Stat::AudioLost, &audio).text, "0%");
    assert_eq!(stat_value(Stat::AudioBuffer, &audio).text, "42 ms");
    assert_eq!(stat_value(Stat::RoundTrip, &at(6, 0.4)).spoken, "Round trip, 6 milliseconds");
    assert_eq!(stat_value(Stat::Lost, &at(6, 0.4)).spoken, "Lost, 0.4 percent");
}

#[test]
fn link_says_secure_connecting_down_or_needs_update() {
    assert_eq!(link_word(&secure(linked(), "encrypted")), ("Secure", Tone::Text));
    assert_eq!(link_word(&secure(linked(), "connecting")), ("Connecting", Tone::Dim));
    assert_eq!(link_word(&link_down(secure(linked(), "encrypted"))), ("Link down", Tone::Danger));
    let mut old = secure(linked(), "connecting");
    old.local.as_mut().unwrap().old_peer = true;
    assert_eq!(link_word(&old), ("Needs update", Tone::Warn));
}

#[test]
fn the_window_width_picks_the_layout() {
    let at = |w, h| frame_layout(w, h);
    assert_eq!(at(1280.0, 760.0), FrameLayout { settings_width: 700.0, stacked: false, stat_rows: [5, 0], body_scrolls: false });
    assert_eq!(at(1100.0, 760.0).settings_width, 700.0);
    assert!((at(1099.0, 760.0).settings_width - 604.45).abs() < 0.01);
    assert_eq!(at(900.0, 760.0).settings_width, 495.0);
    let narrow = at(899.0, 760.0);
    assert!(narrow.stacked && narrow.stat_rows == [3, 2] && narrow.settings_width == 899.0);
    assert!(at(1280.0, 599.0).body_scrolls && !at(1280.0, 600.0).body_scrolls);
    assert_eq!(label_width(700.0), 400.0);
}

#[test]
fn names_past_24_characters_are_cut_with_an_ellipsis() {
    assert_eq!(fit_name("Debian"), ("Debian".into(), None));
    let long = "A desktop with a very long name"; // 31 characters
    let (shown, full) = fit_name(long);
    assert_eq!(shown.chars().count(), 25);
    assert!(shown.ends_with('…'));
    assert_eq!(full.as_deref(), Some(long));
    assert_eq!(fit_name(&"x".repeat(24)).1, None);
}

#[test]
fn notices_leave_out_what_the_status_line_already_says() {
    assert!(notices(&unpaired(), &ServiceStart::Done, None, false).is_empty());
    let none = Snapshot::default();
    assert!(notices(&none, &ServiceStart::Starting, None, false).is_empty());
    let n = notices(&linked(), &ServiceStart::Done, Some("Laptop didn't accept that change.".into()), false);
    assert_eq!((n.last().unwrap().tone, n.last().unwrap().dismiss), (Tone::Danger, true));
    // A device dropping out is what an update looks like, so an update in progress hides the link notices.
}
```

Write out the body of `every_link_state_has_its_square_and_words` with one snapshot per state from the fixtures and the copy from ruling 3.

- [ ] **Step 3: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::view`
Expected: FAIL (module empty).

- [ ] **Step 4: Implement `view.rs`**

The active device comes from `Snapshot::active()`, the other from `Device::other()`. "Linked" is `Snapshot::linked()`. `secure` of `""` means a service that doesn't encrypt. Audio figures are known while `audio.rx_live`, and link figures while `link.measured && peer_online`. Unpaired makes every figure `--`. `switch_to` is `Some(active.other())` only while `on.switch`. `show_release` is `snapshot.local.is_some()`.

- [ ] **Step 5: Run the tests**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/app/view.rs src/app/fixtures.rs src/app/mod.rs src/app/model.rs src/app/ui.rs
git commit -m "View model for the Readout window: status line, stats, notices and breakpoints"
```

---

### Task 3: Sparklines and the top of the window

**Files:**
- Create: `src/app/sparkline.rs`, `src/app/top.rs`
- Modify: `src/app/widgets.rs` (flat buttons and the status square), `src/app/ui.rs` (`show` draws the new top in place of `header`, `update_strip` and `notice_strip`), `src/app/mod.rs`

**Interfaces:**
- Consumes: `view::{status_line, StatusLine, Stat, STATS, stat_value, link_word, notices, Notice}`, `updates::{Banner, UpdateButton, notes_url}`, `theme::{Type, Tone, tone, …}`.
- Produces:
  - `sparkline.rs`: `pub const EVERY: Duration = Duration::from_secs(1)`, `pub const POINTS: usize = 60`, `#[derive(Default)] pub struct Sparkline` with `pub fn push(&mut self, sample: Option<f32>, now: Instant)`, `pub fn points(&self) -> &VecDeque<f32>`, `pub fn frozen(&self) -> bool`, `pub fn clear(&mut self)` and `pub fn path(&self, size: [f32; 2], floor: f32) -> Vec<[f32; 2]>`. `#[derive(Default)] pub struct Sparklines` with `pub fn sample(&mut self, snapshot: &Snapshot, now: Instant)` and `pub fn get(&self, stat: Stat) -> &Sparkline`.
  - `widgets.rs`: `pub enum ButtonKind { Primary, Secondary, Danger }`, `pub fn flat_button(ui: &mut Ui, label: &str, kind: ButtonKind, min_size: Vec2, enabled: bool) -> Response` (hover, pressed and disabled as in the spec's States table, 2px `ACCENT` focus ring 2px out), `pub fn status_square(ui: &mut Ui, colour: Color32)`, `pub fn focus_ring(ui: &Ui, response: &Response)`.
  - `top.rs`: `pub enum TopRequest { Switch, ReleaseAll, Update(UpdateButton), DismissError }`, `pub fn status_bar(ui: &mut Ui, line: &StatusLine, square: Color32) -> Option<TopRequest>`, `pub fn update_line(ui: &mut Ui, banner: &Banner) -> Option<TopRequest>`, `pub fn notice_line(ui: &mut Ui, notice: &Notice) -> Option<TopRequest>`, `pub fn stats_row(ui: &mut Ui, snapshot: &Snapshot, lines: &Sparklines, rows: [usize; 2])`.
  - `ui.rs`: `Window` gains `sparklines: Sparklines`. Its existing `follow_update` keeps the relaunch and nudge logic and returns `Option<Banner>` for `top::update_line`.

- [ ] **Step 1: Write the failing tests in `src/app/sparkline.rs`**

```rust
#[test]
fn one_point_a_second_and_never_more_than_sixty() {
    let (mut line, t) = (Sparkline::default(), Instant::now());
    line.push(Some(6.0), t);
    line.push(Some(9.0), t + Duration::from_millis(200));
    assert_eq!(line.points().len(), 1);
    for s in 1..=100 { line.push(Some(s as f32), t + Duration::from_secs(s)); }
    assert_eq!(line.points().len(), POINTS);
    assert_eq!(*line.points().back().unwrap(), 100.0);
}

#[test]
fn a_gap_with_no_repaint_repeats_the_last_value() {
    let (mut line, t) = (Sparkline::default(), Instant::now());
    line.push(Some(6.0), t);
    line.push(Some(8.0), t + Duration::from_secs(5));
    assert_eq!(line.points().iter().copied().collect::<Vec<_>>(), [6.0, 6.0, 6.0, 6.0, 6.0, 8.0]);
}

#[test]
fn a_figure_that_goes_unknown_freezes_the_line() {
    let (mut line, t) = (Sparkline::default(), Instant::now());
    line.push(Some(6.0), t);
    line.push(None, t + Duration::from_secs(1));
    assert!(line.frozen());
    assert_eq!(line.points().len(), 1);
    line.push(Some(7.0), t + Duration::from_secs(3));
    assert!(!line.frozen());
    assert_eq!(*line.points().back().unwrap(), 7.0);
}

#[test]
fn the_path_fills_the_box_newest_on_the_right() {
    let (mut line, t) = (Sparkline::default(), Instant::now());
    for s in 0..60 { line.push(Some(if s == 59 { 40.0 } else { 0.0 }), t + Duration::from_secs(s)); }
    let path = line.path([210.0, 36.0], 20.0);
    assert_eq!(path.len(), 60);
    assert_eq!(path.last().unwrap()[0], 210.0);
    assert!(path.iter().all(|p| (0.0..=36.0).contains(&p[1])));
    assert!(path.last().unwrap()[1] < path[0][1], "the high point is drawn higher");
}

#[test]
fn unpairing_clears_every_line() {
    let mut lines = Sparklines::default();
    let t = Instant::now();
    lines.sample(&link(secure(linked(), "encrypted"), 6, 0.4), t);
    lines.sample(&unpaired(), t + Duration::from_secs(1));
    assert!(STATS.iter().all(|s| lines.get(*s).points().is_empty()));
}
```

- [ ] **Step 2: Write the failing drawing tests in `src/app/ui.rs`** (they use the existing headless `frame` and `drawn_text` helpers)

```rust
#[test]
fn the_top_shows_the_status_line_and_every_stat_label() {
    let (ctx, mut window, s) = (context(), window(), secure(linked(), "encrypted"));
    frame(&ctx, &mut window, &s, WINDOW_SIZE, vec![], Instant::now());
    let text = drawn_text(&frame(&ctx, &mut window, &s, WINDOW_SIZE, vec![], Instant::now()));
    for line in ["Control", "Debian", "Switch to Laptop", "Release all", "ROUND TRIP", "LOST", "AUDIO LOST", "AUDIO BUFFER", "LINK", "Secure"] {
        assert!(text.contains(line), "{line}\n{text}");
    }
}

#[test]
fn unpaired_shows_dashes_and_no_switch() {
    // text contains "Not paired." and "Everything stays on each device." and "Not paired" and "--" four times;
    // it doesn't contain "Switch to".
}
```

Port `the_update_line_fits_the_smallest_window_with_its_button_and_link`, `the_line_stays_while_the_service_it_is_updating_is_gone`, `a_device_dropping_out_during_an_update_raises_no_alarm` and `an_old_peer_shows_the_update_notice_with_the_other_device_offline` to the new lines. They keep their names and assertions, with `Release all input` replaced by `Release all`.

- [ ] **Step 3: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::sparkline app::ui`
Expected: FAIL.

- [ ] **Step 4: Implement `sparkline.rs`, the widgets and `top.rs`, and wire them into `Window::show`**

These are top panels, in this order: `status bar` (fill `SURFACE`, inner margin 12/20, 1px `LINE` below), `update` (only while `follow_update` returns a banner: `SURFACE`, text and `Release notes` link on the left, `flat_button(Primary)` on the right, a `DANGER` square when `failed`), one `notice N` per `view::notices` entry, and `stats` (`SURFACE`, `stat_rows[0]` equal columns then `stat_rows[1]`, 1px `LINE` dividers, cell padding 10/20/12). A stat cell is the label in `Type::Label`, the figure in `Type::Stat` with `tone(value.tone)`, and the sparkline as a `SPARKLINE_STROKE` `ACCENT` line in `SPARKLINE_SIZE`, hidden while it has no points, and with the last point drawn as a hollow 3px square while frozen. `status_square` uses `theme::tone(line.square)` for now (Task 10 adds the cross-fade). `TopRequest::Switch` calls `client.master_action("toggle_focus")`, `ReleaseAll` calls `client.release_all_input()`, `Update(b)` calls `client.local_action(here, b.action())`, and `DismissError` calls `client.dismiss_error()`. Delete `Window::header`, `update_strip`, `notice_strip` and `device_bay`. Keep `settings_menu` reachable through a plain `Settings` menu button at the right end of the status bar until Task 7 removes it, so pairing, the address, App size, updates and Licences stay usable in between.

- [ ] **Step 5: Run the tests and the Windows check**

Run: `env -u CARGO_TARGET_DIR cargo test --release`, then the Windows check.
Expected: PASS. The Settings menu tests (`menu_text` and its users) still pass, as the menu is still there.

- [ ] **Step 6: Commit**

```bash
git add src/app/sparkline.rs src/app/top.rs src/app/widgets.rs src/app/ui.rs src/app/mod.rs
git commit -m "Status bar, update and notice lines, and the stats row with sparklines"
```

---

### Task 4: Saving state and refusals for each row

**Files:**
- Modify: `src/app/model.rs` (`Edits`, and `Picks` moved here from `panels.rs`), `src/app/client.rs`, `src/app/ui.rs` (`send_changes` uses `send_control`), `src/app/panels.rs` (uses `model::Picks`)

**Interfaces:**
- Produces:
  - `impl Edits { pub fn is_saving(&self, key: (Device, Control), polled: f64, now: Instant) -> bool; pub fn forget(&mut self, key: (Device, Control)); }`
  - `#[derive(Default)] pub struct Picks` with `pub fn set(&mut self, key: (Device, DeviceRole), id: String, now: Instant)` and `pub fn shown(&self, key: (Device, DeviceRole), polled: &str, now: Instant) -> String`, held for the existing `PICK_HOLD` (1 s, moved from `panels.rs`).
  - `impl Client { pub fn send_control(&self, device: Device, control: Control, value: f64); pub fn row_error(&self, key: (Device, Control)) -> Option<String>; pub fn take_refused(&self) -> Vec<(Device, Control)>; }`. `Job` gains `row: Option<(Device, Control)>`, and `Shared` gains `row_errors: Mutex<HashMap<(Device, Control), (String, Instant)>>` and `refused: Mutex<Vec<(Device, Control)>>`.

- [ ] **Step 1: Write the failing tests**

In `src/app/model.rs`:

```rust
#[test]
fn a_value_is_saving_until_the_poll_shows_it() {
    let (mut edits, t, key) = (Edits::default(), Instant::now(), (Device::Windows, Control::KeyboardSharing));
    edits.set(key, 1.0, t);
    assert!(edits.is_saving(key, 0.0, t));
    assert!(!edits.is_saving(key, 1.0, t));
    edits.take_due(t);
    let later = t + HOLD + Duration::from_millis(1);
    assert!(!edits.is_saving(key, 0.0, later));
    assert_eq!(edits.shown(key, 0.0, later), 0.0, "an answer that never came snaps back once the hold is over");
}

#[test]
fn forgetting_an_edit_snaps_back_at_once() {
    let (mut edits, t, key) = (Edits::default(), Instant::now(), (Device::Debian, Control::Autostart));
    edits.set(key, 1.0, t);
    edits.forget(key);
    assert_eq!(edits.shown(key, 0.0, t), 0.0);
    assert!(edits.take_due(t).is_empty());
}

#[test]
fn a_new_pick_shows_for_a_second_then_the_poll_wins() {
    // Picks::set then shown == the pick before PICK_HOLD and == polled after it.
}
```

In `src/app/client.rs`, using the existing `stub`:

```rust
#[test]
fn a_refused_control_is_kept_for_its_row_and_not_the_window() {
    // stub answers /api/local with a debian status and /api/local/action with {"ok":false}.
    // client.send_control(Device::Debian, Control::Autostart, 1.0); wait for the log to show the POST.
    // assert client.row_error((Debian, Autostart)) is Some and contains "didn't accept that change";
    // assert client.last_error() is None;
    // assert client.take_refused() == [(Debian, Autostart)] and is empty when taken again.
}

#[test]
fn a_control_that_works_clears_its_rows_error_after_the_hold() {
    // start_holding with a 0 ms error hold; refuse once, then accept; row_error is None.
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::model app::client`
Expected: FAIL.

- [ ] **Step 3: Implement**

`is_saving` is true while an entry exists for the key, is unsent or still held, and its value differs from `polled`. `send_control` sends `control.action(value)` tagged with the row. The send thread routes a tagged problem into `row_errors` and `refused` instead of `last_error`, and clears that row's error on a later success once `error_hold` has passed, the same rule `last_error` follows now. `send_changes` in `ui.rs` calls `send_control`, and each frame calls `edits.forget(key)` for every key in `client.take_refused()`.

- [ ] **Step 4: Run the tests**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app/model.rs src/app/client.rs src/app/ui.rs src/app/panels.rs
git commit -m "Saving and refused state for each setting, kept with its row"
```

---

### Task 5: Settings rows for each device

**Files:**
- Create: `src/app/rows.rs`
- Modify: `src/app/mod.rs`

**Interfaces:**
- Consumes: `Snapshot`, `LayoutState`, `Edits`, `Picks`, `Control`, `DeviceRole`, `display_for`, `monitor_label`, `size_label`, `resolutions`, `rates`, `rate_label`, `mode_for_size`, `device_label`, `bitrate_label`, `sending_quality`, `receiving_quality`, `game_lock_hint` from `model.rs`, `crate::audio::BITRATES`, `panels::{MIN_POINTER_SPEED, MAX_POINTER_SPEED}` (moved into `rows.rs`), and `view::fit_name`.
- Produces, in `src/app/rows.rs` (no egui types):
  - `pub enum Group { Input, Screens, Clipboard, Audio, Games, Startup, BothDevices, Updates }` with `pub fn heading(self) -> &'static str` (`INPUT` … `BOTH DEVICES`, `UPDATES`) and `pub const ORDER: [Group; 8]`.
  - `pub struct GroupRows { pub group: Group, pub rows: Vec<Row>, pub notes: Vec<Note> }`, `pub struct Note { pub text: String, pub tone: Tone }`, `pub struct Row { pub label: String, pub cells: [Cell; 2], pub error: Option<String> }` (cells in `snapshot.in_order()`).
  - `pub struct Cell { pub value: String, pub tone: Tone, pub kind: CellKind, pub read_only: Option<String>, pub tooltip: Option<String>, pub saving: bool, pub monitor: Option<(Device, String)> }` and `Cell::blank()`.
  - `pub enum CellKind { Blank, Plain, Toggle(Setting), Number { setting: Setting, value: f64, range: RangeInclusive<f64>, step: f64 }, Dropdown { pick: Pick, options: Vec<Choice> }, Name, Level(u32), Action(ActionKind) }`.
  - `pub enum Setting { Device(Control), EdgePush, AutoGameLock, ManualGameLock }`, `pub enum Pick { Audio(DeviceRole), Bitrate, Mode(String), Placement, UiScale }`, `pub struct Choice { pub label: String, pub value: ChoiceValue, pub selected: bool }`, `pub enum ChoiceValue { Id(String), Kbps(u32), Mode(DisplayMode), Placement(Placement), Scale(f32) }`.
  - `pub enum ActionKind { TestSound, MakeMain, ChangeAddress, Pair, Unpair, ResetSizes, CheckUpdate, ShowLicences }`.
  - `pub struct RowsInput<'a> { pub snapshot: &'a Snapshot, pub layout: Option<&'a LayoutState>, pub edits: &'a Edits, pub picks: &'a Picks, pub on: &'a ControlsEnabled, pub row_error: &'a dyn Fn((Device, Control)) -> Option<String>, pub ui_scale: f32, pub main_changing: bool, pub main_error: Option<&'a str>, pub unpairing: bool, pub now: Instant }`.
  - `pub fn groups(input: &RowsInput) -> Vec<GroupRows>`. This task fills INPUT through STARTUP. BOTH DEVICES and UPDATES come in Task 6.
  - Cell values: toggles `ON` (`Accent`) or `OFF` (`Dim`). Pointer speed `{:.2}×`. Volumes `{:.0}%`. A dropdown's value ends in ` ▾`. A cell for a device with no status is `Cell::blank()`.
  - Read-only reasons: `Only <Device> can choose its microphone.`, `<Device> is running an older mesh-kvm. Update it to change this.` (for `is_older`), the existing `CANT_CHANGE` text for a screen with no working mode list, and `Wait for the screen change to finish.` while `snapshot.mode_change()` is waiting.

- [ ] **Step 1: Write the failing tests in `src/app/rows.rs`**

```rust
fn labels(groups: &[GroupRows], group: Group) -> Vec<String> {
    groups.iter().find(|g| g.group == group).map(|g| g.rows.iter().map(|r| r.label.clone()).collect()).unwrap_or_default()
}

#[test]
fn every_device_setting_has_a_row_in_the_boards_order() {
    let g = groups(&input(&linked()));
    assert_eq!(labels(&g, Group::Input), ["Share mouse", "Share keyboard", "Pointer speed on the other device"]);
    assert_eq!(labels(&g, Group::Clipboard), ["Send clipboard", "Receive clipboard"]);
    assert_eq!(labels(&g, Group::Audio), [
        "Output to send", "Microphone or line-in", "Play only on the other device", "Send volume", "Audio quality",
        "Level sent", "Play audio from the other device", "Play received audio on", "Playback volume",
        "Level received", "Test sound",
    ]);
    assert_eq!(labels(&g, Group::Games), ["Hold the mouse while a game runs", "Lock the mouse here now"]);
    assert_eq!(labels(&g, Group::Startup), ["Start when this device boots"]);
}

#[test]
fn toggles_read_on_and_off_in_their_colours() { /* Laptop keyboard sharing off: value "OFF", Tone::Dim; Debian "ON", Tone::Accent */ }

#[test]
fn numbers_and_dropdowns_read_as_on_the_board() { /* "0.35×", "100%", "128 kbit/s ▾" */ }

#[test]
fn the_other_devices_microphone_is_read_only_and_says_why() {
    let g = groups(&input(&linked()));
    let mic = row(&g, "Microphone or line-in");
    assert_eq!(mic.cells[0].read_only, None);
    assert_eq!(mic.cells[1].read_only.as_deref(), Some("Only Laptop can choose its microphone."));
}

#[test]
fn a_device_that_is_not_connected_has_a_blank_column() {
    for group in groups(&input(&unpaired())).iter().filter(|g| g.group != Group::BothDevices && g.group != Group::Updates) {
        for r in &group.rows { assert!(matches!(r.cells[1].kind, CellKind::Blank), "{}", r.label); }
    }
}

#[test]
fn game_locks_apply_only_on_this_device() { /* cells[1] Blank for both rows */ }

#[test]
fn an_older_service_cannot_be_changed_and_says_why() { /* peer.sharing = None: Share mouse cells[1].read_only = "Laptop is running an older mesh-kvm. Update it to change this." */ }

#[test]
fn one_screen_each_reads_resolution_and_refresh_rate() { /* single-monitor STATE: ["Resolution", "Refresh rate"], values "2560×1440 ▾" style, tooltip names the monitor */ }

#[test]
fn several_screens_are_numbered_and_a_missing_one_is_blank() {
    // fixtures::STATE has two Debian screens and one Laptop screen:
    // ["Screen 1 resolution", "Screen 1 refresh rate", "Screen 2 resolution", "Screen 2 refresh rate"],
    // and the Laptop cells of screen 2 are Blank.
}

#[test]
fn a_screen_that_cannot_change_shows_its_mode_read_only() { /* can_change false: Plain "1920×1080", read_only == CANT_CHANGE */ }

#[test]
fn notes_name_their_device_and_stand_out_when_something_is_wrong() {
    // Laptop clipboard note "Couldn't read the clipboard" with note_failed: CLIPBOARD notes contain
    // Note { text: "Laptop: Couldn't read the clipboard", tone: Tone::Warn }; sending_quality lines are Tone::Dim.
}

#[test]
fn a_value_being_saved_is_marked_and_a_refusal_sits_under_its_row() {
    // edits.set((Windows, KeyboardSharing), 1.0, now): cells[1].saving and value "ON";
    // row_error returning Some("Laptop didn't accept that change. Try again, or restart mesh-kvm on it.") for that key: row.error is that text.
}
```

`input(&Snapshot) -> RowsInput` is a test helper with default `Edits`, `Picks` and `ControlsEnabled` from `model::enabled`, `ui_scale` 1.0, and `row_error` always `None`.

- [ ] **Step 2: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::rows`
Expected: FAIL.

- [ ] **Step 3: Implement INPUT through STARTUP in `groups`**

Values come from `edits.shown(key, control.read(status), now)` and `picks.shown(...)`. Mode choices come from `resolutions` and `rates`, with `mode_for_size` behind each resolution option, as in `DevicePanel::mode_menus`. Notes are collected in the order the panels showed them.

- [ ] **Step 4: Run the tests**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::rows`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app/rows.rs src/app/mod.rs
git commit -m "Settings rows for each device, with read-only reasons and notes"
```

---

### Task 6: Rows for both devices and updates

**Files:**
- Modify: `src/app/rows.rs`, `src/app/ui.rs` (`UI_SCALES` becomes `pub(super)`, and the ignored Settings menu tests are deleted)

**Interfaces:**
- Consumes: `model::{placements, position_label, paired_line, Placement}`, `LayoutConfig::has_custom_size`, `ui::UI_SCALES`, `UNKNOWN_ADDRESS`, `PairingStatus::fingerprint`, `UpdateStatus::{current, note}`.
- Produces: BOTH DEVICES and UPDATES in `groups`, with these rows:
  - BOTH DEVICES: `Name` (`CellKind::Name` for each device, tooltip `Type here to rename this device. Leave it empty to use its network name.`), `Main device`, `Address`, `Where <Windows name> sits` (this column only), `Push needed to cross` (`Number { setting: EdgePush, range: 10.0..=180.0, step: 5.0 }`, this column), `Use the real screen sizes` (only while `has_custom_size`, action `ResetSizes`), then the pairing row (below), then `App size` (dropdown over `UI_SCALES` as `{:.0}%`, tooltip `Ctrl and plus or minus do the same.`).
  - Main device: `MAIN` (`Accent`) in the main device's cell, `Make main` (`Dim`, `MakeMain`) in the other. While not `on.main_device`, read-only `The main device can be chosen while the devices are linked and on the same version.` While `main_changing`, the chosen cell reads `Changing…` with `saving`. `main_error` goes to `row.error`.
  - Address: this device's `ip`, `Plain`. The other cell shows `local.peer_ip` with `ChangeAddress`, or `Not set` in `Warn` when it is empty or `UNKNOWN_ADDRESS`. While `peer_pinned`, read-only `How mesh-kvm starts on this device sets the address, so it can't be changed here.`
  - Pairing row: no row while `secure` is empty. Encrypted or connecting: label `Paired, fingerprint`, this cell the fingerprint (`--` in `Dim` until known), the other cell `Unpair` (`Unpairing…` with `saving` while `unpairing`), and the note `paired_line(secure, other)` while connecting. Unpaired: label `Not paired`, this cell `Pair` (`Accent`, `Pair`), the other cell blank.
  - UPDATES: `Version` (each device's `update.current`, else its `version`), `Check for updates` (this cell `Check now`, `CheckUpdate`, with `update.note` as a group note), `Licences` (this cell `Show`, `ShowLicences`).

- [ ] **Step 1: Write the failing tests in `src/app/rows.rs`**

```rust
#[test]
fn every_setting_has_a_row() {
    let mut s = secure(linked(), "encrypted");
    s.pairing = Some(PairingStatus { state: "paired".into(), fingerprint: Some("4F2A-91C3".into()), ..Default::default() });
    let g = groups(&input(&s));
    assert_eq!(g.iter().map(|g| g.group).collect::<Vec<_>>(), ORDER);
    assert_eq!(labels(&g, Group::BothDevices), ["Name", "Main device", "Address", "Where Laptop sits", "Push needed to cross", "Paired, fingerprint", "App size"]);
    assert_eq!(labels(&g, Group::Updates), ["Version", "Check for updates", "Licences"]);
    let paired = row(&g, "Paired, fingerprint");
    assert_eq!((paired.cells[0].value.as_str(), paired.cells[1].value.as_str()), ("4F2A-91C3", "Unpair"));
}

#[test]
fn unpaired_offers_pair_with_the_other_column_empty() {
    let g = groups(&input(&unpaired()));
    let r = row(&g, "Not paired");
    assert!(matches!(r.cells[0].kind, CellKind::Action(ActionKind::Pair)));
    assert!(matches!(r.cells[1].kind, CellKind::Blank));
}

#[test]
fn the_main_device_reads_main_and_the_other_can_be_made_main() { /* "MAIN" Accent / "Make main" Dim; main_error lands in row.error */ }

#[test]
fn a_missing_address_says_not_set_and_a_pinned_one_says_why() { /* "Not set" Tone::Warn; peer_pinned read_only text */ }

#[test]
fn custom_screen_sizes_offer_the_real_ones() { /* has_custom_size adds "Use the real screen sizes" before the pairing row */ }

#[test]
fn a_service_that_does_not_encrypt_has_no_pairing_row() { /* secure "" */ }
```

The `ui.rs` tests `the_settings_menu_shows_the_fingerprint_and_unpair_when_paired` and `the_settings_menu_offers_pair_when_unpaired` are deleted in Task 7 along with the menu. The two tests above replace them. Add `updates_show_each_version_and_the_note` here, with the same version and note assertions as `ui.rs`'s `settings_shows_the_version_and_any_note`. That one goes in Task 7 with the menu.

- [ ] **Step 2: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::rows`
Expected: FAIL.

- [ ] **Step 3: Implement the two groups**

`Where … sits` is built from `placements(&cfg, snapshot.displays(Device::Debian))` and `position_label`, as in `Window::position_control`. With no layout it reads `Not available`, read-only `The layout shows up once mesh-kvm is running on this device.`

- [ ] **Step 4: Run the tests**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app/rows.rs src/app/ui.rs
git commit -m "Rows for both devices and for updates, covering everything the Settings menu had"
```

---

### Task 7: The settings table and the window's body

**Files:**
- Create: `src/app/table.rs`
- Modify: `src/app/ui.rs` (body layout, applying `TableRequest`s, the no-service body, `WINDOW_SIZE`), `src/app/mod.rs`
- Delete: `src/app/panels.rs`, and from `src/app/widgets.rs` everything only the panels used (`toggle_row`, `slider_row`, `meter`, `segment`, `wide_button`, `slide_switch`, `paint_light`, `paint_name_and_state`)

**Interfaces:**
- Consumes: `rows::{groups, GroupRows, Row, Cell, CellKind, Setting, Pick, ChoiceValue, ActionKind, RowsInput}`, `view::{frame_layout, label_width, fit_name, STACKED_CANVAS_HEIGHT}`, `Canvas::{selected, hovered_in_controls, just_selected}`, `Client::row_error`.
- Produces:
  - `#[derive(Default)] pub struct Table` (it holds the `NameEdit`s moved from `panels.rs`, with `NAME_HOLD` 3 s and `MAX_DEVICE_NAME`).
  - `pub enum TableRequest { Set(Device, Control, f64), Number(Setting, f64), Toggle(Setting), Pick(Device, Pick, ChoiceValue), Rename(Device, String), Action(Device, ActionKind), Hover(Option<(Device, String)>) }`. A `Setting::Device(control)` cell, toggle or number, always sends `Set`. `Number` and `Toggle` are only for `EdgePush`, `AutoGameLock` and `ManualGameLock`.
  - `impl Table { pub fn show(&mut self, ui: &mut Ui, groups: &[GroupRows], header: [String; 2], label_width: f32, selected: Option<&(Device, String)>, scroll_to_selected: bool, now: Instant) -> Vec<TableRequest> }`.
  - In `ui.rs`: `pub(super) const WINDOW_SIZE: [f32; 2] = [1280.0, 760.0]` (`MIN_WINDOW_SIZE` stays `[760.0, 560.0]`), `fn body(&mut self, ctx, snapshot, on, now)` and `fn apply_table(&mut self, ctx, snapshot, requests: Vec<TableRequest>, now)`.

- [ ] **Step 1: Write the failing tests in `src/app/ui.rs`**

```rust
#[test]
fn the_table_header_names_both_devices_and_the_groups_follow() {
    // drawn text has "SETTING", "DEBIAN", "LAPTOP", then "INPUT" … "UPDATES" in ORDER.
}

#[test]
fn the_settings_column_is_700_wide_and_the_layout_takes_the_rest() {
    // at 1280×760: the "settings" panel rect width == 700 (ctx.read_response or memory area);
    // monitor rects (monitor_id) all have left() >= 700.
}

#[test]
fn a_narrow_window_stacks_the_layout_under_the_settings() {
    // at 899×760: every monitor rect top() is below the last settings row; stats draw 3 labels on the first line.
}

#[test]
fn clicking_a_toggle_cell_flips_it_and_sends_once() {
    // click the centre of the Debian "Share mouse" cell; window.edits.shown((Debian, MouseSharing), 1.0, now) == 0.0.
}

#[test]
fn a_refused_toggle_snaps_back_and_says_why_under_its_row() {
    // Point the window's Client (Client::start_on) at a TcpListener written in the test that answers
    // /api/local with a minimal debian status JSON (as in fixtures::device) and /api/local/action with {"ok":false}.
    // Click a toggle, then run frames until
    // the refusal arrives: the shown value is the polled one again, and the drawn text has
    // "didn't accept that change" exactly once.
}

#[test]
fn long_names_never_widen_a_device_column() {
    // a 32-character device name and a 60-character audio device name: header text ends with "…",
    // and no text shape of a device cell is wider than DEVICE_COLUMN.
}

#[test]
fn with_no_service_the_body_says_why() {
    // ServiceStart::Failed("mesh-kvm couldn't start on this device. See /tmp/x.log") with Snapshot::default():
    // the text appears in the body and the status square is DANGER.
}
```

Carry these over with their assertions, adjusted only for the new geometry: `draws_every_state_from_the_smallest_window_to_full_screen` (add `[899.0, 760.0]` and `[1280.0, 599.0]` to its sizes), `the_arrangement_fills_the_height_and_stays_clear_of_the_controls` (the bound is `settings_width`, not `rail`), `main_device_runs_off_the_ui_thread_and_shows_its_error`, `main_error_goes_when_the_link_recovers_or_the_choice_takes_hold`, `edits_are_sent_on_the_frame_they_become_due`, `device_details_name_this_device` (now the `Name` cell tooltip: hostname and address), `keep_is_offered_only_on_the_device_with_the_screen`, `a_screen_with_no_working_mode_list_shows_its_mode_and_says_so`, `a_failed_start_is_shown_and_starting_keeps_the_window_looking`, `the_window_is_idle_when_nothing_changes`.

- [ ] **Step 2: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::ui`
Expected: FAIL.

- [ ] **Step 3: Implement `Table::show`**

It draws the header grid (`minmax(label) 130 130`, `Type::Head`, the device names from `fit_name` upper-cased, 6px below and a 1px `LINE_STRONG` rule), then each group's `Type::Label` heading with 10px above it, then the rows. Each row is at least `ROW_MIN_HEIGHT` with a 1px `LINE_FAINT` rule. The label wraps in `label_width`. Each cell is `DEVICE_COLUMN` wide, cut with `…` and the whole value in a tooltip.

- Toggle cells: the whole cell is clickable (`Sense::click`), with a `RAISED` row background on hover.
- Saving cells are drawn at 60% opacity (`gamma_multiply(0.6)`).
- Read-only cells are `TEXT_DIM`, don't respond to hover, and carry `read_only` as their tooltip.
- Dropdowns: an `egui::ComboBox` styled flat under the cell.
- Numbers: `egui::DragValue` with the range, step and a formatter that matches the cell text.
- `Name`: the `TextEdit` and `NameEdit` logic from `Panels::heading`.
- `Level`: a 4px `ACCENT` bar on `BG`.
- Actions: text in the cell's tone, clickable.
- `row.error` goes under the row in `danger` `Type::Caption`, and group notes under the group in `Type::Caption`.
- A cell whose `monitor` is the canvas's `selected` gets a `RAISED` background, scrolls into view on `scroll_to_selected`, and sends `Hover` while hovered.

- [ ] **Step 4: Implement `body` and `apply_table` in `ui.rs`**

`frame_layout(ctx.screen_rect().width(), height)` picks the layout:

- Side by side: `SidePanel::left("settings").exact_width(settings_width)` with `SURFACE`, padding 12/20, its own vertical `ScrollArea`, and a 1px `LINE` on the right. The canvas goes in the `CentralPanel` on `BG`.
- Stacked: one `ScrollArea` holds the table, then the canvas at `STACKED_CANVAS_HEIGHT`.
- `body_scrolls`: the whole body sits inside one `ScrollArea` under the pinned top panels.

With `snapshot.local == None`, the body draws `model::start_notice` or `status_message` in `Type::Body` beside a `DANGER` square, and nothing else.

`apply_table` maps the requests onto what `Window` already does:

- `Set` goes to `edits.set`.
- `Number(EdgePush)` and `Pick(Placement)` edit `layout.state` and call `layout.edited(now)`, as `edge_push_control` and `position_control` did.
- `Toggle(AutoGameLock | ManualGameLock)` follows `send_requests`.
- `Pick(Audio)` sends `set_audio_device` and calls `picks.set`.
- `Pick(Bitrate)` goes through `edits.set(AudioBitrate)`.
- `Pick(Mode)` sends `mode_action`.
- `Pick(UiScale)` sets `ui_scale`.
- `Rename` sends `set_device_name`.
- `Action(MakeMain)` calls `choose_main`, `ChangeAddress` opens `peer_prompt`, `Pair` calls `open_pairing`, `Unpair` opens the confirmation (Task 9; until then it sends `unpair` straight away, as now), `ResetSizes` sends `reset_native_resolutions` and calls `let_poll_through`, `CheckUpdate` sends `check_update`, `ShowLicences` sets `show_licences`, and `TestSound` sends `test_audio_chime`.
- `Hover` sets `canvas.hovered_in_controls`.

Delete `panels.rs`, `controls`, `arrangement_settings`, `position_control`, `edge_push_control`, `main_device_controls`, `send_requests`, `settings_menu` with its temporary button, the tests `the_settings_menu_shows_the_fingerprint_and_unpair_when_paired`, `the_settings_menu_offers_pair_when_unpaired` and `settings_shows_the_version_and_any_note` with `menu_text` (Task 6's rows tests cover them now), `pairing_settings`, `pairing_in_menu` (the window now asks for pairing while `secure` isn't empty or the dialog is open), and `model::body_layout` with its test.

- [ ] **Step 5: Run the tests and the Windows check**

Run: `env -u CARGO_TARGET_DIR cargo test --release`, then the Windows check.
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/app/table.rs src/app/ui.rs src/app/widgets.rs src/app/model.rs src/app/mod.rs
git rm src/app/panels.rs
git commit -m "Every setting for both devices in one table, with the layout beside it"
```

---

### Task 8: The layout canvas, restyled

**Files:**
- Modify: `src/app/canvas.rs`, `src/app/model.rs` (`tile_view`, `nudge`, `tile_caption`), `src/app/ui.rs` (tests)

**Interfaces:**
- Consumes: `model::{fit, seams, drag_device, cursor_position, display_for, MapView, LayoutConfig}`, `theme::{tile_fill, BG, TEXT, TEXT_DIM, LINE_STRONG, ACCENT, Type}`.
- Produces:
  - `pub const MIN_TILE_WIDTH: f32 = 80.0`, and `pub fn tile_view(cfg: &LayoutConfig, width: f32, height: f32) -> (MapView, [f32; 2])` (the view, plus the content size, which is larger than the area when tiles would be under 80 wide), in `model.rs`.
  - `pub fn nudge(cfg: &mut LayoutConfig, device: Device, dx: i32, dy: i32)` in `model.rs` (moves the device box through `drag_device` with magnet 0).
  - `pub fn tile_caption(rect: &Display, display: Option<&DisplayInfo>) -> String`, giving `DP-1 2560×1440` (the output id, else the layout name before ` (`, then `size_label`).
  - `Canvas::show` keeps its signature. `monitor_id` stays.

- [ ] **Step 1: Write the failing tests**

In `src/app/model.rs`:

```rust
#[test]
fn small_screens_never_draw_under_80_wide() {
    let cfg = six_monitor_config();
    let (view, content) = tile_view(&cfg, 300.0, 200.0);
    let narrowest = cfg.devices.iter().flat_map(LayoutDevice::rects).map(|m| m.w as f32 * view.scale).fold(f32::MAX, f32::min);
    assert!(narrowest >= MIN_TILE_WIDTH - 0.01);
    assert!(content[0] > 300.0);
    let (roomy, content) = tile_view(&cfg, 4000.0, 2000.0);
    assert_eq!(roomy, fit(&cfg, 4000.0, 2000.0));
    assert_eq!(content, [4000.0, 2000.0]);
}

#[test]
fn a_nudge_moves_the_device_and_keeps_it_touching() {
    let mut cfg = LayoutState::parse(fixtures::STATE).unwrap().config;
    let (x, y, ..) = cfg.device_box(Device::Windows);
    nudge(&mut cfg, Device::Windows, 10, 0);
    assert_eq!(cfg.device_box(Device::Windows).0, x + 10);
    nudge(&mut cfg, Device::Windows, 0, -1);
    assert_eq!(cfg.device_box(Device::Windows).1, y, "it can't move into the other device, so it stays on the edge");
}

#[test]
fn a_tile_reads_its_output_and_size() { /* "DP-2 2560×1440", and "Display 1 2560×1600" */ }
```

`six_monitor_config()` is the layout `tests/fake_service.py --monitors 6` serves, written in the test module as JSON.

In `src/app/ui.rs`:

```rust
#[test]
fn the_canvas_shows_its_label_and_hint() {
    // drawn text contains "LAYOUT" and "Drag a screen to move it. The bright edge is where the mouse crosses."
}

#[test]
fn an_arrow_key_moves_the_selected_screen_by_ten_and_shift_by_one() {
    // click Laptop's monitor (selects and focuses it), send Event::Key ArrowRight, then Shift+ArrowRight:
    // box_of(&window, Device::Windows).0 grows by 10, then by 1; layout.take_due returns an edit.
}

#[test]
fn the_selected_tile_has_a_text_border_and_the_crossing_edge_is_accent() {
    // after selecting Laptop's monitor, output.shapes has a Rect stroke of TEXT around it and a
    // LineSegment or Rect of ACCENT with width CROSSING_EDGE on the seam.
}
```

`dragging_a_monitor_snaps_its_device_and_ignores_the_poll_until_release`, `every_monitor_of_both_devices_can_be_dragged`, `a_click_on_a_monitor_selects_it_and_does_not_move_it` and `a_drag_that_loses_its_release_still_ends` keep passing unchanged.

- [ ] **Step 2: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::model app::ui`
Expected: FAIL.

- [ ] **Step 3: Implement**

`Canvas::show` draws on `BG`:

- `LAYOUT` in `Type::Label` at (24, 20) and the hint in `Type::Caption` `TEXT_DIM` at (24, 44). The map area starts below the hint, with 24 around it.
- With no layout, the existing `Your screens show up here once mesh-kvm is running on this device.` sentence in `TEXT_DIM`.
- Tiles: `tile_fill(device == Debian)`, 1px border `TEXT` when selected (or hovered in the table, or any tile of a device that is hovered or held), `LINE_STRONG` otherwise, padding 10. The device title (`fit_name`) in `strong(14)`, and `tile_caption` in `mono(12)` `TEXT_DIM`, both clipped to the tile.
- Seams: 4px `ACCENT` bars.
- The pointer: a small `TEXT` arrow outlined in `BG`.
- When `tile_view`'s content is larger than the area, everything sits inside `ScrollArea::both()`.
- A focused tile shows `focus_ring`. While it has focus, ArrowLeft, ArrowRight, ArrowUp and ArrowDown (consumed with `consume_key`) call `nudge` by 10 (Shift: 1) and then `draft.edited(now)`.

Remove `paint_glow`, `paint_tags`, `Screens`, `screens_state` and the `legacy` imports.

- [ ] **Step 4: Run the tests**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app/canvas.rs src/app/model.rs src/app/ui.rs
git commit -m "Flat layout canvas with tinted tiles, a minimum tile size and arrow-key moves"
```

---

### Task 9: Dialogs, pairing and the code input

**Files:**
- Create: `src/app/dialog.rs`
- Modify: `src/app/pairing.rs`, `src/app/ui.rs` (`peer_window`, `mode_prompt`, `licences_window`, the unpair confirmation, `pairing_window`), `src/app/mod.rs`

**Interfaces:**
- Consumes: `widgets::{flat_button, ButtonKind}`, `theme::{DIALOG_WIDTH, RAISED, LINE_STRONG, BG, ACCENT, DANGER, Type, COUNTDOWN_HEIGHT}`, `CODE_SECONDS` (120), `PAIRED_SHOWN` (2 s).
- Produces:
  - `dialog.rs`: `pub fn origin(screen: Vec2, height: f32) -> Pos2` (x = (w − 460) / 2; y = h × 0.25, moved up to fit with a 16 margin). `pub struct Shown<R> { pub inner: R, pub escaped: bool }`. `pub fn show<R>(ctx: &egui::Context, id: egui::Id, title: &str, opacity: f32, body: impl FnOnce(&mut Ui) -> R) -> Shown<R>`. It uses `egui::Modal` (backdrop `BG` at 60% alpha) with an `Area` at `origin`, frame `RAISED`, 1px `LINE_STRONG`, padding 18/20/20, item spacing 12, the title in `Type::Label`, and Esc reported as `escaped`. `pub fn buttons(ui: &mut Ui, add: impl FnOnce(&mut Ui))` right-aligns its buttons with a gap of 8.
  - `pairing.rs`: `pub fn code_shown(typed: &str) -> String`, `pub fn code_digits(shown: &str) -> String`, `pub fn spoken_code(code: &str) -> String` (`4 8 2 9 1 7`), and `pub const WRONG_CODE_TEXT` (a function of the other device's name): `A wrong code can't be used again. Show a new one on <Other> and try once more.` `PairingInput::clean` is replaced by `code_shown`, and `ready()` counts digits. `pub fn window(ctx, view: &PairingView, input: &mut PairingInput, opacity: f32) -> Option<PairingRequest>`. `PairingView` gains `pub left_since_poll: Duration`. `PairingInput` gains `pub refocus: bool`.
  - `ui.rs`: `Window` gains `unpair_confirm: bool`.

- [ ] **Step 1: Write the failing tests**

In `src/app/dialog.rs`:

```rust
#[test]
fn a_dialog_sits_a_quarter_down_and_centred() {
    assert_eq!(origin(vec2(1280.0, 760.0), 300.0), pos2(410.0, 190.0));
    assert_eq!(origin(vec2(760.0, 560.0), 500.0), pos2(150.0, 44.0));
}
```

In `src/app/pairing.rs` (replacing `the_code_box_takes_digits_only_with_or_without_a_space`, keeping its cases):

```rust
#[test]
fn the_code_box_formats_typing_and_pasting() {
    assert_eq!(code_shown("482"), "482");
    assert_eq!(code_shown("4829"), "482 9");
    assert_eq!(code_shown("482917"), "482 917");
    assert_eq!(code_shown("482 917"), "482 917");
    assert_eq!(code_shown("48a2-9 17x"), "482 917");
    assert_eq!(code_shown("4829179999"), "482 917");
    assert_eq!(code_shown("\u{664}\u{665}12"), "12");
    assert_eq!(code_digits("482 917"), "482917");
    assert!(code_shown("482 917").chars().count() <= 7);
}

#[test]
fn backspace_goes_back_through_the_space() {
    // "482 9" with its last character removed is "482 ", which shows as "482"; removing again gives "48".
    assert_eq!(code_shown("482 "), "482");
    assert_eq!(code_shown("48"), "48");
}

#[test]
fn showing_draws_the_board() {
    // view other "Laptop", showing 482917 with 94 s left: text has "PAIR WITH LAPTOP",
    // "Open mesh-kvm on Laptop, choose Enter a code, and type:", "482 917", "1:34 LEFT, ONE TRY",
    // "WAITING FOR LAPTOP", "Cancel", and no "Pair" button.
}

#[test]
fn entering_draws_the_board_and_pair_waits_for_six_digits() {
    // "PAIR WITH DEBIAN", "Type the code Debian is showing.",
    // "Use this device's own keyboard. The devices can't share one until they're paired.", "Cancel", "Pair";
    // with "48291" typed, a click on Pair returns None; with "482917", Some(PairingRequest::Enter("482917")).
}

#[test]
fn a_wrong_code_is_said_in_the_dialog_and_the_box_is_cleared_for_another_go() {
    // status failed "The code was wrong. …", input.entering: text has
    // "A wrong code can't be used again. Show a new one on Laptop and try once more."; input.code is empty;
    // the Pair and Cancel buttons are still there.
}

#[test]
fn other_failures_show_the_services_own_sentence() { /* "The code ran out. Show a new one." */ }

#[test]
fn pairing_says_so_and_cancel_still_works() { /* working: "Pairing…" shown and disabled; a click on Cancel returns Cancel */ }

#[test]
fn the_choice_offers_show_and_enter_a_code() { /* "Show a code", "Enter a code", "Cancel", the address caption */ }

#[test]
fn paired_says_so_with_no_button() { /* "Paired with Laptop", "Everything between these devices is encrypted.", no "Done" */ }

#[test]
fn the_dialog_fits_the_smallest_window() { /* keep the_window_draws_at_the_smallest_size over every screen */ }

#[test]
fn escape_cancels() { /* Event::Key Escape returns Some(PairingRequest::Cancel) */ }
```

In `src/app/ui.rs`:

```rust
#[test]
fn unpair_asks_first() {
    // Action(Unpair) opens a dialog with "Unpair Laptop? The two devices stop sharing until you pair again.",
    // "Cancel" and "Unpair"; nothing is sent until Unpair is clicked; Cancel closes it.
}
```

`a_first_run_asks_for_the_address_and_a_failure_keeps_the_box_open`, `a_paired_device_shows_no_box_until_asked`, `a_waiting_mode_change_shows_the_prompt_on_either_devices_window`, `the_pair_window_opens_asks_for_the_status_and_closes_when_paired`, `a_pairing_action_never_blocks_the_window_and_keeps_it_working` and `a_refused_code_is_shown_in_the_box_it_came_from` keep their assertions. Their copy changes only where this task's titles do.

- [ ] **Step 2: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::dialog app::pairing app::ui`
Expected: FAIL.

- [ ] **Step 3: Implement**

All dialogs go through `dialog::show`:

- Pairing, with screens as in the spec and rulings 10 and 11.
- The code display: `BG`, 18 padding top and bottom, `Type::CodeShow` in `ACCENT`, centred.
- The countdown bar: `COUNTDOWN_HEIGHT`, track `BG`, fill `ACCENT` = seconds_left / `CODE_SECONDS`.
- The `M:SS LEFT, ONE TRY` and `WAITING FOR <OTHER>` row in `mono(12)` `TEXT_DIM`.
- The code input: a `TextEdit` with `Type::CodeEnter`, centred, a 1px `ACCENT` border on `BG`, and `char_limit(7)`. Each frame it rewrites `input.code = code_shown(&input.code)` with the cursor at the end. It is focused when the dialog opens and again after a failure. Enter submits when ready.
- The peer prompt: `CONNECT TO YOUR OTHER DEVICE`, with the existing copy, `Cancel` and `Connect` (primary).
- The mode prompt: `KEEP THESE SCREEN SETTINGS?`, with the existing copy, the 4px countdown, `Go back now` and `Keep this` (primary, only on the device with the screen).
- Licences: `LICENCES`, scroll at 70% of the height, and `Close`.
- The unpair confirmation: `UNPAIR`, `Cancel` and `Unpair` (Danger), with `Unpairing…` and `unpair_error` as now.

Opening a dialog focuses its input, or `Cancel` when it has none. `opacity` is 1.0 for now, and Task 10 animates it.

- [ ] **Step 4: Run the tests and the Windows check**

Run: `env -u CARGO_TARGET_DIR cargo test --release`, then the Windows check.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app/dialog.rs src/app/pairing.rs src/app/ui.rs src/app/mod.rs
git commit -m "Flat dialogs: pairing as on the boards, the code input, and asking before Unpair"
```

---

### Task 10: Motion and accessibility

**Files:**
- Create: `src/app/motion.rs`
- Modify: `Cargo.toml` and `Cargo.lock` (eframe features gain `"accesskit"`), `src/app/ui.rs`, `src/app/top.rs`, `src/app/table.rs`, `src/app/pairing.rs`, `src/app/dialog.rs`, `src/app/mod.rs`

**Interfaces:**
- Produces, in `src/app/motion.rs`:
  - `pub const DIALOG_FADE: f32 = 0.12`, `pub const SQUARE_FADE: f32 = 0.2`.
  - `pub fn reduced_motion() -> bool` (Windows: `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)` returning false; Linux: `kde_reduced(&read ~/.config/kdeglobals)`). `pub fn kde_reduced(kdeglobals: &str) -> bool`.
  - `pub fn fade_in(elapsed: f32, reduced: bool) -> f32` (ease-out over `DIALOG_FADE`, 1.0 when reduced) and `pub fn cross_fade(from: Color32, to: Color32, elapsed: f32, reduced: bool) -> Color32` (ease-in-out over `SQUARE_FADE`).
  - `pub fn countdown_share(seconds_left: u64, since_poll: Duration, reduced: bool) -> f32` (linear between polls, whole seconds when reduced).
  - `pub fn countdown_announcement(seconds_left: u64) -> Option<&'static str>` (`1 minute left`, `30 seconds left`, `10 seconds left` at 60, 30 and 10).
- `Window` gains `reduced: bool` (read once at start), `square: (Color32, Color32, Instant)` and `dialog_opened: Option<Instant>`.

- [ ] **Step 1: Write the failing tests**

In `src/app/motion.rs`:

```rust
#[test]
fn reduced_motion_drops_the_fades_and_steps_the_countdown() {
    assert_eq!(fade_in(0.0, true), 1.0);
    assert_eq!(fade_in(0.0, false), 0.0);
    assert_eq!(fade_in(DIALOG_FADE, false), 1.0);
    assert_eq!(cross_fade(WARN, ACCENT, 0.0, true), ACCENT);
    assert_eq!(cross_fade(WARN, ACCENT, SQUARE_FADE, false), ACCENT);
    assert_eq!(cross_fade(WARN, ACCENT, 0.0, false), WARN);
    assert_eq!(countdown_share(94, Duration::from_millis(500), true), 94.0 / 120.0);
    assert!((countdown_share(94, Duration::from_millis(500), false) - 93.5 / 120.0).abs() < 1e-4);
    assert_eq!(countdown_share(0, Duration::from_secs(3), false), 0.0);
}

#[test]
fn the_countdown_is_announced_three_times() {
    let said: Vec<_> = (0..=120).rev().filter_map(countdown_announcement).collect();
    assert_eq!(said, ["1 minute left", "30 seconds left", "10 seconds left"]);
}

#[test]
fn kde_turns_animations_off_with_a_zero_factor() {
    assert!(kde_reduced("[KDE]\nAnimationDurationFactor=0\n"));
    assert!(!kde_reduced("[KDE]\nAnimationDurationFactor=0.5\n"));
    assert!(!kde_reduced("[General]\nAnimationDurationFactor=0\n"));
    assert!(!kde_reduced(""));
}
```

In `src/app/ui.rs` (with `ctx.enable_accesskit()` in the test context):

```rust
#[test]
fn a_screen_reader_hears_each_stat_and_each_switch_by_name() {
    // FullOutput.platform_output.accesskit_update has nodes labelled "Round trip, 6 milliseconds",
    // "Share mouse, Debian" (toggled true) and "Share keyboard, Laptop" (toggled false).
}

#[test]
fn the_code_is_read_digit_by_digit() { /* a node labelled "4 8 2 9 1 7" while showing 482917 */ }
```

- [ ] **Step 2: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::motion app::ui`
Expected: FAIL.

- [ ] **Step 3: Implement**

- Turn on eframe's `accesskit` feature.
- Stat cells call `response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &value.spoken))`.
- Toggle cells use `WidgetInfo::selected(WidgetType::Checkbox, enabled, on, format!("{label}, {device}"))`.
- The code display is labelled with `spoken_code`.
- Announcements and pairing and row errors go out as polite live regions through `ctx.accesskit_node_builder(id, |n| { n.set_live(accesskit::Live::Polite); n.set_label(text) })`.
- The status square uses `cross_fade` from the colour it had when the state last changed, repainting until done.
- `dialog::show` gets `opacity = fade_in(...)` through `ui.set_opacity`.
- The countdown bar uses `countdown_share`, repainting every 100 ms while showing, or every 500 ms when reduced.
- With `reduced`, `style.animation_time = 0.0`.

- [ ] **Step 4: Run the tests and the Windows check**

Run: `env -u CARGO_TARGET_DIR cargo test --release`, then the Windows check, then `env -u CARGO_TARGET_DIR cargo build --release --target x86_64-pc-windows-gnu`.
Expected: PASS, and the Windows release build still links.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/app/motion.rs src/app/ui.rs src/app/top.rs src/app/table.rs src/app/pairing.rs src/app/dialog.rs src/app/mod.rs
git commit -m "Fades, a smooth countdown and screen reader names, with reduced motion respected"
```

---

### Task 11: The window in the taskbar

**Files:**
- Modify: `src/app/launch.rs` (`APP_ID`, the desktop entry, more icon sizes), `src/app/icon.rs` (`png_scaled`), `src/app/single.rs` (`is_held`), `src/app/ui.rs` (`with_app_id(APP_ID)`, `with_taskbar(true)`), `src/main.rs` (`run_app_window`), `packaging/build-deb.sh` (copies every icon size)

**Interfaces:**
- Produces:
  - `pub const APP_ID: &str = "mesh-kvm"` in `launch.rs`. The menu entry is written to `{APP_ID}.desktop` with `StartupWMClass={APP_ID}`, and `run_window` uses `.with_app_id(APP_ID)`.
  - `pub const ICON_SIZES: [u32; 5] = [32, 48, 64, 128, 256]`, and `pub fn png_scaled(size: u32) -> Vec<u8>` in `icon.rs` (nearest-neighbour from the 32px picture, so the pixel art stays sharp; 48 picks the nearest whole pixel per output pixel). `write_linux_launchers` writes `hicolor/<n>x<n>/apps/mesh-kvm.png` for each size.
  - `pub fn is_held(port: u16) -> bool` in `single.rs`.
  - In `main.rs`, `run_app_window` starts `--tray` when `!single::is_held(TRAY_PORT)`, after claiming the window port. On Windows, the `AlreadyRunning` branch calls `AllowSetForegroundWindow(ASFW_ANY)` before `signal_show`.

- [ ] **Step 1: Write the failing tests**

In `src/app/launch.rs`:

```rust
#[test]
fn the_menu_entry_matches_the_windows_app_id() {
    let home = temp("app-id");
    write_linux_launchers(&home, "/opt/mesh-kvm").unwrap();
    let entry = std::fs::read_to_string(home.join(format!(".local/share/applications/{APP_ID}.desktop"))).unwrap();
    assert!(entry.contains(&format!("\nStartupWMClass={APP_ID}\n")));
    assert!(entry.contains("\nIcon=mesh-kvm\n"));
    for size in ICON_SIZES {
        let png = std::fs::read(home.join(format!(".local/share/icons/hicolor/{size}x{size}/apps/mesh-kvm.png"))).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
    let _ = std::fs::remove_dir_all(home);
}
```

Update `linux_launchers_are_written_under_home` to expect `2 + ICON_SIZES.len()` files. In `src/app/icon.rs`: `a_scaled_icon_is_the_same_picture_bigger()` decodes nothing. It checks that `png_scaled(64)` declares width and height 64 in its IHDR, and that `png_scaled(32) == png()`. In `src/app/single.rs`: `a_held_port_is_seen_and_a_free_one_is_not()` binds `free_port()`, so `is_held` is true while the listener lives and false after it drops.

- [ ] **Step 2: Run them to see them fail**

Run: `env -u CARGO_TARGET_DIR cargo test --release app::launch app::icon app::single`
Expected: FAIL.

- [ ] **Step 3: Implement**

`is_held` is a `TcpStream::connect_timeout` to 127.0.0.1 with `IO_TIMEOUT`. The X button keeps ending the window process (ruling 1). `run_window`'s doc comment says so, and that the service and tray keep running. `build-deb.sh` copies every `hicolor/*/apps/mesh-kvm.png` the program wrote.

- [ ] **Step 4: Run the tests and the Windows check**

Run: `env -u CARGO_TARGET_DIR cargo test --release`, then the Windows check.
Expected: PASS.

- [ ] **Step 5: Check it by hand on KDE Wayland**

Run: `env -u CARGO_TARGET_DIR cargo build --release && target/release/mesh-kvm --install-launchers`, open mesh-kvm from the app menu, and look at the panel. Expected: the mesh-kvm icon, not a generic one. Close it with X. Expected: the tray icon stays and `systemctl --user is-active mesh-kvm` (or the running service) is unchanged. Choose Open in the tray. Expected: one window. Run `target/release/mesh-kvm --app` again. Expected: that same window comes forward, with no second one.

- [ ] **Step 6: Commit**

```bash
git add src/app/launch.rs src/app/icon.rs src/app/single.rs src/app/ui.rs src/main.rs packaging/build-deb.sh
git commit -m "Show the window in the taskbar with its icon, and keep the tray when it closes"
```

---

### Task 12: Fake service states, a look against the boards, and the old style removed

**Files:**
- Modify: `tests/fake_service.py` (`--secure` and `/api/pairing`), `src/app/theme.rs` (delete `legacy`), `src/app/widgets.rs`, and every file still importing `theme::legacy`

**Interfaces:**
- Produces: `tests/fake_service.py --secure {encrypted,connecting,unpaired,old-peer}` sets `secure` and `old_peer` in both devices' `/api/local`. `GET /api/pairing` answers from a small state machine. `pair_show` gives `showing`, code `482917`, counting down from 120. `pair_enter` with `482917` gives `paired`, fingerprint `4F2A-91C3`. Any other code gives `failed` with `The code was wrong. Show a new code and try again.` `pair_cancel` gives `idle`, and `unpair` gives `unpaired`. `POST /fake/secure {"state": …}` changes it while running. The top comment gains one sentence for each.

- [ ] **Step 1: Add the states** to `tests/fake_service.py` and run `python3 -I -m py_compile tests/fake_service.py`. Expected: no output.

- [ ] **Step 2: Look at each board's state against its board**

For each `(flags, board)` pair — `(--secure encrypted --link good, ChosenApp)`, `(--secure unpaired, then choose Pair and Show a code, ChosenPairShow)`, and `(--secure unpaired, then Pair, Enter a code, type 111111, ChosenPairEnter)` — run:

```bash
unshare -rn bash -c 'ip link set lo up && python3 -I tests/fake_service.py --monitors 2 <flags> & sleep 1; target/release/mesh-kvm --app'
```

and take a screenshot with `spectacle -b -n -a -o <scratchpad>/<board>.png`. Open the board's HTML beside it. Check:

- The 1280×760 frame.
- The status bar about 48px tall.
- The stats row with five columns and sparklines.
- The settings column exactly 700px, with 130px device columns and the group order as on the board.
- `ON` in lime and `OFF` dim.
- The layout label and hint at top left.
- The selected tile's light border and the 4px lime crossing edge.
- Dialogs 460px wide with their top at 190px.
- The code in lime at 54px, and the input with its lime border.
- The wrong-code line in `#ff8a7a`.

Then resize to 1000, 880 and 760 wide, and to 580 tall. Expected: the breakpoints from Task 2, nothing cut off. Also run `--update both`, `--update installing` and `--update apt-failed`. Expected: the update line between the status bar and the stats, with a danger square on the failure. Fix anything that differs from the board in the task that owns it, and note each fix in this commit's message.

- [ ] **Step 3: Remove the old style**

Delete `theme::legacy` and whatever still uses it, the old `MAT`/`BEZEL`-era helpers in `widgets.rs`, and `theme::mix` callers that only served them. Run `grep -n 'from_rgb' src/app/*.rs`. Expected: only `src/app/theme.rs` and `src/app/icon_pixels.rs` (the icon's own colours, shared with `build.rs`). Run `grep -rn 'legacy' src/app`. Expected: nothing.

- [ ] **Step 4: Run the tests and the Windows check**

Run: `env -u CARGO_TARGET_DIR cargo test --release`, then the Windows check.
Expected: PASS, no warnings (no dead code left behind).

- [ ] **Step 5: Commit**

```bash
git add tests/fake_service.py src/app/
git commit -m "Pairing states in the fake service, fixes from a look against the boards, and the old style gone"
```

---

### Task 13: Version 0.8.1 and the README

**Files:**
- Modify: `Cargo.toml` (`version = "0.8.1"`), `Cargo.lock` (by `cargo build`), `README.md`

- [ ] **Step 1: Bump the version** in `Cargo.toml` and run `env -u CARGO_TARGET_DIR cargo build --release`, so `Cargo.lock` follows. Run `grep -rn '0\.8\.0' src/ | grep -v test`. Expected: nothing that should now say 0.8.1 (test fixtures that name 0.8.0 as an older version stay).

- [ ] **Step 2: Update the README**

- **The app and the tray:** the new window. The status bar, with Switch and Release all. The update line under it. The five live figures with their minute of history. Every setting for both devices side by side in one table, grouped INPUT to UPDATES, with this device first. The layout beside it, or under it in a narrow window. App size is now under BOTH DEVICES. Closing the window keeps sharing and the tray. Opening mesh-kvm again brings back the same window.
- **Pairing:** the steps say Pair under BOTH DEVICES, then Show a code / Enter a code (no `here`). The fingerprint is in the table. Unpair asks first.
- **Toggles:** they are ON/OFF in the table, and a refused one goes back and says why under its row.
- **Bad Wi-Fi:** the last paragraph points at the ROUND TRIP and LOST figures at the top (same 150 ms and 5% thresholds), and says a figure shows `--` until it's known.
- **Arranging screens:** `the amber line` becomes `the bright edge`. Arrow keys move a selected screen by 10 pixels, Shift by 1.
- **Resolution and refresh rate:** the menus are in the SCREENS rows.
- The Opus licence line also names the bundled Inter and JetBrains Mono fonts under the SIL Open Font License.
- **Tests:** `tests/fake_service.py --secure` shows the pairing states.

Keep the README's style: plain sentences, no exclamation marks.

- [ ] **Step 3: Run the tests and the Windows check**

Run: `env -u CARGO_TARGET_DIR cargo test --release`, then the Windows check.
Expected: PASS.

- [ ] **Step 4: Commit (no tag, no release)**

```bash
git add Cargo.toml Cargo.lock README.md
git commit -m "Version 0.8.1: the Readout window"
```
