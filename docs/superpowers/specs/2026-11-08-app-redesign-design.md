# Handoff Spec: mesh-kvm app redesign ("Readout")

Design canvas: https://claude.ai/artifact/27pLLtNjAcohWVFfjKCCJX — boards **ChosenApp**, **ChosenPairShow**, **ChosenPairEnter** (copies in `2026-11-08-app-redesign-boards/`). The direction chosen was "B1 with B2's graphs": a dark Readout layout, with a lime accent and a sparkline under each figure.

## Overview

The app is the one window for both devices. It replaces the current card layout, which the user dislikes. Information comes first: the live link figures across the top, every setting for both devices side by side on the left, and the screen layout on the right. Everything is flat and square, with monospace for figures and labels, and no rounded cards, shadows or gradients.

Built in egui (`src/app/ui.rs`, `panels.rs`, `canvas.rs`, `pairing.rs`, `updates.rs`). This is a visual and layout rebuild. The service API (`/api/local`, `/api/state`, `/api/pairing`, the actions) doesn't change.

## Layout

Fixed reference frame: **1280 × 760** (ChosenApp is drawn 1200 tall to show every setting).

```
┌──────────────────────────────────────────────────────────────────────┐
│ ■ Control Debian                      [Switch to Laptop] [Release all]│  status bar   (~48px)
├──────────┬──────────┬──────────┬──────────┬──────────────────────────┤
│ROUND TRIP│ LOST     │AUDIO LOST│AUDIO BUF │ LINK                     │  stats row    (~96px)
│ 6 ms     │ 0.4%     │ 0%       │ 42 ms    │ Secure                   │
│ ~~~~~~   │ ~~~~~~   │ ~~~~~~   │ ~~~~~~   │ ~~~~~~                   │
├──────────────────────────────────┬───────────────────────────────────┤
│ SETTING           DEBIAN  LAPTOP │ LAYOUT                            │
│ INPUT                            │ Drag a screen to move it…         │
│ Share mouse        ON     ON     │   ┌────────┐┃┌─────┐               │
│ …                                │   │ Debian │┃│Laptop│              │
│ (settings table, 700px)          │   └────────┘┃└─────┘  (flex)       │
└──────────────────────────────────┴───────────────────────────────────┘
```

| Region | Size | Notes |
|---|---|---|
| Status bar | full width, padding `space-3 space-5` (12/20) | Status square, sentence, actions on the right |
| Stats row | 5 equal columns, padding `10 20 12` | Column dividers 1px `line` |
| Settings column | **700px** fixed, padding `12 20`, bg `surface` | Scrolls on its own when taller than the window |
| Layout column | flex, min 0 | Screen canvas, positions absolute inside |
| Dialogs | 460px wide, centred horizontally, top at ~25% (190px at 760) | Over the dimmed app |

**Why settings left, layout right:** the user asked for every setting to be visible and comparable across both devices at once. The canvas is used less often and can take what's left.

### Smaller windows
egui has no breakpoints. Use these rules on the window's width:

| Width | Changes |
|---|---|
| ≥ 1100px | As drawn |
| 900–1099px | Settings column shrinks to 55% of the width. Device columns stay 130px each, and the label column takes the rest and wraps |
| < 900px | Layout canvas moves below the settings (stacked, canvas 320px tall). Stats row goes to 3 + 2 columns |
| Height < 600px | Status bar and stats stay pinned; only the body scrolls |

## Design tokens

Use these as named constants in one place (such as `src/app/theme.rs`), never inline hex.

### Colour
| Token | Value | Usage |
|---|---|---|
| `bg` | `#0f1113` | Window background, inputs, the code box |
| `surface` | `#171a1d` | Status bar, stats row, settings column |
| `raised` | `#1d2125` | Dialogs, screen tiles on the canvas |
| `line` | `#2a2f34` | Dividers between regions and stat columns |
| `line-faint` | `#23272b` | Row dividers in the settings table |
| `line-strong` | `#6b747c` | Table header rule, dialog border, secondary button border, unselected screen tile |
| `text` | `#e6e8ea` | Body text, values, selected screen tile border |
| `text-dim` | `#9aa3ab` | Labels, captions, group headings, `--` placeholders, OFF |
| `accent` | `#c6f135` | ON, MAIN, primary button, sparklines, the crossing edge, the pairing code, focus ring, links |
| `on-accent` | `#0f1113` | Text on an `accent` fill |
| `warn` | `#ffb86b` | Not paired, link degraded, waiting |
| `danger` | `#ff8a7a` | Release all, errors, a wrong code |

Contrast (WCAG): `text` on `bg` 15.4:1, `text-dim` on `surface` 6.8:1, `accent` on `bg` 14.4:1, `on-accent` on `accent` 14.4:1, `warn` on `raised` 9.5:1, `danger` on `raised` 7.1:1. All pass AA for text. `line-strong` on `bg` is 4.0:1, which is enough for borders (3:1) but not for text.

### Type
| Token | Family / size / weight | Usage |
|---|---|---|
| `type-body` | Inter 14 / 400, line 1.4 | Labels, sentences |
| `type-status` | Inter 15 / 400 (device name 600) | Status bar sentence |
| `type-caption` | Inter 13 / 400, `text-dim` | Hints under controls |
| `type-label` | JetBrains Mono 11 / 400, letter-spacing 1px, UPPERCASE, `text-dim` | Stat labels, group headings, dialog titles |
| `type-head` | JetBrains Mono 12 / 500 | Settings table header (SETTING / device names) |
| `type-value` | JetBrains Mono 13 / 400 | Setting values |
| `type-stat` | JetBrains Mono 26 / 500 | Stat figures |
| `type-code-show` | JetBrains Mono 54 / 500, letter-spacing 4px | Code on the showing device |
| `type-code-enter` | JetBrains Mono 44 / 500, letter-spacing 4px | Code input |

Bundle both fonts into the binary (`egui::FontDefinitions`). The Windows build has no network, and this project doesn't fetch at run time.

### Spacing
`space-1` 4 · `space-2` 8 · `space-3` 12 · `space-4` 16 · `space-5` 20 · `space-6` 24. Settings rows are at least 26px tall. Buttons are at least 36px tall in the status bar and 40px in dialogs, and at least 96px wide in dialogs.

### Shape
Radius 0 everywhere. Borders 1px. The status square is 10×10. The crossing edge is 4px wide. The countdown bar is 4px tall.

## Components

| Component | Variant | Content / props | Notes |
|---|---|---|---|
| Status square | `accent` / `warn` / `danger` / `text-dim` | — | Linked and in control / not paired or degraded / link down / connecting |
| Status sentence | — | `Control <device>`, `Not paired. Everything stays on each device.` | `text-dim` lead word, device name in 600 |
| Primary button | filled `accent`, `on-accent` 600 | `Switch to <other>`, `Pair` | One per view at most |
| Danger button | outline `danger` | `Release all` | Always visible while linked |
| Secondary button | outline `line-strong` | `Cancel`, `Check now` | |
| Stat cell | — | label, figure, sparkline | Sparkline 210×36, 1.5px `accent` stroke, no fill, last ~60 s |
| Settings group | — | heading in `type-label`, rows | Groups: INPUT, SCREENS, CLIPBOARD, AUDIO, GAMES, STARTUP, BOTH DEVICES, then UPDATES |
| Settings row | toggle / dropdown / number / text / action | label + one cell per device (130px each) | A blank cell means the setting doesn't apply to that device, so show nothing, not `--` |
| Toggle cell | ON / OFF | `ON` in `accent`, `OFF` in `text-dim` | Click flips it. Text, not a switch graphic |
| Dropdown cell | — | value + ` ▾` | Opens an egui ComboBox under the cell |
| Number cell | — | `0.35×`, `80%`, `55` | Drag or type, as egui `DragValue` |
| Screen tile | selected / other | name (600), output and resolution in mono 12 `text-dim` | Selected tile has a `text` border, the other `line-strong` |
| Crossing edge | — | 4px `accent` bar | On the shared edge where the mouse crosses |
| Dialog | — | title in `type-label`, body, buttons right-aligned, gap 12 | bg `raised`, border `line-strong`, padding `18 20 20` |
| Code display | — | `482 917` | bg `bg`, padding 18px vertical, centred, `accent` |
| Code input | — | 6 digits shown as `### ###` | bg `bg`, 1px `accent` border, centred, numeric only, max 7 chars with the space |
| Countdown bar | — | fill = time left / 120 s | Track `bg`, fill `accent` |
| Update line | ready / step / failed | from `update_banner()` | **Not on the boards.** It goes between the status bar and the stats row, full width, `surface`, with the text and the release notes link on the left and the button right. Failed uses a `danger` square |

## Screens

### Main (ChosenApp)
- **Status bar:** the status square, `Control <device>` and the two actions. While unlinked, hide **Switch** and keep **Release all**.
- **Stats:**

  | Label | Value | Stands out |
  |---|---|---|
  | ROUND TRIP | ms | ≥ 150 ms, in `warn` |
  | LOST | worst direction, % | ≥ 5%, in `warn` |
  | AUDIO LOST | % | — |
  | AUDIO BUFFER | ms | — |
  | LINK | `Secure` / `Connecting` / `Not paired` / `Link down` | — |

  These thresholds are today's rules, from the README's Bad Wi-Fi section. A figure shows as `--` in `text-dim` until it's known, and the sparkline is hidden then.
- **Settings table:** every setting the current panels have, laid out as on the board. *BOTH DEVICES* holds main device, addresses, where the other device sits, push needed to cross, and `Paired, fingerprint 4F2A-91C3` with `Unpair` in the other column.
- **Layout canvas:** the current canvas behaviour (drag to move, snapping, the crossing edge), restyled. The hint `Drag a screen to move it. The bright edge is where the mouse crosses.` sits at top left under the `LAYOUT` label.

### Showing a code (ChosenPairShow)
- Title `PAIR WITH <OTHER>`. Body `Open mesh-kvm on <Other>, choose Enter a code, and type:`
- Code, then the countdown bar, then a mono 12 `text-dim` row: `M:SS LEFT, ONE TRY` on the left and `WAITING FOR <OTHER>` on the right.
- Only **Cancel**. The app behind it shows the unpaired state: `warn` square, every stat `--`, LINK `Not paired`.

### Entering a code (ChosenPairEnter)
- Title `PAIR WITH <OTHER>`. Label `Type the code <Other> is showing.` (a real `<label>` for the input). Hint `Use this device's own keyboard. The devices can't share one until they're paired.`
- **Cancel** and **Pair**. Pair is disabled until 6 digits are typed.
- The board shows the wrong-code error, in `danger`: `A wrong code can't be used again. Show a new one on <Other> and try once more.`

## States and interactions

| Element | State | Behaviour |
|---|---|---|
| Any button | Hover | Filled: background +8% lightness. Outline: border goes `text`, or stays `danger` for Release all with a 10% `danger` fill |
| Any button | Pressed | Filled −8% lightness. Outline 15% tint |
| Any button | Disabled | 40% opacity, no hover |
| Any focusable | Focus (keyboard) | 2px `accent` outline, 2px offset. Never removed |
| Toggle cell | Hover | `raised` row background. Pointer cursor |
| Toggle cell | Saving | Value stays and dims to 60% until the service answers. On refusal it snaps back and the row shows the error under it in `danger` |
| Settings row | Read-only (other device can't change it) | `text-dim` value, no hover, tooltip says why, for example "Only Laptop can choose its microphone." |
| Switch | Pressed | Sends the switch action, and the status sentence changes when the service says so (no optimistic change) |
| Pair | Working | Label `Pairing…`, disabled, Cancel still works. Pairing runs off the UI thread as today |
| Pair | Success | Dialog shows `Paired with <Other>` and `Everything between these devices is encrypted.` for 2 s (`PAIRED_SHOWN`), then closes |
| Pair | Wrong code / ran out / other device hung up | `danger` line in the dialog with the service's text, input cleared and refocused |
| Code input | Typing | Digits only. The space after 3 digits is inserted automatically and is ignored on paste. Enter = Pair, Esc = Cancel |
| Screen tile | Drag | Follows the pointer, snaps on release as today, crossing edge updates live |
| Unpair | Click | Confirmation dialog ("Unpair <Other>? The two devices stop sharing until you pair again.") with Cancel and danger **Unpair** |

## Content

- Device names: up to 24 chars in the table header and status bar, then `…`, with the full name in a tooltip.
- Setting labels wrap, they're never cut off. Device columns stay 130px, and values that don't fit get `…` and a tooltip.
- Addresses are IPv4 in mono. A missing address shows `Not set`, in `warn`.
- Errors use the service's own sentence, which is already written for people. Don't wrap it in "Error:".
- All copy follows the project's style: plain sentences, no exclamation marks, and say what the person can do.

## Edge cases

- **No service:** keep today's behaviour (the app starts it, or says why it can't and names the log file), shown in the body area, with the status square in `danger`.
- **Not paired:** stats all `--`, LINK `Not paired` in `warn`, and the settings table still shows this device's column, with the other device's column empty and a Pair button in BOTH DEVICES.
- **Old peer** (`old_peer` true): LINK `Needs update`, in `warn`, with the existing note.
- **Link down while paired:** `danger` square, LINK `Link down`, sparklines freeze with their last point hollow.
- **Slow service answers:** a value that is saving shows dimmed. Nothing blocks the UI thread.
- **Three or more monitors:** the canvas scales to fit, with tiles at least 80px wide. Below that, scroll the canvas.
- **Update under way:** the update line shows the step, and the body stays usable.

## Motion

Kept minimal, as this is a tool and egui repaints on demand.

| Element | Trigger | Animation | Duration | Easing |
|---|---|---|---|---|
| Countdown bar | each second | width shrinks | 1000 ms | linear |
| Dialog | open/close | fade, 0 → 1 | 120 ms | ease-out |
| Status square | link state change | colour cross-fade | 200 ms | ease-in-out |
| Sparklines | new sample | none, redraw only | — | — |

Respect reduced motion: with it on, drop the fades and move the countdown in steps.

## Accessibility

- **Focus order:** status bar actions → each settings row left to right (this device, then the other) top to bottom → layout canvas tiles → update line button. Dialogs trap focus: input → Cancel → Pair. Opening a dialog focuses the input, or Cancel when there is none.
- **Keyboard:** Space toggles, Enter opens a dropdown, arrow keys move a selected screen tile by 10px (Shift for 1px), and Esc closes a dialog.
- **Screen readers** (egui AccessKit): each stat is read as "Round trip, 6 milliseconds". Toggles are switches named "<setting>, <device>". The code display is read digit by digit. The countdown is announced at 1:00, 0:30 and 0:10, and errors are announced politely.
- **Colour is never the only signal:** states always have words (ON/OFF, `Not paired`, `Link down`) next to their colour.
- **Targets:** at least 24×24 in the table (rows are 26px tall, and the whole cell is clickable), and 36–40px for buttons.

## Open questions for the build

1. Where the update line goes, and its states. It's drawn nowhere yet: the boards predate the updater.
2. Whether the canvas keeps its device colours or goes fully monochrome as on the board.
3. Font licensing: Inter and JetBrains Mono are both OFL, so bundling them is fine, but confirm the binary size cost (~600 KB).

## Decisions for the build (2026-11-08)

These settle the open questions above, and add how the window behaves.

1. **Window and taskbar.** While the window is open, mesh-kvm shows in the taskbar like any app, with its own icon and name: on Windows a normal top-level window, and on KDE Wayland a window whose app id matches `mesh-kvm.desktop`, so the panel shows the right icon. Closing the window (X) only hides it. Sharing keeps running and the tray icon stays. **Quit mesh-kvm** in the tray stops everything on that device, as in 0.7.2. Opening mesh-kvm again from the Start menu, the app menu or the tray brings back the same window, never a second one. Start at sign-in starts sharing and the tray, with no window.
2. **Update line.** It sits between the status bar and the stats row, full width, only while there is something to say. It shows `Version 0.8.1 is ready` with the button, the steps while updating, or the note after a failure. When there's nothing to say, it takes no space.
3. **Device colours on the canvas.** Each device keeps a muted tint on its screens, so you can tell which screen belongs to which device. Everything else stays monochrome as drawn.
4. **Fonts.** Inter and JetBrains Mono are bundled (both OFL; licences go in `src/licenses`). The ~600 KB is accepted.
5. **What stays the same:** the service API, the tray menu items apart from the restyle, pairing, and all behaviour behind the settings. This is a rebuild of the window only.
