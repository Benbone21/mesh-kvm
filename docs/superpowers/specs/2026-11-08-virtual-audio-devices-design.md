# Virtual mics and speakers for the other device

## What it's for

When one device sends its microphone, the other should see it as a microphone in its sound settings, so apps like Discord, OBS or a recorder can pick it. Debian should also get a speaker for the other device, so anything played into it comes out of the laptop. On Windows, where a new device needs a signed driver, mesh-kvm uses a VB-Cable the user has installed.

### Decided with the user

- A received mic goes **only** to the virtual mic. It isn't played on the speakers.
- The virtual devices appear **while the devices are paired and linked**, and stay for 30 s after the link drops before they go away.
- On the laptop, a free VB-Cable carries **Debian's mic**. Sound from the laptop to Debian keeps using the existing "output to send" setting.
- Renaming VB-Cable's devices from code is out of scope, because it needs admin rights. The app says which device to pick, and the user can rename it in Windows Sound settings.

### Tested on Debian (2026-11-08)

- A PipeWire null sink shows up as an output with the description given, and its monitor carries what's played into it.
- A remap source on a null sink's monitor shows up as a microphone and records what's played into that sink.
- A null sink created with `media.class=Audio/Source/Virtual` shows as a microphone. `pw-play --target` did not link to it, so it recorded silence.
- **Chosen for the build:** `module-pipe-source` fed through a FIFO shows only a microphone, with no extra output. It records what's written, and gives silence when nothing is. Both devices are loaded with `priority.session=1 priority.driver=1` (in one quoted `source_properties`/`sink_properties` value), as WirePlumber otherwise gives a pipe source driver priority 50000 and makes it the default microphone over the user's own choice.

## Design

### 1. Two streams each way

Today each device mixes its mic and its output sound into one stream, which the receiver plays on its speakers. That stream is split in two:

- **Sound**: the chosen output's capture, plus the virtual speaker's monitor on Linux. It plays on the receiver's "play received audio on" output, as now.
- **Mic**: the chosen microphone. It goes to the receiver's virtual mic, and nowhere else.

Each stream has its own packetizer and Opus encoder, its own `RxStream` (jitter buffer and loss repair), and its own reports. The audio packet header carries a stream id, which means packet `VERSION` goes to 3. A device on version 2 is refused as another version and kept quiet, so both devices update together. The bitrate setting applies to each stream. Volume, the limiter and the both-sending warning apply to the sound stream only.

A stream that has nothing to carry sends nothing, so a device with no mic chosen opens no mic stream. The existing "microphone to send" dropdown stays as it is. Only where the receiver puts that sound changes.

### 2. Debian: devices while linked

While the laptop is paired and the link is up, the Linux service creates two devices:

- **"<peer name> mic (mesh-kvm)"**, a source that the received mic stream plays into.
- **"<peer name> speakers (mesh-kvm)"**, a sink whose monitor is captured and mixed into the sound stream sent to the laptop.

The peer name is the other device's own name (its hostname, or the name set in mesh-kvm), as elsewhere in the app.

- **Building the mic.** The first build step picks how to build the mic so that only a microphone appears in KDE's lists, with no extra output beside it. Candidates in order:
  1. A pipe source fed directly by mesh-kvm.
  2. A source the playback stream links to with `pw-link`.
  3. The tested null sink and remap source, with the sink hidden.
- **Taking devices away.** The devices are removed 30 s after the link drops (unless it returns first), at once when the peer is unpaired, and on Quit or service stop. Every module mesh-kvm loads carries a name with the `meshkvm_` prefix. At service start, any of those left by a crash are unloaded, because PipeWire keeps them after the process that made them is gone.
- **Keeping streams apart.** The capture for the sound stream already leaves out mesh-kvm's own playback. It must leave out the mic feed too, so the laptop's mic never comes back to the laptop. The virtual speaker's monitor is captured on its own and mixed in, whatever output is chosen to send.

### 3. Windows: VB-Cable

The service looks for VB-Cable's endpoints by their device names: render "CABLE Input" and capture "CABLE Output" (VB-Audio Virtual Cable).

- **With the cable:** the received mic stream plays through WASAPI into CABLE Input, so apps pick CABLE Output as their microphone. CABLE Input is left out of the "play received audio on" and "output to send" lists, so the cable isn't used twice.
- **Without the cable:** the mic stream isn't played. The receiver's report says it isn't ready, and the app shows a note on both devices.
- **Detection:** detection is re-checked with the device lists, so installing the cable works without a restart.

### 4. The app

The settings table gets a **Virtual devices** row with a cell per device. The cell says one of these:

- **Debian:** "SACBOOKPRO mic, SACBOOKPRO speakers — in sound settings", or "Appears when linked".
- **Windows, cable found:** "Pick CABLE Output as the mic for DEBIAN", with a hint that it can be renamed in Sound settings.
- **Windows, no cable:** "Install VB-Cable to get DEBIAN's mic here", with the download link `https://vb-audio.com/Cable/`.

On the sending device, the mic's cell says when the other side can't take it: "SACBOOKPRO has no virtual mic. Install VB-Cable there."

The cells have accessible names like every other cell (see the redesign spec).

## Errors and edge cases

- **The link drops mid-call:** both devices stay for 30 s (`DROP_GRACE`), the mic giving silence, so a short drop doesn't send apps back to their default mic, where they often stay. A link back within that keeps the same modules. After 30 s they go, and apps fall back to their default mic; when the link returns, they are created again with the same name, so apps that remember devices by name pick them up again. Unpairing and service stop still take them at once.
- **Creating a device fails:** for example, `pactl` is missing or PipeWire is restarting. The row says so, and it's retried with the device lists. Nothing else in audio stops.
- **The peer is renamed:** the devices are recreated with the new name.
- **Both devices send their mics:** this is fine, because each goes to the other's virtual mic and neither is captured as output sound.

## Testing

- **Packets:** stream ids round-trip, and a version 2 packet is refused.
- **Two streams:** the sender sends mic and sound independently. A mic-only sender opens no sound stream, and the reverse.
- **Linux, live:** the devices appear on link and go on unlink. A marked tone sent as mic is recorded from the virtual mic and not heard on the playback output. A tone played into the virtual speaker arrives in the sound stream. Leftover `meshkvm_` modules are cleared at start.
- **Windows, under Wine:** cable detection by device name, list filtering, and the no-cable note. Real playback into the cable is checked on the laptop.
- **On the real devices:** the laptop's mic is picked in a Debian app and records the user's voice. The Debian mic is picked as CABLE Output on the laptop. Nothing sent on one stream is heard on the other.

## Delivery

Ships as 0.9.0. Both devices must update together because of the packet version.
