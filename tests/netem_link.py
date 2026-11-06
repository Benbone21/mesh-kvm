#!/usr/bin/env python3
"""Two whole devices on a bad network: `tc netem` adds loss, delay, reordering and dropouts, while a
pretend person types and clicks on one device and sound plays to the other.

It checks what Stage 4 promises. Keys never stick: nothing stays down on the receiving device for
long after the person let go. Audio keeps playing. Control returns to the device that had it after
a dropout, and the local keyboard is never swallowed while the link is gone.

Input uses `input::live::whole_device`, which is the real router, link and sockets, with a recorder
in place of the keyboard and mouse. Audio uses `audio::live::whole_device` beside private sound
servers. Nothing here can reach your real desktop or audio, and all of it ends when this script does:

    unshare -rnpf --kill-child --mount-proc python3 tests/netem_link.py path/to/test-binary [scenes]

The test binary is what `cargo test --release --no-run` prints. The scenes are loss, delay, reorder,
dropout, mixed, reverse, oneway, release, restart and flood. numpy must be importable. Set AUDIO_TMP
to keep the sound servers' files somewhere other than /tmp.
"""
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import threading
import time

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import audio_devices as audio

TC = "/usr/sbin/tc"
PORT_A, PORT_B = 24961, 24962
TONE = 1000
# A release that got through nowhere is corrected by the next heartbeats. With a few lost in a row
# and the delay on top, this is the most anything may stay down. A dropout ends it at the link timeout.
MOST_HELD_MS = 1500
RETURN_MS = 2500
failures = 0
numbers = []


def check(name, ok, detail=""):
    global failures
    failures += 0 if ok else 1
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail else ""), flush=True)


def netem(*args):
    subprocess.run([TC, "qdisc", "replace", "dev", "lo", "root", "netem", *args], check=True)


def clean():
    subprocess.run([TC, "qdisc", "del", "dev", "lo", "root"], stderr=subprocess.DEVNULL)


class Device:
    def __init__(self, me, listen, peer, seconds, main=0, seq=None):
        env = dict(os.environ, MK_ME=str(me), MK_LISTEN=str(listen), MK_PEER=str(peer), MK_SECONDS=str(seconds), MK_MAIN=str(main))
        if seq is not None:
            env["MK_SEQ"] = str(seq)
        self.p = subprocess.Popen([audio.BIN, "--ignored", "--exact", "input::live::whole_device", "--nocapture"], env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.last = {}
        self.watchers = []
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.p.stdout:
            i = line.find("STATUS ")
            if i >= 0:
                try:
                    status = json.loads(line[i + 7:])
                except ValueError:
                    continue
                self.last = status
                for watcher in self.watchers:
                    watcher(status)

    def do(self, **action):
        try:
            self.p.stdin.write(json.dumps(action) + "\n")
            self.p.stdin.flush()
        except OSError:
            pass

    def wait_for(self, cond, seconds):
        end = time.time() + seconds
        while time.time() < end:
            if self.last and cond(self.last):
                return time.time()
            time.sleep(0.005)
        return None

    def stop(self):
        audio.stop(self.p)


class Person:
    """Types and clicks on `sender`, and keeps what's really down, to judge the other device by."""

    def __init__(self, sender, receiver):
        self.sender, self.receiver = sender, receiver
        self.down = set()
        self.up_at = {}
        self.lock = threading.Lock()
        self.worst = 0
        self.stuck_key = None
        self.taps = 0
        self.long_holds = 0
        self.long_seen = 0
        self.running = True
        receiver.watchers.append(self.judge)

    def judge(self, status):
        # Status times and ours are on one clock, as both are the same machine.
        now = status["ms"] / 1000
        with self.lock:
            for key in status["held"]:
                if key not in self.down and key in self.up_at:
                    late = (now - self.up_at[key]) * 1000
                    if late > self.worst:
                        self.worst, self.stuck_key = late, key

    def press(self, key):
        with self.lock:
            self.down.add(key)
            self.up_at.pop(key, None)
        self.sender.do(key=key, pressed=True)

    def release(self, key):
        self.sender.do(key=key, pressed=False)
        with self.lock:
            self.down.discard(key)
            self.up_at[key] = time.time()

    def typing(self, rng):
        """Taps, with a long hold now and then, and the mouse moving all the while."""
        while self.running:
            key = rng.randrange(16, 50)
            if rng.random() < 0.1:
                self.long_hold(key)
            else:
                self.press(key)
                self.pause(rng.uniform(0.03, 0.12))
                self.release(key)
                self.taps += 1
            self.pause(rng.uniform(0.02, 0.1))

    def long_hold(self, key):
        # Ctrl or Shift, say, with no auto-repeat to help.
        self.press(key)
        self.pause(1.2)
        self.long_holds += 1
        self.long_seen += key in self.receiver.last.get("held", [])
        self.release(key)

    def pause(self, seconds):
        end = time.time() + seconds
        while time.time() < end and self.running:
            self.sender.do(move=2)
            time.sleep(0.01)

    def stop(self):
        self.running = False
        with self.lock:
            down = list(self.down)
        for key in down:
            self.release(key)


class Sound:
    """A tone playing on device a, and what comes out of device b's speakers."""

    def __init__(self, seconds):
        self.a = audio.device("a", 24951, 24952, seconds + 3, MK_CHOOSE_OUTPUT="default")
        self.b = audio.device("b", 24952, 24951, seconds + 3)
        self.ra, self.rb = audio.Reader(self.a), audio.Reader(self.b)
        time.sleep(1.5)
        self.tone = audio.play("a", TONE, seconds + 2, "tone")
        time.sleep(1.5)
        self.heard = None
        self.thread = threading.Thread(target=self.listen, args=(seconds,), daemon=True)
        self.started = time.time()
        self.thread.start()

    def listen(self, seconds):
        self.heard = audio.record("b", seconds)

    def finish(self):
        self.thread.join()
        audio.stop(self.a, self.b)
        self.tone.kill()
        self.tone.wait()
        return self.heard

    def quiet_spells(self):
        """The share of 50 ms stretches with sound in them, and the longest silence, in ms."""
        left = self.heard[:, 0]
        step = audio.RATE // 20
        loud = np.array([np.abs(left[i:i + step]).max() > 0.01 for i in range(0, len(left) - step, step)])
        longest = run = 0
        for sounding in loud:
            run = 0 if sounding else run + 1
            longest = max(longest, run)
        return float(loud.mean()) if len(loud) else 0.0, longest * 50


def forwarding_pair(seconds, main=0, sender=0):
    """Two devices, with `sender`'s keyboard driving the other, and control settled there."""
    a = Device(0, PORT_A, PORT_B, seconds, main)
    b = Device(1, PORT_B, PORT_A, seconds, main)
    devices = (a, b)
    source, target = devices[sender], devices[1 - sender]
    source.wait_for(lambda s: s["online"], 5)
    target.wait_for(lambda s: s["online"], 5)
    time.sleep(0.9)
    source_id, target_id = sender, 1 - sender
    if main == sender:
        source.do(cross=True)
    # A device that isn't main starts out following main, which is already what's wanted.
    source.wait_for(lambda s: s["active"] == target_id, 5)
    target.wait_for(lambda s: s["active"] == target_id, 5)
    ok = source.last.get("active") == target_id and source.last.get("blocking") == [True, True]
    check("control is on the other device to begin with", ok, str({k: source.last.get(k) for k in ("active", "blocking", "online")}))
    return source, target, source_id, target_id


def judge_typing(person, target, name):
    gone = target.wait_for(lambda s: not s["held"] and not s["buttons"], 3)
    check("nothing is left down at the end", gone is not None, str(target.last.get("held")))
    print(f"    {person.taps} taps and {person.long_holds} long holds, {target.last.get('presses')} presses arrived, "
          f"{target.last.get('moves')} moves arrived, and the longest anything stayed down after release was {person.worst:.0f} ms", flush=True)
    check("no key outlives its release for long", person.worst <= MOST_HELD_MS, f"{person.worst:.0f} ms, key {person.stuck_key}")
    numbers.append((name, person.taps, target.last.get("presses"), person.worst, person.long_seen, person.long_holds))


def bad_network(name, args, seconds=20, expect_loss=None):
    print(f"\n=== {name}: netem {' '.join(args)}", flush=True)
    source, target, _, _ = forwarding_pair(seconds + 8)
    sound = Sound(seconds)
    netem(*args)
    person = Person(source, target)
    typist = threading.Thread(target=person.typing, args=(random.Random(name),), daemon=True)
    typist.start()
    time.sleep(seconds - 6)
    quality = source.last.get("link", {})
    print(f"    link as measured: {quality}", flush=True)
    person.running = False
    typist.join()
    person.stop()
    audio_status = sound.rb.last
    print(f"    audio rx: {audio_status.get('rx_loss')}% lost, {audio_status.get('rx_unrepaired')}% unrepaired", flush=True)
    time.sleep(1)
    judge_typing(person, target, name)
    check("control stayed where it was", source.last.get("active") == 1 and source.last.get("online"), str(source.last.get("active")))
    check("long holds came through", person.long_holds == 0 or person.long_seen >= person.long_holds * 0.8, f"{person.long_seen} of {person.long_holds}")
    if expect_loss:
        low, high = expect_loss
        seen = max(quality.get("loss_in", 0), quality.get("loss_out", 0))
        check("the link measures about the right loss", low <= seen <= high, f"{seen:.1f}%")
    check("the round trip is measured", quality.get("round_trip_ms") is not None, str(quality))
    sound.finish()
    share, longest = sound.quiet_spells()
    print(f"    tone {audio.level(sound.heard, TONE):.1f} dB, sounding {share * 100:.1f}% of the time, longest silence {longest} ms", flush=True)
    check("audio keeps playing", share > 0.9 and longest <= 500, f"{share * 100:.1f}%, {longest} ms")
    source.stop()
    target.stop()
    clean()


def dropout(name, base, main=0, sender=0, seconds=22):
    print(f"\n=== {name}: 2-second dropout on top of {' '.join(base) or 'a clean link'}", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(seconds + 8, main, sender)
    sound = Sound(seconds)
    if base:
        netem(*base)
    person = Person(source, target)
    typist = threading.Thread(target=person.typing, args=(random.Random(name),), daemon=True)
    typist.start()
    time.sleep(4)
    person.running = False
    typist.join()
    person.stop()
    person.running = True

    # A key held through it, as someone leaning on Shift would.
    key = 42
    person.press(key)
    target.wait_for(lambda s: key in s["held"], 2.5)
    held_before = key in target.last.get("held", [])
    check("a held key is down on the other device", held_before)

    netem(*replace_loss(base))
    went = time.time()
    released = target.wait_for(lambda s: key not in s["held"], 2)
    unblocked = source.wait_for(lambda s: s["blocking"] == [False, False], 2)
    check("the receiving device lets go during the stall", released is not None)
    check("the local keyboard is free during the stall", unblocked is not None)
    if released:
        print(f"    let go {1000 * (released - went):.0f} ms into the stall, keyboard free at {1000 * ((unblocked or went) - went):.0f} ms", flush=True)
    check("control is on the local device meanwhile", source.last.get("active") == source_id, str(source.last.get("active")))
    source.do(key=30, pressed=True)
    time.sleep(0.1)
    source.do(key=30, pressed=False)
    check("and nothing is being forwarded", 30 not in source.last.get("forwarding", []))
    remaining = 2.0 - (time.time() - went)
    time.sleep(max(remaining, 0))
    person.release(key)
    netem(*base) if base else clean()
    back = time.time()

    returned = source.wait_for(lambda s: s["online"] and s["active"] == target_id and s["blocking"] == [True, True], RETURN_MS / 1000 + 2)
    took = 1000 * (returned - back) if returned else None
    check("control returns to the device that had it", returned is not None and took <= RETURN_MS, f"{took and round(took)} ms after the link came back")
    print(f"    control back {took and round(took)} ms after the link", flush=True)
    numbers.append((f"{name} return", took))
    target.wait_for(lambda s: s["active"] == target_id, 2)
    check("the two devices agree", target.last.get("active") == target_id)

    # Typing carries on where it left off.
    before = target.last.get("presses", 0)
    person.press(31)
    got = target.wait_for(lambda s: 31 in s["held"], 2)
    person.release(31)
    check("keys reach the other device again", got is not None, str(before))
    judge_stuck_after_dropout(person, target, key)
    sound.finish()
    share, longest = sound.quiet_spells()
    print(f"    tone {audio.level(sound.heard, TONE):.1f} dB, sounding {share * 100:.1f}% of the time, longest silence {longest} ms", flush=True)
    tail = sound.heard[-audio.RATE * 2:]
    check("audio keeps playing after it", audio.level(tail, TONE) > -30, f"{audio.level(tail, TONE):.1f} dB")
    check("and only the dropout is missing", longest <= 3000, f"{longest} ms")
    source.stop()
    target.stop()
    clean()


def replace_loss(base):
    """The same conditions with nothing getting through."""
    if "loss" not in base:
        return [*base, "loss", "100%"]
    at = base.index("loss")
    return [*base[:at], "loss", "100%", *base[at + 2:]]


def judge_stuck_after_dropout(person, target, key):
    gone = target.wait_for(lambda s: not s["held"], 3)
    check("nothing is left down afterwards", gone is not None, str(target.last.get("held")))
    print(f"    the longest anything stayed down after release was {person.worst:.0f} ms", flush=True)
    check("no key outlives its release for long", person.worst <= MOST_HELD_MS, f"{person.worst:.0f} ms")


def one_way():
    print("\n=== oneway: the other device stops hearing this one, and keeps talking", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(24)
    # Only packets to the target's port are dropped, so its heartbeats keep arriving here.
    subprocess.run([TC, "qdisc", "replace", "dev", "lo", "root", "handle", "1:", "prio"], check=True)
    subprocess.run([TC, "qdisc", "add", "dev", "lo", "parent", "1:3", "handle", "30:", "netem", "loss", "100%"], check=True)
    subprocess.run([TC, "filter", "add", "dev", "lo", "parent", "1:", "protocol", "ip", "u32", "match", "ip", "dport", str(PORT_B), "0xffff", "flowid", "1:3"], check=True)
    went = time.time()
    free = source.wait_for(lambda s: s["blocking"] == [False, False], 5)
    check("the keyboard comes back though heartbeats still arrive", free is not None and source.last.get("online"), str(source.last))
    if free:
        print(f"    free after {1000 * (free - went):.0f} ms", flush=True)
    clean()
    back = source.wait_for(lambda s: s["active"] == target_id, 5)
    check("control returns once it can be heard again", back is not None, str(source.last.get("active")))
    source.stop()
    target.stop()


def release_during():
    print("\n=== release: Release all input during a dropout", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(16)
    netem("loss", "100%")
    source.wait_for(lambda s: not s["online"], 3)
    source.do(panic=True)
    time.sleep(0.3)
    clean()
    source.wait_for(lambda s: s["online"], 3)
    time.sleep(2.5)
    check("control stays on the device where it was released", source.last.get("active") == source_id and source.last.get("blocking") == [False, False], str(source.last.get("active")))
    source.stop()
    target.stop()


def restart():
    print("\n=== restart: the sender starts again with low sequence numbers", flush=True)
    clean()
    # Device b drives a. a is main, so control sits on a from the start.
    source, target, _, _ = forwarding_pair(30, main=0, sender=1)
    source.do(key=30, pressed=True)
    target.wait_for(lambda s: 30 in s["held"], 2)
    check("a key is down", 30 in target.last.get("held", []))
    source.p.kill()
    source.p.wait()
    fresh = Device(1, PORT_B, PORT_A, 18, 0, seq=1)
    gone = target.wait_for(lambda s: 30 not in s["held"], 3)
    check("what the old run held is let go", gone is not None, str(target.last.get("held")))
    fresh.wait_for(lambda s: s["active"] == 0 and s["online"], 5)
    check("the new run takes control up again", fresh.last.get("active") == 0, str(fresh.last.get("active")))
    fresh.do(key=31, pressed=True)
    got = target.wait_for(lambda s: 31 in s["held"], 2)
    fresh.do(key=31, pressed=False)
    check("its keys are heard, low numbers and all", got is not None, str(target.last.get("held")))
    target.wait_for(lambda s: 31 not in s["held"], 2)
    check("and its releases", 31 not in target.last.get("held", []))
    fresh.stop()
    target.stop()


def main():
    audio.BIN = sys.argv[1]
    which = sys.argv[2:] or ["loss", "delay", "reorder", "mixed", "dropout", "reverse", "flood", "oneway", "release", "restart"]
    subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
    audio.BASE = tempfile.mkdtemp(prefix="mknet.", dir=os.environ.get("AUDIO_TMP", "/tmp"))
    try:
        audio.start_sound_server("a")
        audio.start_sound_server("b")
        time.sleep(1.5)
        if "loss" in which:
            bad_network("loss", ["loss", "15%"], expect_loss=(8, 24))
        if "delay" in which:
            bad_network("delay", ["delay", "80ms", "30ms", "distribution", "normal"])
        if "reorder" in which:
            bad_network("reorder", ["delay", "40ms", "reorder", "25%", "50%"])
        if "mixed" in which:
            bad_network("mixed", ["delay", "30ms", "15ms", "loss", "8%", "duplicate", "2%", "reorder", "15%", "50%"])
        if "dropout" in which:
            dropout("dropout", [])
        if "reverse" in which:
            dropout("reverse", [], main=0, sender=1)
        if "flood" in which:
            dropout("flood", ["delay", "20ms", "10ms", "loss", "5%"])
        if "oneway" in which:
            one_way()
        if "release" in which:
            release_during()
        if "restart" in which:
            restart()
    finally:
        clean()
        shutil.rmtree(audio.BASE, ignore_errors=True)
    print("\nSummary:", flush=True)
    for line in numbers:
        print("   ", line, flush=True)
    print("ALL PASS" if failures == 0 else f"{failures} FAILED", flush=True)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
