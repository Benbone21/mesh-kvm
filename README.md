# mesh-kvm

mesh-kvm shares one mouse and keyboard between a Debian desktop and a Windows laptop. Move the pointer off the edge of one screen and it appears on the other. It also shares the clipboard and audio, and it's made to hold up on poor Wi-Fi. The same program runs on both machines, one build per operating system.

The two devices are equal. Each one is both client and host, each keeps its own copy of the layout, and you can arrange screens and switch devices from either one. A Mac version is planned, but there is no Mac build yet.

Both devices have to run the same version, so update them together. A device on a version from before encryption shares nothing with this one, and each works alone until both are updated and paired.

Everything between the two devices is encrypted. They pair once with a 6-digit code, and after that a device that merely has the right IP address gets nothing.

## Build

You need Rust and a C compiler, plus the Windows target and a MinGW cross-compiler for the laptop build.

```
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu
```

The outputs are `target/release/mesh-kvm` for Debian and `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe` for Windows.

## Install

On Windows, double-click `mesh-kvm.exe`. It copies itself to `%LOCALAPPDATA%\mesh-kvm`, adds a Start menu entry, and sets itself up to start at login.

On Debian, install the `.deb`, or run `mesh-kvm --install-launchers` once. That adds mesh-kvm to your applications menu and starts the tray icon when you log in.

The first time the app opens on a device, it asks for the other device's address. Enter it on both devices. The app shows each device's own address in the box and next to the device's name. The address is saved, and you can change it later under Settings in the window. Use an IPv4 address such as `192.168.1.20`.

## Pairing

The devices share nothing until they're paired. You do this once. It needs mesh-kvm open on both, and the other device's address set on the device that enters the code.

1. Open Settings in the app and choose Pair, then Show a code here. The code is good for 2 minutes and one try.
2. On the other device, choose Pair, then Enter a code here, and type the code. Use that device's own keyboard, as the devices can't share one until they're paired.
3. Both devices show the same fingerprint under Settings, such as `4F2A-91C3`, and the link comes up.

A wrong code can't be tried again: show a new one. TCP port 24853 is open only while a code is showing. If the device showing the code has the other device's address saved, it accepts only that address. With no address saved it accepts any address for that 2-minute, one-try window, so someone else on the network could take the try, with one guess in a million per code. If the wrong address is saved, the device entering the code says so, and the code stays good.

Only the device showing the code needs its firewall to let 24853 in, for that long. Both need the usual UDP 24850 and 24851 and TCP 24852.

Without a window, `mesh-kvm --pair-show` and `mesh-kvm --pair <code>` do the same through the running service.

Unpair is in the same Settings menu. It removes the other device here, and tells that device if it can be reached. If it can't, its handshakes are refused until you pair again. The keys are in `pairing.json`, in `~/.config/mesh-kvm` on Debian and `%LOCALAPPDATA%\mesh-kvm` on Windows, and unpairing keeps this device's own key. If that file can't be read, the device stays unpaired and the app says why.

### What is and isn't protected

Input, audio, the clipboard, files and calls to the other device's API are all encrypted and authenticated. That covers someone on the same network or Wi-Fi who can read, inject, replay, reorder or drop packets, or who takes the other device's IP address. The local API on port 24880 now only listens on the device itself.

It doesn't cover:

- The size and timing of packets, which still show when someone is typing.
- Jamming or flooding the link.
- A compromised device, or malware running as the same user. `pairing.json` holds the private key, and is readable by that user.
- Other programs on the same device reaching 127.0.0.1:24880, which has no token yet.

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
- Send clipboard and Receive clipboard choose which way the clipboard travels: text, images and files. Copied files move as soon as you copy them, up to 2 GB per copy, and the last two copies stay in a `received` folder beside the settings. If an app takes more than 2 seconds to hand over what it copied, a note says so, the copy goes when the app finishes, and a copy from anywhere else still works meanwhile.
- Start mesh-kvm when this device boots controls starting at login or boot. With it off, the tray still appears at login on Debian, but the service waits until you open the app.

Changes take effect straight away and are kept when mesh-kvm restarts.

## Audio

Each device has its own audio controls, and sound can go both ways at once.

- Microphone or line-in to send, and Output to send, choose what this device sends. Each lists your real devices by name, or None. Choosing anything starts sending, and if you choose both they are sent mixed, turned down just enough that the sum doesn't clip. A microphone can only be chosen on the device it belongs to, though the other device can turn it off.
- Play received audio on chooses where sound from the other device plays. Play audio turns it off.
- Play only on the other device mutes the output you chose to send, so you hear that sound on one device only. It is on by default. On Windows it also mutes the default output if that's a different one, and changes what's sent: see below.
- Audio quality chooses the bitrate of what this device sends: 64, 96, 128 or 256 kbit/s, or Uncompressed at about 1.6 Mbit/s. Each packet adds about 32 kbit/s on the network on top of that. On poor Wi-Fi, use a lower one: smaller packets get through more often.
- The lines under each part say how the audio is doing: how much is lost on the way, how much couldn't be repaired, and how much is held back to smooth out the network. A note appears if a capture can't be opened, delivers nothing, or a microphone gives only silence.

### Play only on the other device

On Windows, muting an output also silences an ordinary recording of it, and it follows the volume slider too. A test on the laptop showed that exactly. So with this on, mesh-kvm asks Windows for all sound except its own, which comes from before the mute and the volume slider. That has three effects: every app's sound on every output is sent, a small line under Output to send says so, and the level sent no longer changes with the laptop's volume. It also means the output chosen no longer decides what's sent, so the Windows default output is muted as well. If Windows won't do this, nothing is muted, sound plays on both devices, and a note says why. Debian doesn't need any of this.

The output is muted only once the other device confirms that real sound is coming out of its output. A pause in the sound keeps it muted for up to 30 seconds, so it doesn't go on and off between tracks. It comes back when you stop sending, the link drops, the other device stops playing, or sound from the other device is playing on that same output. If sound stops right after muting, the output comes back within seconds. On Debian, if that happens twice in half a minute, it stays on for a while and a note says so. Sending a microphone doesn't mute anything. If you had muted the output yourself, it stays muted, and if you unmute it by hand, mesh-kvm leaves it alone until sending next starts. If the output changes, say to a headset, the old one comes back and the new one is muted. If mesh-kvm stops unexpectedly, the tray unmutes after 5 seconds.

### Devices coming and going

On Windows without Play only on the other device, sending an output that nothing plays on sends silence. If it stays silent for about 5 seconds while the default output has sound, a note says so.

Plug a device in or take it out and the lists update without restarting anything. If a chosen device isn't there, the system default is used and a note says so, and mesh-kvm goes back to your choice when the device returns. With System default chosen, capture and muting both follow the default to its new device.

### Sending sound both ways

When both devices send an output, each must leave out what it plays, or the sound goes round and round as an echo.

- On Debian, mesh-kvm doesn't record the output. It connects to each app playing there, and leaves itself out, so this always works. It also means that the output's own volume and mute don't change what's sent.
- On Windows, recording an output includes everything playing on it. If received audio plays on a different output, that's fine. If it plays on the same output, then while both directions are in use mesh-kvm asks Windows for all sound except its own, on Windows 10 version 2004 or later. Windows can't limit that to one output, so a line under Output to send says that all of the laptop's sound is being sent. If that fails, mesh-kvm goes back to ordinary recording, the app warns that you may hear an echo, and it tries again now and then, unless Windows says it can't do this at all.

A microphone that can hear the speakers will still pick them up. Headphones avoid that.

### How it works

Audio travels on UDP port 24851 as Opus in 10 ms packets, or plain PCM in 5 ms packets when Uncompressed is chosen. A jitter buffer on the receiving side holds as little as the network allows: one packet on a clean link, more when packets arrive unevenly, and it shrinks back once things calm down. A missing packet is rebuilt or made up by the decoder rather than left as a gap. Both devices need the same packet format, so update them together. If they differ, audio stays silent and the app says so.

Opus is built from the copy in `third_party/opus`, so building needs only a C compiler, on both targets. Its licence is under Licences in the Settings menu, and `mesh-kvm --licenses` shows it too. `tests/audio_measure.py` measures quality, delay and bandwidth at each bitrate with `tc netem` adding loss and jitter. `tests/audio_devices.py` runs two whole devices beside private sound servers, and checks echo, muting, devices coming and going, and mixing. Neither can reach your real audio.

## Bad Wi-Fi

mesh-kvm is built for a network that loses packets, delays them, and now and then stops for a few seconds.

- Keys and clicks get through, in order. Each press and release goes three times, numbered, and carries the few before it, so any packet that arrives repairs what was lost ahead of it. Ctrl lands before C, and letters don't swap. If something is still missing, later keys wait for it for at most 30 ms, so a clean link adds no delay. Mouse movement isn't repeated, as the next movement takes its place.
- Keys and mouse buttons can't stick. Every heartbeat, about 2.5 times a second, says what the sending device is holding down, and a key the other device has down that the sender no longer holds is let go at once.
- A press that comes out of a stall over a second late is dropped, as you've gone on to other things, though a release always gets through. Nothing is typed twice, however a stall ends.
- If a press still gets lost, the heartbeats correct that too, more carefully. A key is pressed late only if two heartbeats in a row say it's held, so a tap never turns up after you've let go. A mouse button is pressed late only if the pointer hasn't moved, as it would otherwise land on something else.
- In a dropout, once 1.35 seconds pass with nothing heard, everything held is let go on both devices, and each one has its own keyboard and mouse. When the link is back, control returns to the device that had it, usually within a second. Until the other device has shown it can hear this one, nothing is swallowed, so a link that only works one way can't take your keyboard. A Ctrl, Shift, Alt or Windows key you've held all the way through, for up to 5 seconds, is held again when control returns. On Windows that takes Windows having seen the key too, which it only has if it was repeating.
- Control stays where it is if you've moved on. That means a key, click or scroll on the device in front of you during the dropout, or a second of steady mouse movement, from a moment after the link went. So if you keep typing through a dropout without noticing, what you type lands on the device in front of you, and control stays there until you push across or use the switch. A brief stutter with the link still up doesn't count: control comes back whatever you press. It also stays if you chose Release all input on either device, if a game lock is on, or if the dropout lasts more than 15 seconds. Pushing across an edge or using the switch counts as your choice too.
- After an outage, a device that isn't the main one stays on its own keyboard and mouse until someone really switches, from either device. The main device can't take it back just by coming into range.
- If the other device can be heard but can't hear this one, even from the moment the link came up, as with a firewall on it, this device keeps its keyboard and mouse. It takes them back after 2.5 seconds if they had crossed, refuses to cross, and says so. Control returns in the same way once the other device can hear.
- Text you copy while the other device can't be reached goes when the link is back. Only the newest copy waits, and for at most 10 minutes. Anything copied since on either device takes its place, and if both devices have text waiting, both end up with the later one. Images and files don't wait: a note under Clipboard says what stayed behind, and to copy it again.

The Input section shows how the link looks from each device: the worst loss in either direction, counting what audio loses as well as the link's own packets, and the time for a packet to get there and back. Hover over it for each direction. It stands out from 5% loss or 150 ms. A figure only shows once it's known. While the link is down it gives no figures, and the other device shows as Link down. If the other device can't hear this one, it says that instead.

## Arranging screens

Every monitor on both devices appears in the app, including ones to the left of or above a device's main screen, with its name, resolution and refresh rate.

Drag any screen to arrange the devices. The whole device moves together: mesh-kvm arranges how the two devices meet, and each device's own monitors stay where its operating system has them. To change those, use that device's display settings. A dragged device snaps to the edge of the other device's nearest screen, never lands on top of one, and lines up with a screen's start, middle or end when it comes close. The amber line shows where the mouse crosses.

An arrangement you change on either device is saved there and passed to the other one. If the other device is away, your change is kept and reaches it when it comes back. Changes to different things on the two devices are both kept, such as moving a device on one and changing the push needed to cross on the other. If both devices changed the same thing, the newer change wins, and on a tie the second device's does.

The layout is saved as `layout.json` beside the settings: `~/.config/mesh-kvm` on Debian and `%LOCALAPPDATA%\mesh-kvm` on Windows. Pointer speed and the game lock settings belong to each device and aren't shared.

## Resolution and refresh rate

Each screen has a resolution and a refresh rate menu, which list what that monitor offers. Click a screen on the map to find its menus. Changing one applies on the device the monitor belongs to, from either device's app.

The app then asks whether to keep the change. Keep this only appears on the device with that screen, since that's where you can see it worked. The other device's app can only send it back. If nobody answers within 15 seconds, the screen goes back by itself. The service does that, so it still happens if the app closes or the screen goes blank, and the devices return to where they sat in the arrangement.

A screen counts as back only once mesh-kvm has seen it on its old mode. Until then it keeps trying, including after the service restarts or a monitor comes back, and no new change can start. Only one change can be waiting at a time.

Debian uses `kscreen-doctor`, from the `libkscreen-bin` package, and Windows uses its own display settings. On Windows a new mode is only saved once you keep it. mesh-kvm offers the menus only while the desktop answers about its monitors. If it doesn't, the screen shows its current resolution and refresh rate and says they can't be changed for now. On KDE this happens when KWin has no outputs, such as with every monitor off or unplugged. `kscreen-doctor` then never exits, so mesh-kvm stops it after 5 seconds.

## Updating

The encrypted release can't talk to older versions at all, so update both devices together and then pair them. Until they're paired each device works alone, which is why the code has to be typed on the laptop's own keyboard.

If only one device is updated, it ignores the other's plaintext and the app says that device needs updating. The older device drops what the new one sends, as it can't read it.

To roll back, reinstall 0.6.1 on both. It ignores `pairing.json`, and traffic is plaintext again.

Your settings carry over. The first start finds the address in the `--peer` your launchers already use and saves it. On Debian, it imports `layout.json` along with its pointer speed, and leaves the old file where it is. It looks in the folder the service runs in, then `~/mesh-kvm`, then the folders above the program.

1. On the laptop, double-click the new `mesh-kvm.exe`. This is the normal way to install. If mesh-kvm is already running, the new copy is installed over the old one (the old one is moved aside and deleted later), the launchers are rewritten so the tray starts at login, and the new version takes effect once every `mesh-kvm.exe` is ended in Task Manager (Details tab) and started again, or at the next login.
2. On Debian, replace the binary, restart mesh-kvm, then run `mesh-kvm --install-launchers` once to refresh the menu entry and the tray's autostart entry.
3. Delete old downloads. Running any `mesh-kvm.exe`, old ones included, replaces the installed copy, so an old download double-clicked later puts the old version back.
4. Pair the two devices, as under [Pairing](#pairing).

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

- On Windows, Play only on the other device sends every app's sound. It can't send just one output and mute it, because a mute silences a recording of that output.
- The Windows lock screen and UAC prompts can't be controlled remotely. Use the laptop's own keyboard and trackpad for those.
- Packet sizes and timing still show when someone is typing, and nothing stops a program on the same device calling the local API. See [What is and isn't protected](#what-is-and-isnt-protected).
- Addresses are IPv4 only, and there is no automatic discovery.
- There is no Mac build yet.

## Tests

`cargo test` runs the unit tests. `tests/netem_link.py` puts two devices on a bad network with `tc netem`, with a pretend person typing and clicking on one and a tone playing to the other. It covers loss, delay, reordering, 2-second dropouts in both directions, using the local device during one, one-way links, an outage past 15 seconds, a flapping link, Release all input on either device, and a restart. It checks that keys arrive in order, Ctrl+C stays Ctrl+C, no key sticks, audio keeps playing and control returns only when it should. Its top comment says how to run it. Input there is the real router, link and sockets with the keyboard and mouse left out, and audio has private sound servers, so it can't reach your real desktop or audio. `tests/fake_service.py` pretends to be the service for two devices, with 3 or 6 monitors, so the app window can be looked at and resized with nothing real behind it. Its top comment says how to run it. `tests/wine_link.py` runs the Windows build under Wine and plays the other device, sending hand-made packets and reading the ones that come back. It checks the link, the active-device logic, input and edges in both directions, held keys and late packets, dropouts, the audio bus, layout sync, and saving and changing the address:

```
unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_link.py [path/to/mesh-kvm.exe]'
```

The exe path defaults to `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe` in the repository and can also be set with `MESH_KVM_EXE`. Run it from the repository root; it needs `wine`, `ip` and Python 3.12 or later. Set `WINE_LINK_DISPLAY` to a throwaway X display to also press a real key and move the real mouse on the laptop, which needs `xdotool`. Set `WINE_LINK_PREFIX` to put the Wine prefix somewhere other than `/tmp`. It speaks to the exe through `tests/noise_peer.py`, a second Noise implementation in Python, and ends with the Linux build pairing with the exe for real.

`tests/capture_link.py` shows what someone on the network gets. A sniffer on a veth pair first proves it can see the markers in the clear. Then two real builds pair and call each other's API while keys, sound and clipboard text cross the same cable, and none of it may show up in the capture. Last, a third namespace takes the peer's IP address and tries plain packets, a handshake from the wrong key, and ports 24852, 24853 and 24880, and any answer fails. Its top comment says how to run it.
