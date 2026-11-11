# mesh-kvm 0.9.0: connection drops, dips and a whole-codebase inspection (2026-11-10)

Source: the link logs from both devices (2026-11-08 to 11-10) and four read-only inspections (input and link; audio; security, updates and clipboard; app, settings and layout). Nothing in the code was changed.

## What the drops and dips are

| Symptom | Cause | Status |
|---|---|---|
| Link down 0.5–6 s, about every 5–50 minutes | Debian's Wi-Fi jumping between the router's 5 GHz and 6 GHz radios (full reconnect each time) | Fixed outside mesh-kvm: Debian is pinned to the 5 GHz radio (E2:99:04:4D:A8:3F) |
| ~300–380 ms freeze on every switch back to Debian | mesh-kvm releases its 5 input devices one at a time on the input loop; the kernel takes ~60 ms each (`src/input/linux.rs:618`, via `update_grabs` 998–1021). Keys typed then are lost | Bug, confirmed |
| "Stalls" of 350–402 ms seen from Debian | Mostly a logging rule: the pause after the mouse stops, until the next 400 ms heartbeat, is logged as a stall (`src/router.rs:411-416`). A 351–354 ms cluster is unexplained | Logging bug; one open question |
| 3.4–4.5 s before the app and layout sync work again after a drop | The encrypted control connection isn't reconnected when the link returns; one lost packet fails the 350 ms attempt (`src/secure/control.rs:251-297`) | Bug, confirmed |

## Findings, most important first

### A. Things you feel
1. **Switch freeze and lost keys** — `src/input/linux.rs:618`. Release devices on a worker thread, all at once.
2. **Slow recovery after a drop** — `src/secure/control.rs:251-297`, `src/input/link.rs:694-704`. Reconnect the control connection in the background when the link returns; send a heartbeat at once on `up` (saves up to ~0.8 s); fail app requests fast while the link is down.
3. **Debian's laptop mic and speakers vanish on every drop** — `src/audio/virtual_linux.rs:72-74`. Apps fall back to the real mic and speakers and often stay there. Keep the devices for 20–60 s after the link drops.
4. **Audio can run 200–400 ms late after a hiccup** — `src/audio/jitter.rs:254-263, 239-249, 231-235`. Seen in the log: 200–390 ms for 40 s. Cap the delay near 100 ms, restart at the newest packets, let trimming fire.
5. **Virtual mic can click** — `src/audio/linux.rs:890-994`. The feed runs on the wall clock, not the sound server's. Pace it by how full the pipe is.
6. **Replugged keyboard or mouse isn't picked up** — `src/input/linux.rs:1171`. Until a restart it drives both devices at once. Rescan on unplug/replug.
7. **Input loop can wait on a disk write** — `src/input/linux.rs:1510`, `src/spatial.rs:802`. Use `try_write`, save outside the lock.
8. **Play only on the other device: sound jumps to local speakers during every drop** — `src/mute.rs:558`. Wait ~2 s before unmuting.

### B. Things that can break or mislead
9. **A malformed clipboard image crashes the service** — `src/clipboard.rs:788-791`. Validate size before use.
10. **After a drop the window blames the firewall and can offer to update only this device** — `src/app/client.rs:430`, `src/app/model.rs:1432`, `src/app/updates.rs:168`. Hold the last peer status ~3 s; disable that button meanwhile.
11. **One bad value resets all settings and overwrites the file** — `src/settings.rs:215`. Per-field defaults; keep a `.bad` copy.
12. **The app says a change failed when it was made** — `src/webui.rs:225-247`, `src/app/client.rs:420`. Line up the timeouts.
13. **Images and files copied during a drop are lost** — `src/clipboard.rs:1057-1092`. Retry the newest one, as text is.
14. **Choosing the main device can leave the two disagreeing** — `src/app/client.rs:354-381`.
15. **A poisoned layout lock looks like "mesh-kvm isn't running"** — `src/spatial.rs:976`.
16. **Audio loss is double-counted for late packets** — `src/audio/jitter.rs:168-171, 433`.
17. **Windows capture restarts on every receive on/off** — `src/audio/wasapi.rs:1098-1100`.

### C. Cost and housekeeping
18. **Clipboard fully read 3 times a second, images decoded each time** — `src/clipboard.rs:30, 269-281`.
19. **Audio player never closes and wakes ~950 times a second** — `src/audio/jitter.rs:375-394`, `src/audio/linux.rs:820-866`. The service's largest CPU user; may also keep the laptop from sleeping.
20. **Control threads linger up to 2 hours after the laptop sleeps** — `src/secure/control.rs:93`. Turn on keepalive.
21. **pw-play top-up drops audio on a full pipe** — `src/audio/linux.rs:853-862`.
22. **A stray LAN connection uses up a pairing code** — `src/secure/pairing.rs:195-200`.

### D. Security notes (suspected, lower risk)
- A device on your LAN that fakes the laptop's address could replay an old handshake to keep the link down (`src/secure/datagram.rs:557-593`). It can't read or inject anything.
- The Debian udev rule lets your own user's programs read all keyboards (`99-mesh-kvm-uinput.rules`).
- Checked and sound: update signatures and the root install helper, the local API (a web page can't read or change anything), replay protection, pairing code guessing.

### E. Logging that would make the next diagnosis faster
- Don't log the normal post-movement pause as a stall (`src/router.rs:411-416`).
- Log send errors (`src/input/mod.rs:79, 126`).
- Per-sample audio counters: concealed, repaired, late, trimmed, restarts, peak buffer (`src/linklog.rs:701-704`).
- Show the last drop and its reason in the app window (roam, Wi-Fi lost, sleep), from data the link log already has.

## Suggested order
1. Items 1, 2, 3, 9 and the logging fixes (the dips, recovery, the crash).
2. Items 4, 5, 6, 7, 8, 10, 11 (audio quality and wrong messages).
3. The rest.
