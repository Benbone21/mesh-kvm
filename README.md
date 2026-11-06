# mesh-kvm

mesh-kvm shares one mouse and keyboard between a Debian desktop and a Windows laptop. Move the pointer off the edge of one screen and it appears on the other. It also shares the clipboard and audio. The same program runs on both machines, one build per operating system.

The two devices are equal. Each one is both client and host, each keeps its own copy of the layout, and you can arrange screens and switch devices from either one. A Mac version is planned, but there is no Mac build yet.

Both devices run the same version. Update them together: a Debian build and a laptop build from different versions may not understand each other's packets.

## Build

You need Rust, plus the Windows target and a MinGW cross-compiler for the laptop build.

```
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu
```

The outputs are `target/release/mesh-kvm` for Debian and `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe` for Windows.

## Install

On Windows, double-click `mesh-kvm.exe`. It copies itself to `%LOCALAPPDATA%\mesh-kvm`, adds a Start menu entry, and sets itself up to start at login.

On Debian, install the `.deb`, or run `mesh-kvm --install-launchers` once. That adds mesh-kvm to your applications menu and starts the tray icon when you log in.

The first time the app opens on a device, it asks for the other device's address. Enter it on both devices. The app shows each device's own address in the box and next to the device's name. The address is saved, and you can change it later under Settings in the window. Use an IPv4 address such as `192.168.1.20`.

## Starting and stopping

Opening the app or the tray starts the background service if it isn't running, so there is nothing else to start. On Linux it uses the `mesh-kvm` systemd user service when there is one, and otherwise starts the service directly. If the service can't start, the app says so and names the log file.

`--peer` and `--switch-edge` are optional. `--peer` sets the address for that run and saves it. While it is in use, the app can't change the address, because the flag would win again at the next start. `--switch-edge` says which side of this device the other one sits on, and only matters for a device with no layout yet.

## The app and the tray

Opening mesh-kvm from the Start menu or the applications menu shows the app window. It works the same from either device. You can also run `mesh-kvm --app`.

Most of the window is your desk: every monitor of both devices, to scale. The screens of the device that has the keyboard and mouse are lit. Beside it are the controls for each device, under Screens, Input, Clipboard, Audio and Startup. They sit side by side in a wide window and one above the other in a narrow one, and they scroll if they don't all fit.

Everything sizes from the window, and a big window enlarges the text and controls a little. To change that, use App size under Settings, or Ctrl with plus, minus or 0. A larger app size raises the smallest size the window can go to, so nothing runs out of room.

The window remembers its size and app size. It remembers its position on Windows and on X11. On Wayland the desktop chooses where windows open, so the position isn't kept. These are saved in `~/.local/share/mesh-kvm` on Debian and `%APPDATA%\mesh-kvm\data` on Windows.

Devices go by their own names. Click a device's name in the window and type to rename it, from either device. Leave it empty to go back to its network name.

The tray icon sits in the notification area. Its menu has Open, Switch device, Release all input, and Quit.

Closing the window does not stop sharing. The service keeps running and the tray stays. Choosing Quit in the tray removes only the tray icon; sharing carries on. To stop sharing completely, end the `mesh-kvm` process on that device.

## Toggles

Each device has its own switches in the app window.

- Share mouse and Share keyboard turn sharing of each one on or off.
- Send clipboard and Receive clipboard choose which way the clipboard travels: text, images and files. Copied files move as soon as you copy them, up to 2 GB per copy, and the last two copies stay in a `received` folder beside the settings.
- Send audio and Play audio choose whether this device sends its sound to the other, and whether it plays the other's.
- Mute this device while sending silences this device's speakers while it sends its system sound, so you hear it on only one device. It is on by default. The speakers come back when sending stops, the link drops, or sound from the other device is playing here. Sending the microphone doesn't mute. If you had muted the speakers yourself, they stay muted, and if you unmute them by hand, mesh-kvm leaves them alone until sending next starts. If the output changes, say to a headset, the old one comes back and the new one is muted. If mesh-kvm stops unexpectedly, the tray unmutes after 5 seconds.
- Start mesh-kvm when this device boots controls starting at login or boot. With it off, the tray still appears at login on Debian, but the service waits until you open the app.

Changes take effect straight away and are kept when mesh-kvm restarts.

## Arranging screens

Every monitor on both devices appears in the app, including ones to the left of or above a device's main screen, with its name, resolution and refresh rate.

Drag any screen to arrange the devices. The whole device moves together: mesh-kvm arranges how the two devices meet, and each device's own monitors stay where its operating system has them. To change those, use that device's display settings. A dragged device snaps to the edge of the other device's nearest screen, never lands on top of one, and lines up with a screen's start, middle or end when it comes close. The amber line shows where the mouse crosses.

An arrangement you change on either device is saved there and passed to the other one. If the other device is away, your change is kept and reaches it when it comes back. Changes to different things on the two devices are both kept, such as moving a device on one and changing the push needed to cross on the other. If both devices changed the same thing, the newer change wins, and on a tie the second device's does.

The layout is saved as `layout.json` beside the settings: `~/.config/mesh-kvm` on Debian and `%LOCALAPPDATA%\mesh-kvm` on Windows. Pointer speed and the game lock settings belong to each device and aren't shared.

## Resolution and refresh rate

Each screen has a resolution and a refresh rate menu, which list what that monitor offers. Click a screen on the map to find its menus. Changing one applies on the device the monitor belongs to, from either device's app.

The app then asks whether to keep the change. If nobody answers within 15 seconds, the screen goes back by itself. The service does that, so it still happens if the app closes or the screen goes blank. If the service itself is stopped during those 15 seconds, it goes back the next time the service starts. Only one change can be waiting at a time.

Debian uses `kscreen-doctor`, from the `libkscreen-bin` package, and Windows uses its own display settings. A monitor mesh-kvm can only list says so in place of its menus. That happens on Debian if `kscreen-doctor` can't reach the desktop.

## Updating

This version changes what the devices send each other, so update both before you use them together. Until then, crossing between them won't work properly. If one device still runs an older mesh-kvm, the app says which one and turns off the controls it can't use.

Your settings carry over. The first start finds the address in the `--peer` your launchers already use and saves it. On Debian, it imports `layout.json` along with its pointer speed, and leaves the old file where it is. It looks in the folder the service runs in, then `~/mesh-kvm`, then the folders above the program.

1. On the laptop, double-click the new `mesh-kvm.exe`. This is the normal way to install. If mesh-kvm is already running, the new copy is installed over the old one (the old one is moved aside and deleted later), the launchers are rewritten so the tray starts at login, and the new version takes effect once every `mesh-kvm.exe` is ended in Task Manager (Details tab) and started again, or at the next login.
2. On Debian, replace the binary, restart mesh-kvm, then run `mesh-kvm --install-launchers` once to refresh the menu entry and the tray's autostart entry.
3. Delete old downloads. Running any `mesh-kvm.exe`, old ones included, replaces the installed copy, so an old download double-clicked later puts the old version back.

## Hotkeys

These work on both devices.

| Keys | What it does |
| --- | --- |
| Ctrl+Alt+F11 | Switch input to the other device |
| Ctrl+Alt+F12 or Ctrl+Alt+ScrollLock | Release: hand input back to the device you are sitting at |
| Ctrl+Alt+G | Game lock: keep the pointer on the current device |

Ctrl+Alt+F11 and Ctrl+Alt+G need the link between the devices to be up. Ctrl+Alt+F12 and Ctrl+Alt+ScrollLock always work.

If input seems stuck on the wrong device, press Ctrl+Alt+F12.

Ctrl+Alt+F11 still switches focus when mouse or keyboard sharing is off, so it is the way out if turning sharing off leaves the other device with focus.

## Limits

- The Windows lock screen and UAC prompts can't be controlled remotely. Use the laptop's own keyboard and trackpad for those.
- The API port (24880) stays open on the network, and it and the input traffic are not encrypted until the encryption stage. Only use mesh-kvm on a network you trust. Anyone on the network can read a device's status, but only that device and the other one can change anything, and only the app on the same device can change the other device's address.
- Addresses are IPv4 only, and there is no automatic discovery.
- There is no Mac build yet.

## Tests

`cargo test` runs the unit tests. `tests/fake_service.py` pretends to be the service for two devices, with 3 or 6 monitors, so the app window can be looked at and resized with nothing real behind it. Its top comment says how to run it. `tests/wine_link.py` runs the Windows build under Wine and plays the other device, sending hand-made packets and reading the ones that come back. It checks the link, the active-device logic, input and edges in both directions, layout sync, and saving and changing the address:

```
unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_link.py [path/to/mesh-kvm.exe]'
```

The exe path defaults to `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe` in the repository and can also be set with `MESH_KVM_EXE`. Run it from the repository root; it needs `wine`, `ip` and Python 3.12 or later. Set `WINE_LINK_DISPLAY` to a throwaway X display to also press a real key and move the real mouse on the laptop, which needs `xdotool`.
