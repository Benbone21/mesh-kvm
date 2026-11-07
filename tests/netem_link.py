#!/usr/bin/env python3
"""Two whole devices on a bad network: `tc netem` adds loss, delay, reordering and dropouts, while a
pretend person types and clicks on one device and sound plays to the other.

It checks what Stage 4 promises. Keys never stick: nothing stays down on the receiving device for
long after the person let go. Taps and clicks all arrive, in order, and Ctrl+C arrives as Ctrl+C.
Audio keeps playing. Control returns to the device that had it after a dropout, unless the person
moved on to the device in front of them, and the local keyboard is never swallowed while the link
is gone.

Input uses `input::live::whole_device`, which is the real router, link, encryption and sockets, with
a recorder in place of the keyboard and mouse, and a test pairing the two devices share. Audio uses
`audio::live::whole_device` beside private sound servers. Nothing here can reach your real desktop
or audio, and all of it ends when this script does:

    unshare -rnpf --kill-child --mount-proc python3 tests/netem_link.py path/to/test-binary [scenes]

The test binary is what `cargo test --release --no-run` prints. The scenes are clean, loss, delay,
reorder, mixed, dropout, reverse, flood, stay, stayreverse, oneway, oneway-start, outage,
outagereverse, flapping, panic, release, restart, restart-receiver, queued, lossy and bursty. numpy must be importable. Set AUDIO_TMP
to keep the sound servers' files somewhere other than /tmp. Keep it under about 70 characters: past
that their sockets don't fit in a socket path, the servers don't start, and every tone comes out silent.
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
CTRL, SHIFT, KEY_C = 29, 42, 46
# Mouse buttons come after the keys in what's sent and injected.
BUTTONS = 256
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
        env = dict(os.environ, MK_ME=str(me), MK_LISTEN=str(listen), MK_PEER=str(peer), MK_SECONDS=str(seconds), MK_MAIN=str(main),
                   MK_PAIRING=audio.paired_files(audio.BASE)[me])
        if seq is not None:
            env["MK_SEQ"] = str(seq)
        self.p = subprocess.Popen([audio.BIN, "--ignored", "--exact", "input::live::whole_device", "--nocapture"], env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.last = {}
        self.watchers = []
        self.injected = []
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.p.stdout:
            i = line.find("INJECT ")
            if i >= 0:
                self.injected.append(json.loads(line[i + 7:]))
                continue
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
    """Types and clicks on `sender`, and keeps what's really down, to judge the other device by.

    `sent` is every key and button in order, with when it went. Keys come round in turn, so that
    a swap or a loss shows. Ctrl and C are kept for shortcuts, which go with no pause at all."""

    KEYS = [k for k in range(16, 50) if k not in (CTRL, KEY_C, SHIFT)]

    def __init__(self, sender, receiver):
        self.sender, self.receiver = sender, receiver
        self.down = set()
        self.up_at = {}
        self.lock = threading.Lock()
        self.worst = 0
        self.stuck_key = None
        self.sent = []
        self.shortcuts = 0
        self.long_holds = 0
        self.long_seen = 0
        self.running = True
        self.start_mark = len(receiver.injected)
        receiver.watchers.append(self.judge)

    def judge(self, status):
        # Status times and ours are on one clock, as both are the same machine.
        now = status["ms"] / 1000
        with self.lock:
            for key in status["held"] + [BUTTONS + b for b in status["buttons"]]:
                if key not in self.down and key in self.up_at:
                    late = (now - self.up_at[key]) * 1000
                    if late > self.worst:
                        self.worst, self.stuck_key = late, key

    def press(self, key):
        with self.lock:
            self.down.add(key)
            self.up_at.pop(key, None)
            self.sent.append((time.time(), key, True))
        self.send(key, True)

    def release(self, key):
        self.send(key, False)
        with self.lock:
            self.down.discard(key)
            self.up_at[key] = time.time()
            self.sent.append((time.time(), key, False))

    def send(self, key, pressed):
        if key >= BUTTONS:
            self.sender.do(button=key - BUTTONS, pressed=pressed)
        else:
            self.sender.do(key=key, pressed=pressed)

    def typing(self, rng):
        """Taps, clicks, shortcuts and a long hold now and then, with the mouse moving all the while."""
        turn = 0
        while self.running:
            turn += 1
            key = self.KEYS[turn % len(self.KEYS)]
            if turn % 40 == 0:
                self.long_hold(key)
            elif turn % 7 == 0:
                self.shortcut()
            elif turn % 5 == 0:
                self.tap(BUTTONS + 1, rng)
            else:
                self.tap(key, rng)
            self.pause(rng.uniform(0.01, 0.04))

    def tap(self, key, rng):
        self.press(key)
        self.pause(rng.uniform(0.01, 0.04))
        self.release(key)

    def shortcut(self):
        for key, pressed in ((CTRL, True), (KEY_C, True), (KEY_C, False), (CTRL, False)):
            (self.press if pressed else self.release)(key)
        self.shortcuts += 1

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
            time.sleep(0.005)

    def stop(self):
        self.running = False
        with self.lock:
            down = list(self.down)
        for key in down:
            self.release(key)

    def delivery(self):
        """How many presses got there, how many came in the wrong place, and the time each took."""
        sent = [(at, key) for at, key, pressed in self.sent if pressed]
        got = [e for e in list(self.receiver.injected)[self.start_mark:] if e["pressed"]]
        a, b = [key for _, key in sent], [e["input"] for e in got]
        # The longest run in common, which is what arrived in the right order.
        table = np.zeros((len(a) + 1, len(b) + 1), dtype=np.int32)
        for i, key in enumerate(a):
            row = table[i]
            same = np.array(b) == key if b else np.zeros(0, dtype=bool)
            table[i + 1, 1:] = np.maximum(np.maximum(row[1:], row[:-1] + same), 0)
            table[i + 1] = np.maximum.accumulate(table[i + 1])
        in_order = int(table[-1, -1])
        times, i, j = [], len(a), len(b)
        while i and j:
            if a[i - 1] == b[j - 1] and table[i, j] == table[i - 1, j - 1] + 1:
                times.append(got[j - 1]["ms"] - sent[i - 1][0] * 1000)
                i, j = i - 1, j - 1
            elif table[i - 1, j] >= table[i, j - 1]:
                i -= 1
            else:
                j -= 1
        return len(a), in_order, len(b) - in_order, times

    def shortcuts_landed(self):
        """Shortcuts that arrived as Ctrl, C down, C up, Ctrl, and C presses with no Ctrl under them."""
        ctrl, good, bad = False, 0, 0
        for e in list(self.receiver.injected)[self.start_mark:]:
            if e["input"] == CTRL:
                ctrl = e["pressed"]
            elif e["input"] == KEY_C and e["pressed"]:
                good, bad = (good + 1, bad) if ctrl else (good, bad + 1)
        return good, bad


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


def forwarding_pair(seconds, main=0, sender=0, ports=(PORT_A, PORT_B)):
    """Two devices, with `sender`'s keyboard driving the other, and control settled there."""
    a = Device(0, ports[0], ports[1], seconds, main)
    b = Device(1, ports[1], ports[0], seconds, main)
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
    total, in_order, misplaced, times = person.delivery()
    good, bad = person.shortcuts_landed()
    waits = target.last.get("waits", {})
    middle = float(np.median(times)) if times else 0.0
    most = max(times) if times else 0.0
    share = 100 * in_order / max(total, 1)
    print(f"    {total} presses, {in_order} delivered in order ({share:.1f}%), {misplaced} out of place, {good} of {person.shortcuts} shortcuts as Ctrl+C, "
          f"{target.last.get('moves')} moves arrived", flush=True)
    print(f"    took {middle:.0f} ms typically, {most:.0f} ms at most. Waited for a gap: {waits}. "
          f"Longest anything stayed down after release: {person.worst:.0f} ms", flush=True)
    check("at least 99% of presses are delivered, in order", share >= 99.0 and total >= 100, f"{in_order} of {total}")
    check("nothing lands out of order", misplaced == 0, str(misplaced))
    check("Ctrl+C arrives as Ctrl+C", good == person.shortcuts and bad == 0 and good > 0, f"{good} of {person.shortcuts}, {bad} bare")
    check("no key outlives its release for long", person.worst <= MOST_HELD_MS, f"{person.worst:.0f} ms, key {person.stuck_key}")
    numbers.append((name, f"{in_order}/{total} in order", f"{good}/{person.shortcuts} shortcuts", f"median {middle:.0f} ms, max {most:.0f} ms",
                    f"gap waits {waits.get('held')} longest {waits.get('longest_ms')} ms skipped {waits.get('skipped')}", f"stuck {person.worst:.0f} ms"))
    return waits


def bad_network(name, args, seconds=20, expect_loss=None):
    print(f"\n=== {name}: netem {' '.join(args) or '(clean)'}", flush=True)
    clean()
    source, target, _, _ = forwarding_pair(seconds + 8)
    sound = Sound(seconds)
    if args:
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
    waits = judge_typing(person, target, name)
    if not args:
        check("a clean link adds no delay", waits.get("held") == 0 and waits.get("skipped") == 0, str(waits))
    check("control stayed where it was", source.last.get("active") == 1 and source.last.get("online"), str(source.last.get("active")))
    check("long holds came through", person.long_holds == 0 or person.long_seen >= person.long_holds * 0.8, f"{person.long_seen} of {person.long_holds}")
    if expect_loss:
        low, high = expect_loss
        seen = max(quality.get("loss_in") or 0, quality.get("loss_out") or 0)
        check("the link measures about the right loss", low <= seen <= high, f"{seen:.1f}%")
    check("the round trip is measured", quality.get("round_trip_ms") is not None, str(quality))
    sound.finish()
    share, longest = sound.quiet_spells()
    print(f"    tone {audio.level(sound.heard, TONE):.1f} dB, sounding {share * 100:.1f}% of the time, longest silence {longest} ms", flush=True)
    check("audio keeps playing", share > 0.9 and longest <= 500, f"{share * 100:.1f}%, {longest} ms")
    source.stop()
    target.stop()
    clean()


def dropout(name, base, main=0, sender=0, seconds=16):
    """Nobody touches anything during the stall, so control has to come back."""
    print(f"\n=== {name}: 2-second dropout on top of {' '.join(base) or 'a clean link'}", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(seconds + 8, main, sender)
    sound = Sound(seconds)
    if base:
        netem(*base)
    person = Person(source, target)
    typist = threading.Thread(target=person.typing, args=(random.Random(name),), daemon=True)
    typist.start()
    time.sleep(3)
    person.running = False
    typist.join()
    person.stop()
    person.running = True

    # Shift held right through it, as someone leaning on it would.
    person.press(SHIFT)
    target.wait_for(lambda s: SHIFT in s["held"], 2.5)
    check("a held key is down on the other device", SHIFT in target.last.get("held", []))

    netem(*replace_loss(base))
    went = time.time()
    released = target.wait_for(lambda s: SHIFT not in s["held"], 2)
    unblocked = source.wait_for(lambda s: s["blocking"] == [False, False], 2)
    check("the receiving device lets go during the stall", released is not None)
    check("the local keyboard is free during the stall", unblocked is not None)
    if released:
        print(f"    let go {1000 * (released - went):.0f} ms into the stall, keyboard free at {1000 * ((unblocked or went) - went):.0f} ms", flush=True)
    check("control is on the local device meanwhile", source.last.get("active") == source_id, str(source.last.get("active")))
    check("nothing counts as forwarded while the key is still down", SHIFT in person.down and SHIFT not in source.last.get("forwarding", []),
          str(source.last.get("forwarding")))
    time.sleep(max(2.0 - (time.time() - went), 0))
    netem(*base) if base else clean()
    back = time.time()

    returned = source.wait_for(lambda s: s["online"] and s["active"] == target_id and s["blocking"] == [True, True], RETURN_MS / 1000 + 2)
    took = 1000 * (returned - back) if returned else None
    check("control returns to the device that had it", returned is not None and took <= RETURN_MS, f"{took and round(took)} ms after the link came back")
    print(f"    control back {took and round(took)} ms after the link", flush=True)
    numbers.append((f"{name} return", f"{took and round(took)} ms"))
    target.wait_for(lambda s: s["active"] == target_id, 2)
    check("the two devices agree", target.last.get("active") == target_id)
    # It went away with the link, and it's still down.
    check("the modifier held through it is held again", target.wait_for(lambda s: SHIFT in s["held"], 2) is not None, str(target.last.get("held")))
    person.release(SHIFT)
    check("and let go with the real one", target.wait_for(lambda s: SHIFT not in s["held"], 2) is not None, str(target.last.get("held")))

    # Typing carries on where it left off.
    person.press(31)
    got = target.wait_for(lambda s: 31 in s["held"], 2)
    person.release(31)
    check("keys reach the other device again", got is not None)
    gone = target.wait_for(lambda s: not s["held"], 3)
    check("nothing is left down afterwards", gone is not None, str(target.last.get("held")))
    print(f"    the longest anything stayed down after release was {person.worst:.0f} ms", flush=True)
    check("no key outlives its release for long", person.worst <= MOST_HELD_MS, f"{person.worst:.0f} ms")
    sound.finish()
    share, longest = sound.quiet_spells()
    print(f"    tone {audio.level(sound.heard, TONE):.1f} dB, sounding {share * 100:.1f}% of the time, longest silence {longest} ms", flush=True)
    tail = sound.heard[-audio.RATE * 2:]
    check("audio keeps playing after it", audio.level(tail, TONE) > -30, f"{audio.level(tail, TONE):.1f} dB")
    check("and only the dropout is missing", longest <= 3000, f"{longest} ms")
    source.stop()
    target.stop()
    clean()


def typed_during(name, main=0, sender=0):
    print(f"\n=== {name}: the person starts using this device during the stall", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(16, main, sender)
    netem("loss", "100%")
    went = time.time()
    source.wait_for(lambda s: s["blocking"] == [False, False], 3)
    check("the local keyboard is free", source.last.get("blocking") == [False, False])
    time.sleep(0.4)
    source.do(key=30, pressed=True)
    source.do(key=30, pressed=False)
    time.sleep(max(2.0 - (time.time() - went), 0))
    clean()
    source.wait_for(lambda s: s["online"], 3)
    time.sleep(3)
    check("control stays with the person using this device", source.last.get("active") == source_id and source.last.get("blocking") == [False, False],
          str({k: source.last.get(k) for k in ("active", "blocking")}))
    check("and nothing was forwarded", not target.last.get("held"), str(target.last.get("held")))
    source.do(switch=True) if main == sender else source.do(cross=True)
    back = source.wait_for(lambda s: s["active"] == target_id and s["blocking"] == [True, True], 3)
    check("a real switch still works", back is not None, str(source.last.get("active")))
    source.stop()
    target.stop()


def block_port(port):
    """Packets to `port` are lost, and everything else gets through."""
    subprocess.run([TC, "qdisc", "replace", "dev", "lo", "root", "handle", "1:", "prio"], check=True)
    subprocess.run([TC, "qdisc", "add", "dev", "lo", "parent", "1:3", "handle", "30:", "netem", "loss", "100%"], check=True)
    subprocess.run([TC, "filter", "add", "dev", "lo", "parent", "1:", "protocol", "ip", "u32", "match", "ip", "dport", str(port), "0xffff", "flowid", "1:3"], check=True)


def one_way():
    print("\n=== oneway: the other device stops hearing this one, and keeps talking", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(24)
    time.sleep(1)
    block_port(PORT_B)
    went = time.time()
    free = source.wait_for(lambda s: s["blocking"] == [False, False], 5)
    check("the keyboard comes back though heartbeats still arrive", free is not None and source.last.get("online"), str(source.last))
    if free:
        print(f"    free after {1000 * (free - went):.0f} ms", flush=True)
    bare = source.wait_for(lambda s: s["link"]["one_way"], 3)
    link = source.last.get("link", {})
    check("the link says it's one way, with no stale figures", bare is not None and link.get("round_trip_ms") is None and link.get("loss_out") is None, str(link))
    clean()
    back = source.wait_for(lambda s: s["active"] == target_id, 5)
    check("control returns once it can be heard again", back is not None, str(source.last.get("active")))
    source.stop()
    target.stop()


def one_way_from_the_start():
    print("\n=== oneway-start: the other device never hears this one, as with a firewall", flush=True)
    clean()
    block_port(PORT_B)
    source = Device(0, PORT_A, PORT_B, 20)
    target = Device(1, PORT_B, PORT_A, 20)
    # No session can form until both directions work, so it's the handshakes that say so.
    heard = source.wait_for(lambda s: s["link"]["one_way"], 5)
    check("hearing handshakes says one way, with the link still down", heard is not None and not source.last.get("online"), str(source.last.get("link")))
    source.do(cross=True)
    time.sleep(0.3)
    crossed = source.last.get("active") == 1
    went = time.time()
    free = source.wait_for(lambda s: s["blocking"] == [False, False] and s["active"] == 0, 5)
    check("a crossing can't keep this keyboard", free is not None, str(source.last.get("active")))
    print(f"    crossed {crossed}, keyboard free {1000 * ((free or went) - went):.0f} ms later", flush=True)
    source.wait_for(lambda s: s["link"]["one_way"], 4)
    link = source.last.get("link", {})
    check("the link says it's one way", link.get("one_way") is True and link.get("round_trip_ms") is None, str(link))
    time.sleep(0.5)
    source.do(cross=True)
    time.sleep(1)
    check("another crossing is refused", source.last.get("active") == 0 and source.last.get("blocking") == [False, False], str(source.last.get("active")))
    clean()
    back = source.wait_for(lambda s: s["online"] and not s["link"]["one_way"], 4)
    source.do(cross=True)
    moved = source.wait_for(lambda s: s["active"] == 1, 3)
    check("once it can hear, crossing works", back is not None and moved is not None, str(source.last.get("link")))
    source.stop()
    target.stop()


def long_outage(name, main=0, sender=0):
    print(f"\n=== {name}: the link is gone for more than 15 seconds", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(34, main, sender)
    netem("loss", "100%")
    time.sleep(16.5)
    clean()
    source.wait_for(lambda s: s["online"], 3)
    target.wait_for(lambda s: s["online"], 3)
    time.sleep(3)
    for device, device_id, who in ((source, source_id, "where the keyboard was"), (target, target_id, "the other device")):
        check(f"{who} stays on its own", device.last.get("active") == device_id and device.last.get("blocking") == [False, False],
              str({k: device.last.get(k) for k in ("active", "blocking", "online")}))
    source.do(cross=True)
    moved = source.wait_for(lambda s: s["active"] == target_id, 3)
    check("and a real switch brings them together", moved is not None, str(source.last.get("active")))
    source.stop()
    target.stop()


def flapping():
    print("\n=== flapping: the link keeps dropping for a moment", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(30)
    person = Person(source, target)

    def blocked():
        return source.last.get("blocking") == [True, True]
    stop = threading.Event()

    def type_while_remote():
        turn = 0
        while not stop.is_set():
            turn += 1
            if blocked():
                key = person.KEYS[turn % len(person.KEYS)]
                person.press(key)
                time.sleep(0.03)
                person.release(key)
            time.sleep(0.03)
    typist = threading.Thread(target=type_while_remote, daemon=True)
    typist.start()
    for _ in range(4):
        netem("loss", "100%")
        time.sleep(1.7)
        clean()
        time.sleep(1.3)
    stop.set()
    typist.join()
    back = source.wait_for(lambda s: s["active"] == target_id and s["blocking"] == [True, True], 3)
    check("control ends up back where it was", back is not None, str(source.last.get("active")))
    gone = target.wait_for(lambda s: not s["held"], 3)
    check("nothing is left down", gone is not None, str(target.last.get("held")))
    print(f"    the longest anything stayed down after release was {person.worst:.0f} ms", flush=True)
    check("no key outlives its release for long", person.worst <= MOST_HELD_MS, f"{person.worst:.0f} ms, key {person.stuck_key}")
    source.stop()
    target.stop()


def panic_on_receiver():
    print("\n=== panic: Release all input on the device being driven, with the news lost", flush=True)
    clean()
    source, target, source_id, target_id = forwarding_pair(16)
    netem("loss", "100%")
    time.sleep(0.2)
    target.do(panic=True)
    time.sleep(0.4)
    clean()
    went = time.time()
    free = source.wait_for(lambda s: s["active"] == source_id and s["blocking"] == [False, False], 3)
    check("the device driving it gets its keyboard back", free is not None, str(source.last.get("active")))
    if free:
        print(f"    free {1000 * (free - went):.0f} ms after the link came back", flush=True)
    time.sleep(2)
    check("and control doesn't come back by itself", source.last.get("active") == source_id, str(source.last.get("active")))
    source.stop()
    target.stop()


def replace_loss(base):
    """The same conditions with nothing getting through."""
    if "loss" not in base:
        return [*base, "loss", "100%"]
    at = base.index("loss")
    return [*base[:at], "loss", "100%", *base[at + 2:]]


class Bounces:
    """Counts each time control leaves the device it should be on, however briefly."""

    def __init__(self, device, wanted):
        self.count, self.was_right, self.wanted = 0, True, wanted
        device.watchers.append(self.watch)

    def watch(self, status):
        right = status["active"] == self.wanted and status["blocking"] == [True, True] and status["online"]
        self.count += self.was_right and not right
        self.was_right = right


def stays_put(name, args, seconds):
    """Both ways of being driven at once: from main, which goes by answers, and to it, by views."""
    print(f"\n=== {name}: netem {' '.join(args)} for {seconds} seconds", flush=True)
    clean()
    from_main = forwarding_pair(seconds + 12)
    to_main = forwarding_pair(seconds + 12, main=0, sender=1, ports=(PORT_A + 2, PORT_B + 2))
    counters = [Bounces(from_main[0], 1), Bounces(to_main[0], 0)]
    netem(*args)
    person = Person(from_main[0], from_main[1])
    typist = threading.Thread(target=person.typing, args=(random.Random(name),), daemon=True)
    typist.start()
    time.sleep(seconds)
    quality = from_main[0].last.get("link", {})
    person.running = False
    typist.join()
    person.stop()
    clean()
    print(f"    link as measured: {quality}", flush=True)
    print(f"    control bounced {counters[0].count} times driving from main and {counters[1].count} times driving it", flush=True)
    check("control doesn't bounce from main", counters[0].count == 0, str(counters[0].count))
    check("control doesn't bounce to main", counters[1].count == 0, str(counters[1].count))
    time.sleep(1)
    judge_typing(person, from_main[1], name)
    numbers.append((f"{name} bounces", counters[0].count, counters[1].count, f"over {seconds} s"))
    for device in (*from_main[:2], *to_main[:2]):
        device.stop()


def tbf(rate):
    subprocess.run([TC, "qdisc", "change", "dev", "lo", "root", "tbf", "rate", rate, "burst", "1600", "limit", "20000000"], check=True)


def queued_stall():
    print("\n=== queued: nothing is lost in the stall, it all arrives at once 2 seconds late", flush=True)
    clean()
    subprocess.run([TC, "qdisc", "add", "dev", "lo", "root", "tbf", "rate", "1gbit", "burst", "1600", "limit", "20000000"], check=True)
    source, target, source_id, target_id = forwarding_pair(20)
    person = Person(source, target)
    rng = random.Random("queued")
    before = target.last.get("presses", 0)
    for key in Person.KEYS[:6]:
        person.tap(key, rng)
        time.sleep(0.05)
    time.sleep(0.6)
    clean_presses = len(person.sent) // 2
    check("typing is getting through first", target.last.get("presses", 0) - before == clean_presses, str(target.last.get("presses")))

    mark = len(target.injected)
    stale_before = target.last.get("waits", {}).get("stale", 0)
    tbf("8bit")
    went = time.time()
    # Typed into the stall: some of these will be well over a second old when they come out.
    typed = []
    while time.time() - went < 1.2:
        key = Person.KEYS[len(typed) % len(Person.KEYS)]
        person.press(key)
        time.sleep(0.02)
        person.release(key)
        typed.append((time.time() - went, key))
        time.sleep(0.08)
    time.sleep(max(2.0 - (time.time() - went), 0))
    tbf("1gbit")
    freed = time.time()
    time.sleep(2.5)
    late = [e for e in target.injected[mark:] if e["pressed"]]
    seen = [e["input"] for e in late]
    stale = target.last.get("waits", {}).get("stale", 0) - stale_before
    # The filter lets a little through as the stall starts. All the rest is over a second old by the
    # time it's out, as it went in during the first 1.2 seconds of two.
    too_old = sum(at < 0.9 for at, _ in typed) - 2
    print(f"    {len(typed)} taps typed into the stall, {len(seen)} presses injected, {stale} dropped as stale", flush=True)
    check("nothing from before the stall is typed again", all(key in [k for _, k in typed] for key in seen), str(seen))
    check("nothing is typed twice", len(seen) == len(set(seen)), str(seen))
    check("a press over a second old is dropped", stale >= too_old and len(seen) + stale == len(typed), f"{stale} stale, {len(seen)} of {len(typed)}")
    gone = target.wait_for(lambda s: not s["held"], 2)
    check("every release got out", gone is not None, str(target.last.get("held")))
    numbers.append(("queued", f"{len(typed)} typed", f"{len(seen)} injected", f"stale {stale}", f"out at {freed - went:.1f} s"))
    source.stop()
    target.stop()
    clean()


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


def restart_receiver():
    print("\n=== restart-receiver: a click restarts the device it lands on", flush=True)
    clean()
    source, target, _, _ = forwarding_pair(16)
    source.do(button=1, pressed=True)
    source.do(button=1, pressed=False)
    end = time.time() + 1
    while time.time() < end and len(target.injected) < 2:
        time.sleep(0.005)
    check("the click landed", any(e["input"] == BUTTONS + 1 and e["pressed"] for e in target.injected), str(target.injected[-3:]))
    target.p.kill()
    target.p.wait()
    fresh = Device(1, PORT_B, PORT_A, 12, 0)
    fresh.wait_for(lambda s: s["online"], 3)
    time.sleep(1.5)
    check("the new run isn't clicked again by what's repeated", not fresh.injected, str(fresh.injected))
    source.do(key=31, pressed=True)
    got = fresh.wait_for(lambda s: 31 in s["held"], 2)
    source.do(key=31, pressed=False)
    check("and new presses reach it", got is not None, str(fresh.last.get("held")))
    fresh.stop()
    source.stop()


def main():
    audio.BIN = sys.argv[1]
    which = sys.argv[2:] or ["clean", "loss", "delay", "reorder", "mixed", "dropout", "reverse", "flood", "stay", "stayreverse", "oneway",
                             "oneway-start", "outage", "outagereverse", "flapping", "panic", "release", "restart", "restart-receiver", "queued", "lossy", "bursty"]
    subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
    audio.BASE = tempfile.mkdtemp(prefix="mknet.", dir=os.environ.get("AUDIO_TMP", "/tmp"))
    try:
        audio.start_sound_server("a")
        audio.start_sound_server("b")
        time.sleep(1.5)
        if "clean" in which:
            bad_network("clean", [])
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
        if "stay" in which:
            typed_during("stay")
        if "stayreverse" in which:
            typed_during("stayreverse", main=0, sender=1)
        if "oneway" in which:
            one_way()
        if "oneway-start" in which:
            one_way_from_the_start()
        if "outage" in which:
            long_outage("outage")
        if "outagereverse" in which:
            long_outage("outagereverse", main=0, sender=1)
        if "flapping" in which:
            flapping()
        if "restart-receiver" in which:
            restart_receiver()
        if "queued" in which:
            queued_stall()
        if "lossy" in which:
            stays_put("lossy", ["loss", "30%"], int(os.environ.get("BOUNCE_SECONDS", 120)))
        if "bursty" in which:
            # A good state with 2% chance of a burst, of 4 packets or so, all lost.
            stays_put("bursty", ["loss", "gemodel", "2%", "25%", "100%", "0%"], int(os.environ.get("BOUNCE_SECONDS", 120)))
        if "panic" in which:
            panic_on_receiver()
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
