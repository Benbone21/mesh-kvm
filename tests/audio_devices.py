#!/usr/bin/env python3
"""Two whole audio devices, each beside a private PipeWire, sending to each other over loopback.

It checks the things only real audio can show: that sound sent both ways doesn't come back as an
echo, that muting waits for the other device and restores, that devices can come and go, that a
microphone and an output mix without clipping, and how long sound takes from one device's speakers
to the other's.

Everything runs in its own namespaces, with its own sound servers on null outputs. It can't reach
your real audio, and all of it ends when this script does:

    unshare -rnpf --kill-child --mount-proc python3 tests/audio_devices.py path/to/test-binary

The test binary is what `cargo test --release --no-run` prints. Name scenes after it to run only
those: idle, loop, both, mute, hotplug, mix, delay. Set NETEM to add loss and jitter, for example
NETEM="delay 5ms 8ms distribution normal loss 5%". numpy must be importable.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

import numpy as np

RATE = 48000
FMT = ["--raw", "--rate=48000", "--channels=2", "--format=f32", "--latency=10ms"]
TC = "/usr/sbin/tc"
BASE = None
BIN = None
failures = 0

PIPEWIRE_CONF = """context.properties = { core.daemon = true core.name = pipewire-0 default.clock.rate = 48000 default.clock.quantum = 1024 default.clock.min-quantum = 32 default.clock.max-quantum = 2048 link.max-buffers = 16 mem.warn-mlock = false }
context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert  support.* = support/libspa-support }
context.modules = [
  { name = libpipewire-module-protocol-native }
  { name = libpipewire-module-access args = { } }
  { name = libpipewire-module-profiler }
  { name = libpipewire-module-metadata }
  { name = libpipewire-module-spa-node-factory }
  { name = libpipewire-module-client-node }
  { name = libpipewire-module-client-device }
  { name = libpipewire-module-adapter }
  { name = libpipewire-module-link-factory }
  { name = libpipewire-module-session-manager }
]
context.objects = [
  { factory = spa-node-factory args = { factory.name = support.node.driver node.name = Dummy-Driver node.group = pipewire.dummy priority.driver = 20000 } }
  { factory = adapter args = { factory.name = support.null-audio-sink node.name = test_speakers node.description = "Test speakers" media.class = Audio/Sink audio.position = [ FL FR ] object.linger = true priority.session = 2000 priority.driver = 2000 node.driver = true monitor.channel-volumes = true } }
  { factory = adapter args = { factory.name = support.null-audio-sink node.name = test_headset node.description = "Test headset" media.class = Audio/Sink audio.position = [ FL FR ] object.linger = true priority.session = 1000 priority.driver = 1000 node.driver = true } }
  { factory = adapter args = { factory.name = support.null-audio-sink node.name = test_mic node.description = "Test microphone" media.class = Audio/Source/Virtual audio.position = [ FL FR ] object.linger = true priority.session = 1000 node.driver = true } }
]
"""

WIREPLUMBER_CONF = """wireplumber.profiles = { main = {
  hardware.audio = disabled
  hardware.bluetooth = disabled
  hardware.video-capture = disabled
  support.dbus = disabled
  support.reserve-device = disabled
  support.portal-permissionstore = disabled
  support.logind = disabled
  monitor.alsa = disabled
  monitor.alsa-midi = disabled
  monitor.bluez = disabled
  monitor.bluez-midi = disabled
  monitor.libcamera = disabled
  monitor.v4l2 = disabled
} }
"""


def check(name, ok, detail=""):
    global failures
    failures += 0 if ok else 1
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail else ""), flush=True)


def env_for(name):
    root = f"{BASE}/{name}"
    return dict(os.environ, HOME=f"{root}/home", XDG_RUNTIME_DIR=f"{root}/run", PIPEWIRE_RUNTIME_DIR=f"{root}/run",
                XDG_CONFIG_HOME=f"{root}/home/.config", XDG_STATE_HOME=f"{root}/home/.state",
                XDG_DATA_HOME=f"{root}/home/.data", XDG_CACHE_HOME=f"{root}/home/.cache",
                DBUS_SESSION_BUS_ADDRESS="disabled:", DBUS_SYSTEM_BUS_ADDRESS="disabled:")


def start_sound_server(name):
    root = f"{BASE}/{name}"
    os.makedirs(f"{root}/home/.config/wireplumber/wireplumber.conf.d")
    os.makedirs(f"{root}/run", mode=0o700)
    with open(f"{root}/pipewire.conf", "w") as f:
        f.write(PIPEWIRE_CONF)
    with open(f"{root}/home/.config/wireplumber/wireplumber.conf.d/99-private.conf", "w") as f:
        f.write(WIREPLUMBER_CONF)
    quiet = dict(stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.Popen(["pipewire", "-c", f"{root}/pipewire.conf"], env=env_for(name), **quiet)
    for _ in range(50):
        if os.path.exists(f"{root}/run/pipewire-0"):
            break
        time.sleep(0.1)
    subprocess.Popen(["wireplumber"], env=env_for(name), **quiet)


def tool(name, *args, **kw):
    return subprocess.run(args, env=env_for(name), capture_output=True, text=True, timeout=10, **kw)

def device(name, listen, peer, seconds, **choices):
    e = env_for(name)
    e.update(MK_LISTEN=str(listen), MK_PEER=str(peer), MK_SECONDS=str(seconds), **choices)
    return subprocess.Popen([BIN, "--ignored", "--exact", "audio::live::whole_device", "--nocapture"], env=e,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)

class Reader(threading.Thread):
    def __init__(self, process):
        super().__init__(daemon=True); self.p = process; self.last = {}; self.start()
    def run(self):
        for line in self.p.stdout:
            i = line.find("STATUS ")
            if i >= 0:
                try: self.last = json.loads(line[i + 7:])
                except ValueError: pass

def play(name, freq, seconds, node, sink="test_speakers"):
    """An app playing a tone."""
    t = np.arange(int(RATE * seconds)) / RATE
    s = (0.25 * np.sin(2 * np.pi * freq * t)).astype("<f4")
    data = np.stack([s, s], 1).tobytes()
    p = subprocess.Popen(["pw-play", *FMT, "--target", sink, "-P", f"node.name={node}", "-"], env=env_for(name),
                         stdin=subprocess.PIPE, stderr=subprocess.DEVNULL)
    def feed():
        try:
            p.stdin.write(data); p.stdin.close()
        except OSError: pass
    threading.Thread(target=feed, daemon=True).start()
    return p

def record(name, seconds, sink="test_speakers"):
    """What a sink is really playing, from its monitor: this test's ears."""
    p = subprocess.run(["timeout", "-s", "INT", str(seconds), "pw-record", *FMT, "-P", "stream.capture.sink=true",
                        "-P", "node.name=test-ears", "--target", sink, "-"], env=env_for(name), capture_output=True)
    return np.frombuffer(p.stdout[: len(p.stdout) // 8 * 8], "<f4").reshape(-1, 2)

def level(pcm, f):
    l = pcm[:, 0].astype(float)
    if len(l) < RATE // 4: return -200.0
    l = l[len(l) // 4:]
    sp = np.abs(np.fft.rfft(l * np.hanning(len(l)))) / len(l) * 4
    fr = np.fft.rfftfreq(len(l), 1 / RATE)
    return 20 * np.log10(sp[(fr > f - 40) & (fr < f + 40)].max() + 1e-12)

def scene(name):
    print(f"\n=== {name}", flush=True)

def stop(*ps):
    for p in ps:
        try: p.stdin.close()
        except Exception: pass
        p.terminate()
    for p in ps:
        try: p.wait(3)
        except subprocess.TimeoutExpired: p.kill()

def echo_test():
    scene("both directions, tone A at 1 kHz on device A, tone B at 2.5 kHz on device B")
    a = device("a", 24951, 24952, 16, MK_CHOOSE_OUTPUT="default")
    b = device("b", 24952, 24951, 16, MK_CHOOSE_OUTPUT="default")
    ra, rb = Reader(a), Reader(b)
    time.sleep(1.5)
    pa, pb = play("a", 1000, 10, "tone-a"), play("b", 2500, 10, "tone-b")
    time.sleep(4)
    # What's heard on each, and whether each tone came back round to where it started.
    res = {}
    t = [threading.Thread(target=lambda n=n: res.__setitem__(n, record(n, 3))) for n in "ab"]
    [x.start() for x in t]; [x.join() for x in t]
    for n, own, other in (("a", 1000, 2500), ("b", 2500, 1000)):
        print(f"device {n}: own tone {level(res[n], own):.1f} dB, other's tone {level(res[n], other):.1f} dB")
        check(f"device {n} plays its own sound and the other's", level(res[n], own) > -20 and level(res[n], other) > -20)
    for n, r in (("a", ra), ("b", rb)):
        s = r.last
        check(f"device {n} is sure of no echo", s.get("echo_safe") is True and s.get("tx_reaching") is True, str(s.get("notes")))
    stop(a, b); pa.kill(); pb.kill(); pa.wait(); pb.wait()
    time.sleep(1)

def highest_serial(name):
    objects = json.loads(tool(name, "pw-dump", "-N").stdout)
    return max(o.get("info", {}).get("props", {}).get("object.serial", 0) if isinstance(o.get("info"), dict) else 0
               for o in objects)


def idle_test():
    scene("a device with nothing to do leaves the sound server alone")
    a = device("a", 24951, 24952, 9)
    time.sleep(3)
    before = highest_serial("a")
    time.sleep(5)
    # Each read of the graph is a new client, so a service that wakes itself shows up here. This
    # script's own pw-dump is one, and the five-second look another.
    grown = highest_serial("a") - before
    check("it isn't reading the graph over and over", grown <= 8, f"{grown} new objects in 5 seconds")
    stop(a)
    time.sleep(0.5)


def delay_test(kbps):
    scene(f"delay from speakers to speakers, {f'{kbps} kbit/s' if kbps else 'uncompressed'}")
    a = device("a", 24951, 24952, 12, MK_CHOOSE_OUTPUT="default", MK_KBPS=str(kbps))
    b = device("b", 24952, 24951, 12, MK_KBPS=str(kbps))
    ra, rb = Reader(a), Reader(b)
    time.sleep(1.5)
    n = RATE * 9
    click = (0.002 * np.sin(2 * np.pi * 300 * np.arange(n) / RATE)).astype("<f4")
    for at in range(RATE // 2, n - RATE, int(RATE * 0.4)):
        click[at:at + 96] = 0.6 * np.sin(2 * np.pi * 3000 * np.arange(96) / RATE)
    app = subprocess.Popen(["pw-play", *FMT, "--target", "test_speakers", "-P", "node.name=clicks", "-"], env=env_for("a"),
                           stdin=subprocess.PIPE, stderr=subprocess.DEVNULL)
    threading.Thread(target=lambda: (app.stdin.write(np.stack([click, click], 1).tobytes()), app.stdin.close()), daemon=True).start()
    # Both ears start together, and each timestamps what it reads against one clock.
    times = {}
    def ear(name):
        p = subprocess.Popen(["pw-record", *FMT, "-P", "stream.capture.sink=true", "-P", "node.name=test-ears", "--target", "test_speakers", "-"],
                             env=env_for(name), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        end = time.monotonic() + 7
        got, held = [], 0.0
        while time.monotonic() < end:
            data = p.stdout.read(960 * 8 // 4)
            now = time.monotonic()
            pcm = np.frombuffer(data[: len(data) // 8 * 8], "<f4")[0::2]
            hit = np.nonzero(np.abs(pcm) > 0.15)[0]
            if len(hit) and now - held > 0.2:
                # The time the first loud sample was made, working back from the end of the read.
                got.append(now - (len(pcm) - hit[0]) / RATE); held = now
        p.terminate(); p.wait(); times[name] = got
    t = [threading.Thread(target=ear, args=(n,)) for n in "ab"]
    time.sleep(0.3); [x.start() for x in t]; [x.join() for x in t]
    ta, tb = np.array(times["a"]), np.array(times["b"])
    check("clicks reach B", len(tb) > 0 and len(ta) > 0, f"{len(ta)} sent, {len(tb)} heard")
    if not (len(ta) and len(tb)):
        stop(a, b); app.kill(); app.wait()
        return
    d = [1000 * (tb[np.argmin(np.abs(tb - x - 0.05))] - x) for x in ta]
    d = [x for x in d if -5 < x < 300]
    if d:
        print(f"{len(ta)} clicks on A, {len(tb)} on B: added delay median {np.median(d):.1f} ms, min {np.min(d):.1f}, max {np.max(d):.1f}")
    else:
        print("no clicks matched", len(ta), len(tb))
    print("B status: buffer", rb.last.get("rx_buffer_ms"), "ms, loss", rb.last.get("rx_loss"), "% unrepaired", rb.last.get("rx_unrepaired"), "%; A hears back loss", ra.last.get("tx_loss"), "%")
    stop(a, b); app.kill(); app.wait()
    time.sleep(1)

def no_loop_test():
    scene("does sound come back? tone on A only, both devices sending their output, both playing")
    a = device("a", 24951, 24952, 12, MK_CHOOSE_OUTPUT="default")
    b = device("b", 24952, 24951, 12, MK_CHOOSE_OUTPUT="default")
    ra, rb = Reader(a), Reader(b)
    time.sleep(1.5)
    pa = play("a", 1000, 4, "tone-a")
    time.sleep(2.5)
    check("A's sound reaches B", rb.last.get("rx_peak", 0) > 10, f"peak {rb.last.get('rx_peak')}%")
    check("B doesn't send it back", rb.last.get("tx_peak") == 0, f"peak {rb.last.get('tx_peak')}%")
    check("nothing arrives back at A", ra.last.get("rx_peak") == 0, f"peak {ra.last.get('rx_peak')}%")
    pa.wait()
    time.sleep(0.4)
    r = record("a", 1.5)
    check("once it stops, A's speakers are silent", np.abs(r).max() < 1e-4, f"peak {np.abs(r).max():.6f}")
    stop(a, b)
    time.sleep(1)

def muted(name, sink_id):
    return "MUTED" in tool(name, "wpctl", "get-volume", sink_id).stdout

def sink_id(name, node):
    for line in tool(name, "wpctl", "status", "--name").stdout.splitlines():
        if node in line:
            return line.replace("│", " ").replace("*", " ").split()[0].rstrip(".")

def mute_test():
    scene("play only on the other device: A sends its speakers to B")
    sid = sink_id("a", "test_speakers")
    a = device("a", 24951, 24952, 27, MK_CHOOSE_OUTPUT="default", MK_MUTE="1")
    ra = Reader(a)
    time.sleep(3)
    check("with nothing confirming it's heard, A isn't muted", not muted("a", sid))
    b = device("b", 24952, 24951, 17)
    rb = Reader(b)
    time.sleep(2.5)
    check("B listening with nothing to hear doesn't mute A", not muted("a", sid))
    pa = play("a", 1000, 8, "tone-a")
    time.sleep(3)
    check("once B confirms it, A is muted", muted("a", sid))
    heard = level(record("b", 1), 1000)
    check("a muted output still sends", heard > -20, f"{heard:.1f} dB on B")
    pa.kill(); pa.wait()
    time.sleep(4)
    check("a pause in the sound doesn't bring the output back", muted("a", sid))
    check("and the app says connected, not playing", ra.last.get("tx_connected") is True and ra.last.get("tx_reaching") is False,
          f"connected {ra.last.get('tx_connected')}, reaching {ra.last.get('tx_reaching')}")
    pa = play("a", 1000, 8, "tone-a")
    time.sleep(1)
    stop(b)
    time.sleep(3.2)
    check("with B gone, A comes back", not muted("a", sid))
    pa.kill(); pa.wait()
    stop(a)
    time.sleep(0.5)
    check("and stays back after exit", not muted("a", sid))

def hotplug_test():
    scene("a chosen output comes and goes")
    a = device("a", 24951, 24952, 16, MK_CHOOSE_OUTPUT="hot_sink", MK_CHOOSE_INPUT="hot_mic")
    b = device("b", 24952, 24951, 16)
    ra, rb = Reader(a), Reader(b)
    time.sleep(1.5)
    check("a missing device leaves a note", len(ra.last.get("notes", [])) == 2, str(ra.last.get("notes")))
    args = "{ factory.name=support.null-audio-sink node.name=hot_sink node.description=Hot media.class=Audio/Sink audio.position=[FL FR] node.driver=true }"
    cli = subprocess.Popen(["pw-cli"], env=env_for("a"), stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, text=True)
    cli.stdin.write(f"create-node adapter {args}\n"); cli.stdin.flush()
    time.sleep(1)
    check("plugging it in lists it and clears its note",
          "Hot" in [d["name"] for d in ra.last.get("outputs", [])] and len(ra.last.get("notes", [])) == 1)
    pa = play("a", 1000, 3, "tone-hot", sink="hot_sink")
    time.sleep(1.5)
    check("the chosen output is what's sent, with no restart", rb.last.get("rx_peak", 0) > 10)
    cli.terminate(); cli.wait()
    pa.kill(); pa.wait()
    time.sleep(1)
    check("unplugging it brings the note back", len(ra.last.get("notes", [])) == 2)
    pd = play("a", 1000, 3, "tone-default")
    time.sleep(1.5)
    check("the default stands in", rb.last.get("rx_peak", 0) > 10)
    pd.kill(); pd.wait()
    time.sleep(0.5)
    tool("a", "wpctl", "set-default", sink_id("a", "test_headset"))
    time.sleep(0.5)
    ph = play("a", 1000, 3, "tone-headset", sink="test_headset")
    time.sleep(1.5)
    check("and capture follows the default to a new device", rb.last.get("rx_peak", 0) > 10)
    ph.kill(); ph.wait()
    tool("a", "wpctl", "set-default", sink_id("a", "test_speakers"))
    stop(a, b)
    time.sleep(1)

def mix_test():
    scene("microphone and output together, both loud")
    a = device("a", 24951, 24952, 10, MK_CHOOSE_OUTPUT="test_speakers", MK_CHOOSE_INPUT="test_mic")
    b = device("b", 24952, 24951, 10)
    ra, rb = Reader(a), Reader(b)
    time.sleep(1.5)
    t = np.arange(RATE * 6) / RATE
    def loud(f, sink, node):
        s = (0.95 * np.sin(2 * np.pi * f * t)).astype("<f4")
        p = subprocess.Popen(["pw-play", *FMT, "--target", sink, "-P", f"node.name={node}", "-"], env=env_for("a"), stdin=subprocess.PIPE, stderr=subprocess.DEVNULL)
        threading.Thread(target=lambda: (p.stdin.write(np.stack([s, s], 1).tobytes()), p.stdin.close()), daemon=True).start()
        return p
    p1, p2 = loud(700, "test_speakers", "tone-out"), loud(1900, "test_mic", "tone-mic")
    time.sleep(2.5)
    r = record("b", 2)
    check("both reach B", level(r, 700) > -12 and level(r, 1900) > -12, f"{level(r, 700):.1f} and {level(r, 1900):.1f} dB")
    check("without clipping", np.abs(r).max() < 1.0, f"peak {np.abs(r).max():.3f}")
    p1.kill(); p2.kill(); p1.wait(); p2.wait()
    stop(a, b)
    time.sleep(1)

def main():
    global BASE, BIN
    BIN = sys.argv[1]
    which = sys.argv[2:] or ["idle", "loop", "both", "mute", "hotplug", "mix", "delay"]
    subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
    BASE = tempfile.mkdtemp(prefix="mkpw.", dir=os.environ.get("AUDIO_TMP", "/tmp"))
    try:
        start_sound_server("a")
        start_sound_server("b")
        time.sleep(1.5)
        if os.environ.get("NETEM"):
            subprocess.run([TC, "qdisc", "add", "dev", "lo", "root", "netem", *os.environ["NETEM"].split()], check=True)
        scenes = {"idle": idle_test, "loop": no_loop_test, "both": echo_test, "mute": mute_test, "hotplug": hotplug_test, "mix": mix_test}
        for name, scene_run in scenes.items():
            if name in which:
                scene_run()
        if "delay" in which:
            for kbps in [int(k) for k in os.environ.get("KBPS", "64,96,128,256,0").split(",")]:
                delay_test(kbps)
    finally:
        shutil.rmtree(BASE, ignore_errors=True)
    print("ALL PASS" if failures == 0 else f"{failures} FAILED")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
