# mesh-kvm

mesh-kvm shares one mouse and keyboard between a Debian desktop and a Windows laptop. Move the pointer off the edge of one screen and it appears on the other. It also shares the clipboard and audio, and it's made to hold up on poor Wi-Fi. The same program runs on both machines, one build per operating system.

The two devices are equal. Each one is both client and host, each keeps its own copy of the layout, and you can arrange screens and switch devices from either one. A Mac version is planned, but there is no Mac build yet.

Both devices run the same version: a Debian build and a laptop build from different versions may not understand each other's packets. From 0.7.0 on, one click updates both. See [Updating](#updating). A device on a version from before encryption shares nothing with this one, and each works alone until both are updated and paired.

Everything between the two devices is encrypted. They pair once with a 6-digit code, and after that a device that merely has the right IP address gets nothing.

## Build

You need Rust and a C compiler, plus the Windows target and a MinGW cross-compiler for the laptop build.

```
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu
```

The outputs are `target/release/mesh-kvm` for Debian and `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe` for Windows. `target/release/apply-update` is the Debian update helper. `packaging/build-deb.sh` makes the `.deb` from these, and `packaging/windows/build-setup.sh <version>` makes the Windows installer, `target/windows/mesh-kvm-setup.exe`, with `makensis` from the `nsis` package. Both use `$CARGO_TARGET_DIR` if that's set.

## Install

On Windows, run `mesh-kvm-setup.exe`. It installs mesh-kvm for you only, so it needs no admin rights, into `%LOCALAPPDATA%\mesh-kvm`, where the settings are kept too. It adds a Start menu entry, and asks whether to start mesh-kvm when you sign in, which is on unless you turned it off before. If mesh-kvm is running, it's stopped first. At the end it offers to open mesh-kvm. It appears in Installed apps in Windows' settings, with its version, and is removed from there. Removing it asks whether to delete your settings as well, and keeps them unless you say yes. `mesh-kvm-setup.exe /S` installs without asking, and `/AUTOSTART=off` turns starting at sign-in off. `uninstall.exe /S` removes it and keeps the settings, and `/DELETESETTINGS` deletes them as well.

A `mesh-kvm.exe` run from anywhere else, such as Downloads, doesn't install itself. With mesh-kvm installed, it opens the installed one's window and closes. With nothing installed, it runs as it is, and the window says to use `mesh-kvm-setup.exe` to install.

On Debian, install the `.deb`, or run `mesh-kvm --install-launchers` once. That adds mesh-kvm to your applications menu and starts the tray icon when you log in.

The first time the app opens on a device, it asks for the other device's address. Enter it on both devices. The app shows each device's own address in the box and next to the device's name. The address is saved, and you can change it later in the Address row of the window's table. Use an IPv4 address such as `192.168.1.20`.

## Pairing

The devices share nothing until they're paired. You do this once. It needs mesh-kvm open on both, and the other device's address set on the device that enters the code.

1. In the app, find Pair under BOTH DEVICES in the table and choose Show a code. The code is good for 2 minutes and one try.
2. On the other device, choose Pair under BOTH DEVICES, then Enter a code, and type the code. Use that device's own keyboard, as the devices can't share one until they're paired.
3. Both devices show the same fingerprint in the table, such as `4F2A-91C3`, and the link comes up.

A wrong code can't be tried again: show a new one. TCP port 24853 is open only while a code is showing. If the device showing the code has the other device's address saved, it accepts only that address. With no address saved it accepts any address for that 2-minute, one-try window, so someone else on the network could take the try, with one guess in a million per code. If the wrong address is saved, the device entering the code says so, and the code stays good.

Only the device showing the code needs its firewall to let 24853 in, for that long. Both need the usual UDP 24850 and 24851 and TCP 24852.

Without a window, `mesh-kvm --pair-show` and `mesh-kvm --pair <code>` do the same through the running service.

Unpair is in the same BOTH DEVICES group, and asks you to confirm first. It removes the other device here, and tells that device if it can be reached. If it can't, its handshakes are refused until you pair again. The keys are in `pairing.json`, in `~/.config/mesh-kvm` on Debian and `%LOCALAPPDATA%\mesh-kvm` on Windows, and unpairing keeps this device's own key. If that file can't be read, the device stays unpaired and the app says why.

### What is and isn't protected

Input, audio, the clipboard, files and calls to the other device's API are all encrypted and authenticated. That covers someone on the same network or Wi-Fi who can read, inject, replay, reorder or drop packets, or who takes the other device's IP address. The local API on port 24880 now only listens on the device itself.

It doesn't cover:

- The size and timing of packets, which still show when someone is typing.
- Jamming or flooding the link.
- A compromised device, or malware running as the same user. `pairing.json` holds the private key, and is readable by that user.
- Other programs on the same device reaching 127.0.0.1:24880, which has no token yet.

## Starting and stopping

Opening the app or the tray starts the background service if it isn't running, so there is nothing else to start. On Linux it uses the `mesh-kvm` systemd user service when there is one, and otherwise starts the service directly. If the service can't start, the app says so and names the log file. Each line in the log starts with the time it was written.

`--peer` and `--switch-edge` are optional. `--peer` sets the address for that run and saves it. While it is in use, the app can't change the address, because the flag would win again at the next start. `--switch-edge` says which side of this device the other one sits on, and only matters for a device with no layout yet.

## The app and the tray

Opening mesh-kvm from the Start menu or the applications menu shows the app window. It works the same from either device. You can also run `mesh-kvm --app`.

The window is a flat dark readout. From the top down:

- The status bar shows whether the devices are linked and which one has the keyboard and mouse, with Switch and Release all beside it.
- Under it, the update line appears when an update is ready or being installed. See [Updating](#updating).
- Five live figures follow, each with the last minute of history drawn beside it: ROUND TRIP, LOST, AUDIO LOST, AUDIO BUFFER and LINK.
- Below is one table with every setting for both devices side by side, this device first. It is grouped INPUT, SCREENS, CLIPBOARD, AUDIO, GAMES, STARTUP, BOTH DEVICES and UPDATES.
- The layout, your desk with every monitor of both devices to scale, sits beside the table, or under it in a narrow window. The screens of the device that has the keyboard and mouse are lit.

The window scrolls if it doesn't all fit. Everything sizes from the window, and a big window enlarges the text and controls a little. To change that, use App size under BOTH DEVICES, or Ctrl with plus, minus or 0. A larger app size raises the smallest size the window can go to, so nothing runs out of room. Motion is cut back if the system asks for reduced motion.

The window remembers its size and app size. It remembers its position on Windows and on X11. On Wayland the desktop chooses where windows open, so the position isn't kept. These are saved in `~/.local/share/mesh-kvm` on Debian and `%APPDATA%\mesh-kvm\data` on Windows.

Devices go by their own names. To rename one, type in its cell in the Name row at the top of BOTH DEVICES, from either device. Leave it empty to go back to its network name.

The tray icon sits in the notification area. Its menu has Open, Switch device, Release all input, and Quit mesh-kvm.

The window has its own taskbar button. Closing it with X closes only the window: sharing, the service and the tray keep running. Opening mesh-kvm again brings back the same window instead of making a second one, and if the tray isn't running, opening the window starts it.

Quit mesh-kvm in the tray stops all of mesh-kvm on that device: the window, the tray and the service. Anything Play only on the other device muted comes back first. On Debian under systemd, the `mesh-kvm` unit is stopped, so it doesn't start again by itself. The other device then shows this one as offline. To start again, open mesh-kvm from the Start menu or the applications menu, or sign in again with starting at sign-in on. `mesh-kvm --quit` does the same as the tray's Quit.

## Toggles

Each device has its own switches in the table. They read ON or OFF. If a device refuses a change, the switch goes back and a line under its row says why.

- Share mouse and Share keyboard turn sharing of each one on or off.
- Send clipboard and Receive clipboard choose which way the clipboard travels: text, images and files. Copied files move as soon as you copy them, up to 2 GB per copy, and the last two copies stay in a `received` folder beside the settings. If an app takes more than 2 seconds to hand over what it copied, a note says so, the copy goes when the app finishes, and a copy from anywhere else still works meanwhile.
- Low-latency Wi-Fi, under INPUT, pauses background Wi-Fi scans on Windows and marks input and sound as high priority. It's on by default. See [Low-latency Wi-Fi](#low-latency-wi-fi).
- Start when this device boots controls starting at login or boot. With it off, the tray still appears at login on Debian, but the service waits until you open the app.

Changes take effect straight away and are kept when mesh-kvm restarts.

## Audio

Each device has its own audio controls, and sound can go both ways at once.

- Output to send chooses what this device sends. It lists your real outputs by name, or None. Choosing one starts sending, and its sound plays on the other device's speakers. Send volume and the limiter that keeps the sound from clipping apply to it. Only output sound is shared: no microphone is sent.
- Play received audio on chooses where sound from the other device plays. Play audio from the other device turns it off.
- Play only on the other device mutes the output you chose to send, so you hear that sound on one device only. It is on by default. On Windows it also mutes the default output if that's a different one, and changes what's sent: see below.
- Audio quality chooses the bitrate of what this device sends: 64, 96, 128 or 256 kbit/s, or Uncompressed at about 1.6 Mbit/s. Each packet adds about 32 kbit/s on the network on top of that. On poor Wi-Fi, use a lower one: smaller packets get through more often.
- The lines under each part say how the audio is doing: how much is lost on the way, how much couldn't be repaired, and how much is held back to smooth out the network. A note appears if a capture can't be opened or delivers nothing.

### Upgrading from 0.9.0 or 0.9.1

Earlier versions could send a microphone, and 0.9.0 and 0.9.1 also gave each device a way to use the other one's: on Debian a mic and speakers in the sound settings, on the laptop a virtual cable you installed. That is gone. When mesh-kvm starts on Debian, it takes away any mic and speakers those versions left in the sound settings, and an app that was using them goes back to a real device. A virtual cable on the laptop is left installed, and is listed like any other device. A microphone chosen in those versions is forgotten, and the rest of the settings are kept. Audio packets are still version 3, so sound goes both ways with a device on 0.9.0 or 0.9.1 while the two are updated one at a time, and a microphone such a device still sends is ignored.

### Play only on the other device

On Windows, muting an output also silences an ordinary recording of it, and it follows the volume slider too. A test on the laptop showed that exactly. So with this on, mesh-kvm asks Windows for all sound except its own, which comes from before the mute and the volume slider. That has three effects: every app's sound on every output is sent, a small line under Output to send says so, and the level sent no longer changes with the laptop's volume. It also means the output chosen no longer decides what's sent, so the Windows default output is muted as well. If Windows won't do this, nothing is muted, sound plays on both devices, and a note says why. Debian doesn't need any of this.

The output is muted only once the other device confirms that real sound is coming out of its output. A pause in the sound keeps it muted for up to 30 seconds, so it doesn't go on and off between tracks. On Windows, where the capture is known to keep working under the mute, a pause of any length keeps it muted, so pressing play again doesn't sound on the laptop first. There it comes back if Play is turned off on the other device, if nothing has been heard from that device for 30 seconds, or if sound going out isn't heard there for 5 seconds. It comes back when you stop sending, the link drops, the other device stops playing, or sound from the other device is playing on that same output. If sound stops right after muting, the output comes back within seconds. On Debian, if that happens twice in half a minute, it stays on for a while and a note says so. If you had muted the output yourself, it stays muted, and if you unmute it by hand, mesh-kvm leaves it alone until sending next starts. If the output changes, say to a headset, the old one comes back and the new one is muted. If mesh-kvm stops unexpectedly, the tray unmutes after 5 seconds.

### Devices coming and going

On Windows without Play only on the other device, sending an output that nothing plays on sends silence. If it stays silent for about 5 seconds while the default output has sound, a note says so.

Plug a device in or take it out and the lists update without restarting anything. If a chosen device isn't there, the system default is used and a note says so, and mesh-kvm goes back to your choice when the device returns. With System default chosen, capture and muting both follow the default to its new device.

### Sending sound both ways

When both devices send an output, each must leave out what it plays, or the sound goes round and round as an echo.

- On Debian, mesh-kvm doesn't record the output. It connects to each app playing there, and leaves itself out, so this always works. It also means that the output's own volume and mute don't change what's sent.
- On Windows, recording an output includes everything playing on it. If received audio plays on a different output, that's fine. If it plays on the same output, then while both directions are in use mesh-kvm asks Windows for all sound except its own, on Windows 10 version 2004 or later. Windows can't limit that to one output, so a line under Output to send says that all of the laptop's sound is being sent. If that fails, mesh-kvm goes back to ordinary recording, the app warns that you may hear an echo, and it tries again now and then, unless Windows says it can't do this at all.

### How it works

Audio travels on UDP port 24851 as Opus in 10 ms packets, or plain PCM in 5 ms packets when Uncompressed is chosen. A jitter buffer on the receiving side holds as little as the network allows: one packet on a clean link, more when packets arrive unevenly, and it shrinks back once things calm down. A missing packet is rebuilt or made up by the decoder rather than left as a gap. Both devices need the same packet format, so update them together. If they differ, audio stays silent and the app says so.

Opus is built from the copy in `third_party/opus`, so building needs only a C compiler, on both targets. Its licence is under Licences in the UPDATES group, and `mesh-kvm --licenses` shows it too. The same list covers the bundled Inter and JetBrains Mono fonts, under the SIL Open Font License. `tests/audio_measure.py` measures quality, delay and bandwidth at each bitrate with `tc netem` adding loss and jitter. `tests/audio_devices.py` runs two whole devices beside private sound servers, and checks echo, muting, devices coming and going, and mixing. Neither can reach your real audio.

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

The ROUND TRIP and LOST figures at the top show how the link looks: the time for a packet to get there and back, and the worst loss in either direction, counting what audio loses as well as the link's own packets. Each device's own figures are in a note under BOTH DEVICES that starts with its name, such as `Laptop: Link: 0.5% lost, 12 ms round trip.` Hover over that note for each direction. A figure stands out from 5% loss or 150 ms. It shows `--` until it's known. While the link is down it gives no figures, and the other device shows as Link down. If the other device can't hear this one, it says that instead.

### Low-latency Wi-Fi

When both devices reach each other over Wi-Fi through one router, every packet crosses Wi-Fi twice, and the laptop's background scans for other networks take its radio off the channel for a moment, which shows as a round trip of 100 to 500 ms a few times an hour. While the devices are linked, the laptop puts each connected Wi-Fi interface in media streaming mode and turns its background scans off. Windows takes both back by itself if the Wi-Fi disconnects, and mesh-kvm sets them again after a change to the network, after sleep and once a minute. They go off once the link has been down for 30 seconds, when the setting is turned off, and when the service stops. On both devices, input packets are marked for priority, so Wi-Fi can send them ahead of anything else waiting. Debian marks input DSCP 46 (EF) and sound 34 (AF41), and its Wi-Fi sends them in the voice and video queues: Linux 6.8 and later map EF to voice and AF41 to video (RFC 8325), unless the router hands out a QoS map of its own. Windows marks them through qWAVE, which picks the values itself: 56 for input and 40 for sound. Choosing them there needs an administrator. qWAVE is an old API that Windows still ships, and needs the QoS Packet Scheduler bound to the Wi-Fi adapter, as it is by default. Whether the laptop's own Wi-Fi driver then sends them ahead isn't documented, so the certain gain on the laptop is on the router's side: a router that goes by DSCP puts them ahead on their way on to Debian.

Turn Low-latency Wi-Fi off on a device that has trouble moving between Wi-Fi access points, as with scans paused it may stay on a weak one for longer. A cable between a device and the router, or between the two devices, still does far more than this.

### The link log

Each device keeps a record of what the link did, so that a drop can be looked into afterwards from both ends. It's `link.log`, in `~/.config/mesh-kvm` on Debian and `%LOCALAPPDATA%\mesh-kvm` on Windows, and it moves to `link.log.1` once it passes 2 MB. Each line is a local time to the millisecond, a word, then details:

- `up` and `down`, with how long nothing was heard, and on a `down` the round trip, the loss each way and the address.
- `gap` for a stall that recovered before the link went: over 0.45 seconds in the middle of a stream of packets, or 0.7 seconds otherwise. `while_busy` says which. The pause after the mouse stops, until the next heartbeat 0.4 seconds on, isn't one.
- `one_way` when the other device stops hearing this one with the link up, and again when it hears it once more.
- A `sample` every 10 seconds. It also has how late this device's own input loop came round at worst (`loop_late_ms`), how far the other device's clock is ahead (`peer_clock_ms`), which lines the two logs up, and how many of the link's sends failed since the last sample (`send_errors`, with the latest kind in `send_error_last` and a count of each in `send_errors_by`, such as `no_buffer:3`). A `down` has the same count, so far. While sound comes from the other device, a sample also has its loss and buffer now (`audio_loss`, `audio_buffer_ms`), and since the last sample: packets received, frames concealed (lost and covered), repaired, packets late, frames trimmed, buffer restarts, and the fullest the buffer got (`audio_received`, `audio_concealed`, `audio_repaired`, `audio_late`, `audio_trimmed`, `audio_restarts`, `audio_buffer_max_ms`). A `stall` is the input loop over 0.3 seconds late.
- `control_up` and `control_down` for the encrypted connection that requests to the other device go over, with the error. The app and the layout sync use it, so with the app closed it may say nothing for a long time. Updating and unpairing use it too, even while the link is down, so a line can appear then. The clipboard and the once-a-minute look at the other device's clock have connections of their own, and they aren't in the log.
- `net`, `roam` and `signal` when this device's addresses or Wi-Fi change. `wifi=unknown` means the Wi-Fi couldn't be read, with why, and not that it went. On Windows 11 that can be the location permission Windows wants before it says anything about Wi-Fi.
- `asleep` when the device slept, and on Windows `suspend` and `resume`.
- `wifi_mode` on Windows when Low-latency Wi-Fi changes the Wi-Fi: `streaming=on ifaces=1 bgscan=off`, `streaming=off reason=link_down`, `reason=setting` or `reason=stop`, or `streaming=unavailable error="…"` when it can't, as with no Wi-Fi, Ethernet only or the WLAN service off. `bgscan=on` means background scans couldn't be paused, though streaming mode is on.
- `qos` when the marking of a socket that sends input or sound changes (`socket=input`, `audio_tx`, `audio_rx`), with how its packets are marked, such as `dscp=46 applied=ip_tos` on Debian or `dscp=56 applied=qwave traffic=voice dest=… flow=1` on Windows, `applied=none error="…"` when they can't be (tried again after a network change, a wake and every minute, and logged again only for a new error), and `applied=off reason=setting` with the setting off. On Windows the listening audio socket says it once sound arrives, as qWAVE needs to know where its replies go.
- `cross` for each crossing: `dir=to_laptop` or `to_debian`, where the push crossed (`at=x,y`, on this device) and where the pointer lands (`entry=x,y`, on the other), how old the position it went by was (`sample_age_ms`, always 0 on the laptop), the mouse movement added on since that position for the estimate (`est_dx`, `est_dy`), how long the completed push waited for a position or to leave a guard strip (`held_ms`), and how much of the full 250 ms settle was left (`settle_left_ms`: above 0 when a quick return went through early). A `cross` with `at` and `entry` a few pixels apart along the edge from where you'd expect points to the layout.
- `cross_refused reason=stale|guard|settle|off_span count=N` when a completed push didn't cross: the position was too old, it was in a guard strip, too soon after control arrived, or the estimate had left the bright edge. One line a second at most, with how many there were since the last line, and a push held for one reason counts once. A `guard` or `off_span` just before the side display took the pointer means they did their job.
- `cursor_tracker` on Debian, about the KWin script that says where the pointer is. `state=silent last_position_ms=…`: mouse movement has gone unanswered by any position for 1.5 seconds, so crossings go by the last position, as before. A mouse that's simply still isn't silence. `state=back`: positions are coming again. `state=bad_stamp`, once: a position's time came as a double, so positions are timed by when they arrived instead. `state=lost reason=silent` (silent for over 5 seconds while the mouse moves, as after KWin restarted) or `reason=dbus_monitor_exited`: the script and `dbus-monitor` are started again, at most once every 30 seconds.
- `peer_addr`, and `start`, `restarting`, `restart_failed` and `stop`. A `start` with no `stop` or `restarting` before it means the service was killed.

Both directions going at once with the Wi-Fi changing points to the network. A `one_way` points to a firewall or one side, a `stall` to this device being too busy, and an `asleep` or `roam` just before a `down` explains it. `mesh-kvm --link-log` prints the last 500 lines of this device's log, then the other device's, and says so if either can't be reached. The app's API serves them at `/api/local/linklog?lines=N`, up to 2000, including what the service before a restart wrote.

## Arranging screens

Every monitor on both devices appears in the app, including ones to the left of or above a device's main screen, with its name, resolution and refresh rate.

Drag any screen in the layout to arrange the devices. The whole device moves together: mesh-kvm arranges how the two devices meet, and each device's own monitors stay where its operating system has them. To change those, use that device's display settings. A dragged device snaps to the edge of the other device's nearest screen, never lands on top of one, and lines up with a screen's start, middle or end when it comes close. The bright edge shows where the mouse crosses. With a screen selected, the arrow keys move it by 10 pixels, and Shift with an arrow by 1. A move with the arrows that ends within 4 pixels of lining up lines up exactly, so long as that's the way it was going, so Shift and an arrow still move a lined-up screen off a pixel at a time. "Below" in the Where it sits menu puts the second device under the middle of that screen, which for a screen of the same width is exactly under it. The first time a layout loads with this version, if the second device is within 8 pixels of lining up with one screen of the same width (the same height, beside the first device), it is lined up exactly, and that goes to the other device like any other change: a few pixels off puts the cursor a few pixels off at every crossing. That happens once per layout, which `aligned_once` in the file records, so an offset you make afterwards stays.

An arrangement you change on either device is saved there and passed to the other one. If the other device is away, your change is kept and reaches it when it comes back. Changes to different things on the two devices are both kept, such as moving a device on one and changing the push needed to cross on the other. If both devices changed the same thing, the newer change wins, and on a tie the second device's does.

The layout is saved as `layout.json` beside the settings: `~/.config/mesh-kvm` on Debian and `%LOCALAPPDATA%\mesh-kvm` on Windows. Pointer speed and the game lock settings belong to each device and aren't shared.

### Crossing

Push the pointer against the bright edge and it crosses once it has pushed as far as the push needed to cross, in mouse counts. A button held down stops it, so a drag never carries a window across.

Once the pointer has crossed, it lands 12 pixels in from the edge it came in by, and 28 from any other edge it would land on. For a moment it can't cross back, so a hand still moving doesn't bounce it straight back: until 250 ms have passed, or until it has been 12 pixels further in than it landed and 60 ms have passed, so a quick return that's meant goes through at once.

Where another screen of the same device carries on the line of the bright edge without touching the other device, the last 48 pixels of the bright edge beside it don't cross. With DP-3's bottom carrying on DP-2's, and the laptop under DP-2 only, the pointer can then slide along the bottom from DP-2 onto DP-3 while pushing down without dropping into the laptop on the way. So Debian's x 1920 to 1967 doesn't cross down, while the laptop's x 0 to 47 beneath it still crosses up: the laptop has one screen, and nothing beside its edge to slide onto. That is meant. A push held in that strip crosses once the pointer moves out of it along the edge. Where the bright edge ends at the desktop's own corner, or the screen beside it touches the other device too, there is no strip.

On Debian, where the pointer is comes from a small KWin script, through D-Bus, a moment after the pointer got there. Each position carries a number and the time KWin took it, only the newest is kept, and the script sends at most one every 2 ms, always the latest, so nothing queues behind a fast mouse. The mouse movement since that position is added on to estimate where the pointer is now. A push crosses on a position younger than 12 ms, or one that nothing has moved since, as when you push straight against the edge, with both the position and the estimate on the bright edge. An older position crosses only when wherever the pointer could have got to since (three times the movement along the edge, either way, for pointer acceleration) is all on the bright edge and out of any guard strip, so a slow D-Bus doesn't hold up a push in the middle of it. Otherwise it waits for the next position, which comes within a few milliseconds of any movement, so a real crossing is late by one position at most and a late one can't carry the pointer across from a screen beside the bright edge. The pointer lands on the other device where the estimate says it was along the edge. After the pointer comes back to Debian, nothing counts until KWin says where it really is. Mouse movement is timed by the kernel's own clock for it, so a busy moment on Debian doesn't count movement KWin had already shown. If KWin's positions stop coming while the mouse moves, Debian goes by the last one, as before, and the link log says so. If that lasts, as after KWin restarted, the script is started again.

While Debian has control, the laptop's own cursor is parked in the middle of its screen, so that its trackpad's movement can be read from there, and hidden under a small window of mesh-kvm's that has a blank cursor. That window never takes focus, has no taskbar button and isn't in Alt+Tab. It goes as soon as the laptop has control again, with Release all input, when the link goes and control comes back, and when the service stops, and it ends with the process, so a crash can't leave it behind. On the laptop, a cursor that a game hides counts towards the automatic game lock only while the laptop is the one in use, its cursor isn't parked and half a second has passed since it was, and not when Windows merely isn't drawing it after touch or pen input. The parked cursor sits still under whatever app is in the middle of the screen, and an app that hides an idle cursor, such as a video player, could set the game lock, which then refused the next crossing back and kept control from returning after a dropout.

The app's layout marks where the cursor is with a small dot and ring in the accent colour, so it can't be taken for a second cursor.

## Resolution and refresh rate

Each screen has a resolution and a refresh rate menu, in the SCREENS rows of the table, which list what that monitor offers. Changing one applies on the device the monitor belongs to, from either device's app.

The app then asks whether to keep the change. Keep this only appears on the device with that screen, since that's where you can see it worked. The other device's app can only send it back. If nobody answers within 15 seconds, the screen goes back by itself. The service does that, so it still happens if the app closes or the screen goes blank, and the devices return to where they sat in the arrangement.

A screen counts as back only once mesh-kvm has seen it on its old mode. Until then it keeps trying, including after the service restarts or a monitor comes back, and no new change can start. Only one change can be waiting at a time.

Debian uses `kscreen-doctor`, from the `libkscreen-bin` package, and Windows uses its own display settings. On Windows a new mode is only saved once you keep it. mesh-kvm offers the menus only while the desktop answers about its monitors. If it doesn't, the screen shows its current resolution and refresh rate and says they can't be changed for now. On KDE this happens when KWin has no outputs, such as with every monitor off or unplugged. `kscreen-doctor` then never exits, so mesh-kvm stops it after 5 seconds.

## Updating

From 0.7.0 on, mesh-kvm updates itself. Each device's service looks at the latest full release on GitHub 2 minutes after it starts and every 6 hours after that, downloads a new one in the background, and checks it. This works with the app closed. Downloads go to `updates` beside the settings, carry on after a dropped connection, and try again with growing gaps. Nothing restarts until you click.

When an update is ready, a line at the top of the app says so, with a link to the release notes.

- Update both devices shows when both have it ready. The other device goes first, whichever one you click, and the line shows each step. Sharing drops for a few seconds on each. If the other device doesn't come back on the new version within 5 minutes, this one stays as it is and the line says which device failed.
- Update this device shows when the other device is offline or can't update, with a note that the two won't work together until both match.
- If the other device hasn't finished downloading, the line shows how far along it is, and the button waits. The device that's behind is told to look as soon as the other has found the update.
- A copy that isn't the installed one, such as one run from `target/release`, only says that a version is available. On Windows it also says to use `mesh-kvm-setup.exe` to install.
- UPDATES in the table shows the version running, has Check now, and says if a download was turned down or no check has worked for 3 days. A failed check is otherwise quiet.
- An update clicked during a screen change waits for the change to end.

Anything is installed only if it's signed by the release key built into the app, newer than what's running, the version it came as, and exactly the file that was signed. HTTPS isn't what's trusted, so a compromised GitHub account alone can't push an update. A download that doesn't match what was signed is deleted and fetched again from nothing, once. If it still doesn't match, or the signature is wrong, that version isn't tried again. A copy that isn't the installed one never turns a version down for the installed copy.

On Windows the new exe is copied beside the installed one and checked again, then takes the installed place with the old one kept as `mesh-kvm.exe.old`. No admin prompt is needed. A small watcher running from the old exe waits for the old service to end, for up to 20 seconds, and then ends it if it hasn't, as it holds the ports the new one needs. Only then does it start the new copy, which waits up to 20 more seconds for any port that's still taken. If the new copy doesn't have its ports, answer and reach 30 seconds old within 90 seconds of starting, the watcher puts the old one back and starts it, the app says what happened, and that version isn't offered again. Until it has proved itself, `/api/local` says `proving`, from its very first answer, and the other device doesn't follow it. The other device also waits until the new version has kept answering for 10 seconds, so a copy that answers and then dies isn't followed. A look or two lost on bad Wi-Fi doesn't start that wait over. If the laptop restarts before the new copy has proved itself, the login script starts the old exe as a watcher again, so the same thing happens at login, even if the crash left no installed exe at all. The watcher and everything else mesh-kvm starts on Windows get none of its open handles, so no port can stay held by one of them. After an update, Installed apps shows the new version.

On Debian a small helper, `/usr/lib/mesh-kvm/apply-update`, runs as root through `pkexec`, with no password for the active local user. It takes only a version, copies the download into `/var/cache/mesh-kvm`, which only root can reach, repeats every check on those copies, and installs through `apt-get install -y`, so the Timeshift hook still runs. If apt fails, the old version keeps running, the line shows apt's message, and it gives the command that puts the last good package back. mesh-kvm asks polkit beforehand, with `pkcheck`, which installs nothing, and `/api/local` reports the answer as `preflight`. If polkit would refuse or want a password, or `pkexec` says no anyway, the line says so and gives the exact `sudo apt-get install` command, and the old version keeps running. mesh-kvm stops waiting for the helper after 30 minutes and says so, though apt may still finish. The last two installed packages stay in `/var/cache/mesh-kvm/kept` as the way back by hand, as Debian has no automatic rollback.

After an update, the window and tray start again by themselves if they were open. A window that was closed stays closed.

### Updating by hand

Version 0.7.0 itself goes on by hand, on both devices, and so does anything older. On the laptop, 0.7.2 goes on by hand too, with `mesh-kvm-setup.exe`: the update to 0.7.1 could leave the laptop with nothing running. The two devices should run the same version: if one still runs an older mesh-kvm, the app says which one and turns off the controls it can't use. Against a 0.5 device, input still works as it did, with no correcting of lost keys and no return of control, and the app says so. The clipboard doesn't reach that device until it's updated, and a note says that too.

Your settings carry over. The first start finds the address in the `--peer` your launchers already use and saves it. On Debian, it imports `layout.json` along with its pointer speed, and leaves the old file where it is. It looks in the folder the service runs in, then `~/mesh-kvm`, then the folders above the program.

1. On the laptop, run `mesh-kvm-setup.exe` from the release. It stops whatever mesh-kvm is running, installs over it in the same place, keeps your settings, and opens it at the end.
2. On Debian, install the new `.deb` with `sudo apt-get install ./mesh-kvm_<version>_amd64.deb`, as that also brings the update helper, then restart mesh-kvm.
3. Delete old downloads of `mesh-kvm.exe`. One from before 0.7.2 still installs itself over the installed copy when it's double-clicked, which puts that old version back.
4. Pair the two devices, as under [Pairing](#pairing).

### Updating to the encrypted release

0.8.0 is the first version that encrypts. On two devices running 0.7.2, Update both devices gets 0.8.0 onto both, but it takes a second click, and the first one ends in a message that looks like a failure:

1. Click Update both devices on either device. The other device installs 0.8.0 and restarts. The device you clicked can't see it come back, because 0.8.0 only answers the device it's paired with, so its line shows Waiting for it to come back for 5 minutes.
2. Then the line says the other device didn't come back on version 0.8.0 within 5 minutes, so this device stayed as it is, with an Update this device button. The other device is fine: it's on 0.8.0. Click Update this device.
3. Once both are on 0.8.0, pair them, as under [Pairing](#pairing). Until then each device works alone, so type the code on the laptop's own keyboard.

From 0.8.0 on, Update both devices takes one click again, over the encrypted link. Updating, by either way, keeps the pairing: `pairing.json` is left alone by the `.deb`, by `mesh-kvm-setup.exe` and by the self-update. A fresh install starts unpaired, and uninstalling on Windows and choosing to delete the settings removes the pairing too.

If only one device is updated, it ignores the other's plaintext and the app says that device needs updating. The older device drops what the new one sends, as it can't read it.

To roll back, reinstall the earlier version on both. It ignores `pairing.json`, and traffic is plaintext again.

## Publishing a release

Once, make the signing key with `packaging/make-release-key.sh`. It writes the private key to `~/.config/mesh-kvm-release/signing.key`, readable only by its owner, and the public key to `packaging/release-key.pub`, and refuses to replace either. Commit the public key, and back up the private one: if it's lost, a new key can only reach devices through one more hand install. Until then `packaging/release-key.pub` holds a placeholder, and a build with it trusts no update at all.

Then, with the version in `Cargo.toml` already bumped and committed:

```
packaging/release.sh <version> <notes-file>
```

It refuses a placeholder key, a signing key that doesn't match the public one, uncommitted changes, a tag on another commit and a version that isn't newer than the latest release. It runs the tests, builds both targets, the `.deb` and `mesh-kvm-setup.exe`, and checks the programs as they'll ship: no test hooks, this key, this version. Then it writes and signs `release.json`, which lists the installer as well, uploads a draft, pushes the tag and publishes a full release. A device only looks up its own file in `release.json`, so older versions read it as before. If that fails partway, run it again: nothing sees a draft, and it carries on from a tag on the same commit. `--dry-run` stops before the tag and prints the rest, and never talks to GitHub, so `--latest <version>` stands in for the latest release there.

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
- Of updates, the other device can only ask for a download that has already passed its checks to be installed.
- A Windows update that's cut short is put right by the login script, which only exists with Start mesh-kvm when this device boots on. With that off, if the laptop restarts in the minute and a half after the swap and the new exe doesn't work, running `mesh-kvm-setup.exe` again is the way back by hand.
- Addresses are IPv4 only, and there is no automatic discovery.
- There is no Mac build yet.

## Tests

`cargo test` runs the unit tests. `tests/netem_link.py` puts two devices on a bad network with `tc netem`, with a pretend person typing and clicking on one and a tone playing to the other. It covers loss, delay, reordering, 2-second dropouts in both directions, using the local device during one, one-way links, an outage past 15 seconds, a flapping link, Release all input on either device, a restart, taps far apart on a link that loses packets in runs, and a 1000 Hz mouse on a link with less room than it needs. It checks that keys arrive in order, Ctrl+C stays Ctrl+C, no key sticks, audio keeps playing and control returns only when it should. Its top comment says how to run it. Input there is the real router, link and sockets with the keyboard and mouse left out, and audio has private sound servers, so it can't reach your real desktop or audio. `tests/fake_service.py` pretends to be the service for two devices, with 3 or 6 monitors, so the app window can be looked at and resized with nothing real behind it. Its top comment says how to run it. `tests/fake_service.py --secure` shows the pairing states. `tests/wine_link.py` runs the Windows build under Wine and plays the other device, sending hand-made packets and reading the ones that come back. It checks the link, the active-device logic, input and edges in both directions, held keys and late packets, dropouts and what the link log made of them, that Low-latency Wi-Fi logs its Wi-Fi and packet marks and carries on without them under Wine, the audio bus, layout sync, and saving and changing the address:

```
unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_link.py [path/to/mesh-kvm.exe]'
```

The exe path defaults to `target/x86_64-pc-windows-gnu/release/mesh-kvm.exe` in the repository and can also be set with `MESH_KVM_EXE`. Run it from the repository root; it needs `wine`, `ip` and Python 3.12 or later. Set `WINE_LINK_DISPLAY` to a throwaway X display to also press a real key and move the real mouse on the laptop, which needs `xdotool`. Set `WINE_LINK_PREFIX` to put the Wine prefix somewhere other than `/tmp`. Set `MESH_KVM_HOOKS_EXE` to a build made with `--features test-hooks` to also run a real self-update under Wine against a pretend release server: the swap, the other-device-first order, proving, the watcher, the restore after a new exe that won't start or hangs, a swap that can't happen, and restarts in the middle, recovered through the real login script. It also covers an old service that's slow to let go of its ports, a new copy that answers and then dies because its input port stays taken, and another device that does the same. It speaks to the exe through `tests/noise_peer.py`, a second Noise implementation in Python, and ends with the Linux build pairing with the exe for real. `tests/wine_setup.py` runs `mesh-kvm-setup.exe` under Wine: a silent install, a reinstall over a running copy, starting at sign-in off and on, a copy run from Downloads with and without an install, `--quit`, and a silent uninstall. Its top comment says how to run it.

The updater's unit tests use their own key and a release server on a local port. Only a build with the `test-hooks` feature can be pointed at another server or key, and `packaging/release.sh` refuses to ship one. `tests/deb_helper.py` attacks the real `apply-update` as root in a throwaway namespace with a pretend `apt-get`, so it can't reach the real apt, polkit, `/usr` or `/etc`. `tests/release_dry_run.py` checks everything `packaging/release.sh` refuses, then does a whole `--dry-run` on a throwaway clone with a throwaway key. Their top comments say how to run them. `tests/fake_service.py --update` shows the update line in each state.

`tests/capture_link.py` shows what someone on the network gets. A sniffer on a veth pair first proves it can see the markers in the clear. Then two real builds pair and call each other's API while keys, sound and clipboard text cross the same cable, and none of it may show up in the capture. Last, a third namespace takes the peer's IP address and tries plain packets, a handshake from the wrong key, and ports 24852, 24853 and 24880, and any answer fails. Its top comment says how to run it.
