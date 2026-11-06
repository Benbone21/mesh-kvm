# mesh-kvm

mesh-kvm shares one mouse and keyboard between a Debian desktop and a Windows laptop. Move the pointer off the edge of one screen and it appears on the other. It also shares the clipboard and audio. The same program runs on both machines, one build per operating system.

Both devices run the same version. Update them together: a Debian build and a laptop build from different versions may not understand each other's packets.

## Build

You need Rust, plus the Windows target and a MinGW cross-compiler for the laptop build.

```
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu
```

The outputs are `target/release/mesh-kvm` for Debian and `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe` for Windows.

## Run

On Debian, run `mesh-kvm` with `--peer` set to the laptop's IP address. On Windows, run `mesh-kvm.exe --role windows --peer <debian-ip>`. The Windows build copies itself to `%LOCALAPPDATA%\mesh-kvm` and sets itself up to start at login.

## Update the laptop

Update the laptop first. A new laptop with an old Debian stays local and safe. A new Debian with an old laptop forwards input into nothing until you press Ctrl+Alt+F11.

1. Quit the running `mesh-kvm.exe` (Task Manager, Details tab). Windows won't let the installed copy be replaced while it runs.
2. Copy the new `mesh-kvm.exe` to the laptop and run it once with the same `--peer` as before. The new exe keeps running from the folder you downloaded it to. The installed copy takes effect at the next login.
3. Update and restart Debian.

## Hotkeys

These work on both devices.

| Keys | What it does |
| --- | --- |
| Ctrl+Alt+F11 | Switch input to the other device |
| Ctrl+Alt+F12 or Ctrl+Alt+ScrollLock | Release: hand input back to the device you are sitting at |
| Ctrl+Alt+G | Game lock: keep the pointer on the current device |

Ctrl+Alt+F11 and Ctrl+Alt+G need the link to the laptop to be up. Ctrl+Alt+F12 and Ctrl+Alt+ScrollLock always work.

If input seems stuck on the wrong device, press Ctrl+Alt+F12.

Ctrl+Alt+F11 still switches focus when mouse or keyboard sharing is off, so it is the way out if turning sharing off leaves the other device with focus.

## Studio

The Studio shows both devices and lets you arrange their screens. Open http://localhost:24880 in a browser, or run `mesh-kvm --studio`.

## Limits

- The Windows lock screen and UAC prompts can't be controlled remotely. Use the laptop's own keyboard and trackpad for those.
- Port 24880 (the Studio) and the input traffic are not encrypted until the next stage. Only use mesh-kvm on a network you trust.

## Tests

`cargo test` runs the unit tests. `tests/wine_link.py` runs the Windows build under Wine and drives it with hand-made packets to check the link, the active-device logic and the cursor jump:

```
unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_link.py [path/to/mesh-kvm.exe]'
```

The exe path defaults to `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe` in the repository and can also be set with `MESH_KVM_EXE`. Run it from the repository root; it needs `wine`.
