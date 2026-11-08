#!/usr/bin/env python3
"""End-to-end link test: the Windows exe under Wine, with this script as the other device.

It sends the exe hand-made packets and reads the ones the exe sends back, so that input, EdgeMap,
PeerDisplayInfo and the layout are all driven in both directions.

Run inside a network namespace, so that the daemon's ports are free:

    unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_link.py [path/to/mesh-kvm.exe]'

The exe path comes from the first argument, then $MESH_KVM_EXE, then the default below. The script
makes a second namespace for the other device, joined to the first by a veth pair, as both sides
serve the same API port.

To also drive the laptop's own keyboard and mouse, set WINE_LINK_DISPLAY to a throwaway X display that
xdotool can type on. Without it, that step is skipped and says so.

To also run the self-update, set MESH_KVM_HOOKS_EXE to a build with the test hooks, as that takes this
script's own key and release server:

    cargo build --release --features test-hooks --target x86_64-pc-windows-gnu

A real install, swap, watcher and restore then run under Wine. Without it, that part is skipped and says
so. That includes an old service that's slow to let go of its ports, a new copy that answers and then
dies for want of one, and another device that does the same. It needs python3-cryptography, and takes
about fifteen minutes.
"""
import contextlib
import glob
import hashlib
import http.client
import http.server
import json
import os
import pathlib
import shutil
import socket
import socketserver
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

DEFAULT_EXE = str(pathlib.Path(__file__).resolve().parent.parent
                  / "target/x86_64-pc-windows-gnu/release/mesh-kvm.exe")
PREFIX = os.environ.get("WINE_LINK_PREFIX", "/tmp/claude-1000/mk/wineprefix")
LAPTOP = "10.77.0.1"
PEER = "10.77.0.2"
STRANGER = "10.77.0.3"
ANOTHER_PEER = "10.77.0.9"
INPUT_PORT = 24850
AUDIO_PORT = 24851
AUDIO_MAGIC = b"MKAU"
AUDIO_VERSION = 2
API_PORT = 24880
RELEASE_EXE = "mesh-kvm-windows-x86_64.exe"
LOCAL = f"http://127.0.0.1:{API_PORT}"
DESKTOP_NAME = "wine-link-desktop"

DEBIAN, WINDOWS = 0, 1
LEFT, RIGHT, TOP, BOTTOM = 0, 1, 2, 3
(MOUSE_MOVE, MOUSE_BUTTON, MOUSE_WHEEL, KEY_EVENT, _FOCUS_SWITCH, _FOCUS_ACK, PANIC_RELEASE, HEARTBEAT,
 PEER_DISPLAY_INFO, EDGE_MAP, FOCUS_SWITCH_AT, GAME_LOCK, ACTIVE_VIEW, LAYOUT_REVISION, LINK_STATE, PRESSES) = range(16)
LINK_PROTOCOL = 2

failures = 0
seq = 0
netns_file = None


@contextlib.contextmanager
def other_device():
    """Runs a block on the other device's side of the cable. Sockets stay there once made."""
    here = os.open("/proc/self/ns/net", os.O_RDONLY)
    os.setns(netns_file, os.CLONE_NEWNET)
    try:
        yield
    finally:
        os.setns(here, os.CLONE_NEWNET)
        os.close(here)


def run(*command):
    subprocess.run(command, check=True)


def make_other_device():
    """Returns the process holding the other device's namespace open."""
    global netns_file
    holder = subprocess.Popen(["unshare", "-n", "sleep", "900"])
    own = os.readlink("/proc/self/ns/net")
    while os.readlink(f"/proc/{holder.pid}/ns/net") == own:
        time.sleep(0.01)
    netns_file = os.open(f"/proc/{holder.pid}/ns/net", os.O_RDONLY)
    run("ip", "link", "add", "veth0", "type", "veth", "peer", "name", "veth1")
    run("ip", "link", "set", "veth1", "netns", str(holder.pid))
    run("ip", "addr", "add", f"{LAPTOP}/24", "dev", "veth0")
    run("ip", "link", "set", "veth0", "up")
    with other_device():
        run("ip", "link", "set", "lo", "up")
        run("ip", "addr", "add", f"{PEER}/24", "dev", "veth1")
        run("ip", "addr", "add", f"{STRANGER}/24", "dev", "veth1")
        run("ip", "link", "set", "veth1", "up")
    return holder


# ---- Packets ----

def packet(payload: bytes) -> bytes:
    """bincode layout: magic, u64 seq, u32 source (Debian), then the payload."""
    global seq
    seq += 1
    return b"MKVM" + struct.pack("<QI", seq, DEBIAN) + payload


def heartbeat():
    return packet(struct.pack("<I", HEARTBEAT))


def active_view(node):
    return packet(struct.pack("<II", ACTIVE_VIEW, node))


def focus_switch_at(node, x, y):
    return packet(struct.pack("<IIii", FOCUS_SWITCH_AT, node, x, y))


def mouse_move(dx, dy):
    return packet(struct.pack("<Iii", MOUSE_MOVE, dx, dy))


def key_event(scancode, pressed):
    return packet(struct.pack("<IHB", KEY_EVENT, scancode, 1 if pressed else 0))


def mouse_button(button, pressed):
    return packet(struct.pack("<IBB", MOUSE_BUTTON, button, 1 if pressed else 0))


def presses(newest, events):
    """`events` are (input, pressed), oldest first, the last of them numbered `newest`."""
    body = struct.pack("<IIQQ", PRESSES, newest, clock_ms(), len(events))
    return packet(body + b"".join(struct.pack("<HBBQ", i, 0, 1 if p else 0, clock_ms()) for i, p in events))


def clock_ms():
    return int(time.monotonic() * 1000)


def link_state(beat, echo=0, age=0, keys=(), buttons=(), protocol=LINK_PROTOCOL, panicked=False, press_order=0, main=True):
    words = [0, 0, 0, 0]
    for key in keys:
        words[key // 64] |= 1 << (key % 64)
    mask = sum(1 << (button - 1) for button in buttons)
    return packet(struct.pack("<IHIIIHBIBQ4QB", LINK_STATE, protocol, beat, echo, age, 0, 1 if panicked else 0, press_order,
                              1 if main else 0, clock_ms(), *words, mask))


def edge_map(edge, start, end, resistance):
    return packet(struct.pack("<IIiii", EDGE_MAP, edge, start, end, resistance))


def game_lock(locked):
    return packet(struct.pack("<IB", GAME_LOCK, 1 if locked else 0))


def layout_revision(revision, editor):
    return packet(struct.pack("<IQI", LAYOUT_REVISION, revision, editor))


def display_info(name, monitors):
    def text(s):
        raw = s.encode()
        return struct.pack("<Q", len(raw)) + raw
    body = struct.pack("<I", PEER_DISPLAY_INFO) + text(name) + struct.pack("<Q", len(monitors))
    for m in monitors:
        body += text(m["name"]) + struct.pack("<iiii", m["x"], m["y"], m["w"], m["h"])
    return packet(body)


class Reader:
    def __init__(self, data):
        self.data, self.at = data, 0

    def take(self, fmt):
        values = struct.unpack_from("<" + fmt, self.data, self.at)
        self.at += struct.calcsize("<" + fmt)
        return values if len(values) > 1 else values[0]

    def text(self):
        length = self.take("Q")
        raw = self.data[self.at:self.at + length]
        self.at += length
        return raw.decode()


def parse(data):
    """A packet from the exe, as (source, kind, fields), or None if it isn't one."""
    if len(data) < 20 or data[:4] != b"MKVM":
        return None
    r = Reader(data)
    r.at = 4
    _, source = r.take("QI")
    kind = r.take("I")
    if kind == PEER_DISPLAY_INFO:
        name = r.text()
        monitors = []
        for _ in range(r.take("Q")):
            monitor_name = r.text()
            x, y, w, h = r.take("iiii")
            monitors.append({"name": monitor_name, "x": x, "y": y, "w": w, "h": h})
        return source, kind, {"name": name, "monitors": monitors}
    if kind == EDGE_MAP:
        edge, start, end, resistance = r.take("Iiii")
        return source, kind, {"edge": edge, "start": start, "end": end, "resistance": resistance}
    if kind == FOCUS_SWITCH_AT:
        target, x, y = r.take("Iii")
        return source, kind, {"target": target, "x": x, "y": y}
    if kind == GAME_LOCK:
        return source, kind, {"locked": r.take("B") != 0}
    if kind == LAYOUT_REVISION:
        revision, editor = r.take("QI")
        return source, kind, {"revision": revision, "editor": editor}
    if kind == MOUSE_MOVE:
        dx, dy = r.take("ii")
        return source, kind, {"dx": dx, "dy": dy}
    if kind == KEY_EVENT:
        scancode, pressed = r.take("HB")
        return source, kind, {"scancode": scancode, "pressed": pressed != 0}
    if kind == LINK_STATE:
        protocol, beat, echo, age, loss, panicked, order, _, _ = r.take("HIIIHBIBQ")
        words = r.take("4Q")
        mask = r.take("B")
        keys = [k for k in range(256) if words[k // 64] >> (k % 64) & 1]
        return source, kind, {"protocol": protocol, "beat": beat, "echo": echo, "age": age, "loss": loss,
                              "panicked": panicked != 0, "order": order, "keys": keys, "buttons": mask}
    if kind == PRESSES:
        newest, _ = r.take("IQ")
        events = [r.take("HBBQ") for _ in range(r.take("Q"))]
        return source, kind, {"newest": newest, "events": [(i, p) for i, _, p, _ in events]}
    return source, kind, {}


class Inbox:
    """Everything the exe sends to the other device's input port."""

    def __init__(self, sock):
        self.sock = sock
        self.seen = []
        self.lock = threading.Lock()
        threading.Thread(target=self.listen, daemon=True).start()

    def listen(self):
        while True:
            data, _ = self.sock.recvfrom(4096)
            parsed = parse(data)
            if parsed:
                with self.lock:
                    self.seen.append(parsed)

    def clear(self):
        with self.lock:
            self.seen.clear()

    def wait_for(self, kind, timeout=3, match=lambda f: True, keep_alive=None):
        """The first packet of `kind` that matches, sent after the last `clear`."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                for source, seen_kind, fields in self.seen:
                    if seen_kind == kind and match(fields):
                        return source, fields
            if keep_alive:
                keep_alive()
            time.sleep(0.05)
        return None


# ---- The exe's API ----

def call(path, body=None, headers=None, base=LOCAL):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(base + path, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(request, timeout=3) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        return json.load(e)


def post_from(source, path, body):
    """A POST to the laptop's API from `source`, on the other device's side. Returns the status."""
    with other_device():
        connection = http.client.HTTPConnection(LAPTOP, API_PORT, timeout=3, source_address=(source, 0))
        connection.connect()
    connection.request("POST", path, json.dumps(body))
    status = connection.getresponse().status
    connection.close()
    return status


def local():
    return call("/api/local")


def state():
    return call("/api/state")


def check(name, ok, detail=""):
    global failures
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail and not ok else ""), flush=True)
    if not ok:
        failures += 1


def wait_for(cond, timeout):
    """Poll /api/local until cond(state) holds; return the last state seen."""
    deadline = time.time() + timeout
    st = None
    while time.time() < deadline:
        try:
            st = local()
            if cond(st):
                return st
        except Exception:
            pass
        time.sleep(0.1)
    return st


def view(st):
    return (st or {}).get("peer_online"), (st or {}).get("active_node")


class Peer:
    def __init__(self, sock, stranger):
        self.sock, self.stranger = sock, stranger

    def send(self, data):
        self.sock.sendto(data, (LAPTOP, INPUT_PORT))

    def send_as_stranger(self, data):
        self.stranger.sendto(data, (LAPTOP, INPUT_PORT))

    def pump(self, seconds, *builders, sender=None):
        """Send the given packets every 200 ms for a while."""
        end = time.time() + seconds
        while time.time() < end:
            for b in builders:
                (sender or self.send)(b())
            time.sleep(0.2)

    def drive_to_debian(self, timeout=3):
        """Send heartbeats and Debian views until active_node is Debian; return the last state
        seen. A slow or failed /api/local read counts as no state, so the check fails instead of
        raising."""
        stop = time.time() + timeout
        st = None
        while time.time() < stop:
            self.send(heartbeat())
            self.send(active_view(DEBIAN))
            try:
                st = local()
            except Exception:
                st = None
            else:
                if st.get("active_node") == "Debian":
                    break
            time.sleep(0.2)
        return st

    def give_focus_to_laptop(self, x, y):
        """Hands focus to the laptop as a crossing from Debian would, and keeps the link up."""
        self.send(heartbeat())
        self.send(focus_switch_at(WINDOWS, x, y))
        self.send(active_view(WINDOWS))
        return wait_for(lambda s: s["active_node"] == "Windows", 2)


class Exe:
    def __init__(self, exe, env, log):
        self.exe, self.env, self.log = exe, env, log
        self.proc = None

    def start(self, *args):
        # On an X display, a Wine desktop gives the keyboard somewhere to land.
        desktop = ["explorer", f"/desktop={DESKTOP_NAME},800x600"] if "DISPLAY" in self.env else []
        self.proc = subprocess.Popen(["wine", *desktop, self.exe, "--role", "windows", *args],
                                     env=self.env, stdout=self.log, stderr=subprocess.STDOUT)

    def stop(self):
        """Ends the exe and anything it restarted into."""
        if self.proc:
            self.proc.kill()
            with contextlib.suppress(Exception):
                self.proc.wait(timeout=5)
        subprocess.run(["wineserver", "-k"], env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["wineserver", "-w"], env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def settings_file():
    found = glob.glob(f"{PREFIX}/drive_c/users/*/AppData/Local/mesh-kvm/local-settings.json")
    return found[0] if found else None


def serve_layout(holder):
    """The other device's API: `holder["state"]` is what it says at /api/state,
    `holder["peer_version"]` is its version, and `holder["asked_to_install"]` what it was asked."""
    class Handler(http.server.BaseHTTPRequestHandler):
        def reply(self, body):
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if holder.get("peer_gone"):
                # Gone mid-request, as a service that dies does.
                self.close_connection = True
                return
            if self.path == "/api/local" and "peer_version" in holder:
                update = {"state": "idle", "proving": holder.get("peer_proving", False)}
                self.reply(json.dumps({"version": holder["peer_version"], "device_name": "Fake desk", "update": update}).encode())
            else:
                self.reply(json.dumps(holder["state"]).encode() if self.path == "/api/state" else b"{}")

        def do_POST(self):
            if holder.get("peer_gone"):
                self.close_connection = True
                return
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
            holder["asked_to_install"] = body
            self.reply(b'{"ok":true}')

        def log_message(self, *args):
            pass

    class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
        daemon_threads = True
        allow_reuse_address = True

    with other_device():
        server = Server((PEER, API_PORT), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()


class ReleaseServer:
    """Pretends to be GitHub, with this script's key, on a local port."""

    def __init__(self):
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
        self.key = Ed25519PrivateKey.generate()
        raw = self.key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
        self.public = raw.hex()
        self.tag = None
        self.files = {}
        release = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path.endswith("/releases/latest"):
                    body = json.dumps({"tag_name": release.tag}).encode() if release.tag else None
                else:
                    body = release.files.get(self.path.rsplit("/", 1)[-1])
                self.send_response(200 if body is not None else 404)
                self.send_header("Content-Length", str(len(body or b"")))
                self.end_headers()
                self.wfile.write(body or b"")

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.address = f"http://127.0.0.1:{self.server.server_address[1]}"

    def publish(self, version, exe, signed_exe=None):
        """A signed release. `signed_exe` is what the signature vouches for, if not `exe`."""
        vouched = signed_exe or exe
        files = {RELEASE_EXE: {"size": len(vouched), "sha256": hashlib.sha256(vouched).hexdigest()}}
        manifest = json.dumps({"version": version, "files": files}).encode()
        self.files = {"release.json": manifest, "release.json.sig": self.key.sign(manifest), RELEASE_EXE: exe}
        self.tag = f"v{version}"


def link_checks(peer):
    # a. Packets from an unknown address are ignored.
    peer.pump(1.0, heartbeat, lambda: active_view(DEBIAN), sender=peer.send_as_stranger)
    online, active = view(wait_for(lambda s: True, 2))
    check("packets from a stranger leave peer_online false, active Windows",
          online is False and active == "Windows", f"{online}, {active}")

    # b. Heartbeats alone bring the link up, but the laptop stays active.
    peer.send(heartbeat())
    st = wait_for(lambda s: s["peer_online"], 3)
    online, active = view(st)
    check("heartbeats from the peer make peer_online true, active Windows",
          online is True and active == "Windows", f"{online}, {active}")

    # c. An ActiveView naming Debian hands the view over.
    st = peer.drive_to_debian()
    check("ActiveView Debian gives active_node Debian", view(st) == (True, "Debian"), str(view(st)))

    # d. Heartbeats without views: one-way link fallback after 3 s.
    peer.pump(3.0, heartbeat)
    online, active = view(wait_for(lambda s: True, 2))
    check("3 s of heartbeats with no views: online, back to Windows",
          online is True and active == "Windows", f"{online}, {active}")

    # e. A view again, then silence drops the link.
    st = peer.drive_to_debian()
    check("ActiveView Debian again gives Debian", view(st) == (True, "Debian"), str(view(st)))
    time.sleep(2.0)
    st = wait_for(lambda s: not s["peer_online"], 1.5)
    online, active = view(st)
    check("2 s of silence: peer_online false, active Windows",
          online is False and active == "Windows", f"{online}, {active}")

    # f. A focus switch to Windows warps the cursor.
    peer.send(heartbeat())
    peer.send(focus_switch_at(WINDOWS, 100, 200))
    st = wait_for(lambda s: s["cursor"] == [100, 200], 2)
    cur = st["cursor"] if st else None
    check("FocusSwitchAt Windows 100,200 puts the cursor at 100,200", cur == [100, 200], f"cursor {cur}")


def heartbeat_checks(peer, inbox):
    # g. The exe sends everything Debian sends: the same packets, the other way.
    peer.send(heartbeat())
    inbox.clear()
    got = {kind: inbox.wait_for(kind, 2, keep_alive=lambda: peer.send(heartbeat()))
           for kind in (HEARTBEAT, PEER_DISPLAY_INFO, EDGE_MAP, GAME_LOCK, LAYOUT_REVISION)}
    check("the exe sends Heartbeat, PeerDisplayInfo, EdgeMap, GameLock and LayoutRevision",
          all(got.values()), str({k: bool(v) for k, v in got.items()}))
    check("they come from Windows", all(v and v[0] == WINDOWS for v in got.values()))

    info = got[PEER_DISPLAY_INFO]
    monitors = info[1]["monitors"] if info else []
    check("PeerDisplayInfo names at least one monitor with a size",
          bool(monitors) and all(m["w"] > 0 and m["h"] > 0 and m["name"] for m in monitors), str(monitors))
    edge = got[EDGE_MAP]
    # A fresh layout has the laptop to Debian's right, so Debian's edge is its right one.
    check("EdgeMap tells Debian which of its edges touches the laptop",
          bool(edge) and edge[1]["edge"] == RIGHT and edge[1]["start"] <= edge[1]["end"], str(edge))


def reported_monitors_check(peer):
    # h. Debian's report reaches the exe's copy, negative coordinates included.
    monitors = [{"name": "DP-3", "x": -1920, "y": 360, "w": 1920, "h": 1080},
                {"name": "DP-2", "x": 0, "y": 0, "w": 2560, "h": 1440}]
    deadline = time.time() + 3
    seen = None
    while time.time() < deadline:
        peer.send(heartbeat())
        peer.send(display_info("Desk", monitors))
        devices = {d["node"]: d for d in state()["config"]["devices"]}
        seen = devices["Debian"]
        if seen["monitors"] == monitors:
            break
        time.sleep(0.2)
    check("PeerDisplayInfo from Debian lands in the exe's layout", seen["monitors"] == monitors, str(seen))
    check("Debian's name came with it", seen["name"] == "Desk", str(seen))
    return devices


def input_checks(peer, inbox):
    # i. The other device's mouse moves this cursor.
    st = peer.give_focus_to_laptop(400, 300)
    check("focus handed to the laptop", view(st)[1] == "Windows", str(view(st)))
    time.sleep(0.3)
    before = local()["cursor"]
    peer.send(heartbeat())
    peer.send(mouse_move(40, 25))
    st = wait_for(lambda s: s["cursor"] != before, 2)
    after = st["cursor"]
    check("a MouseMoveDelta from Debian moves the laptop's cursor", after != before, f"{before} -> {after}")

    # j. With no EdgeMap yet, the laptop goes by its own layout, which has Debian on its left.
    peer.give_focus_to_laptop(28, 300)
    time.sleep(0.6)
    inbox.clear()
    peer.pump(0.8, heartbeat, lambda: active_view(WINDOWS), lambda: mouse_move(-400, 0))
    own = inbox.wait_for(FOCUS_SWITCH_AT, 1, lambda f: f["target"] == DEBIAN)
    check("with no EdgeMap from Debian, the laptop crosses at the edge in its own layout", own is not None)

    # An EdgeMap then wins over the layout: this one leaves nothing on the left to cross to.
    peer.send(edge_map(RIGHT, 0, 5000, 30))
    peer.give_focus_to_laptop(28, 300)
    time.sleep(0.4)
    inbox.clear()
    peer.pump(0.6, heartbeat, lambda: edge_map(RIGHT, 0, 5000, 30), lambda: active_view(WINDOWS),
              lambda: mouse_move(-400, 0))
    check("an EdgeMap from Debian replaces the laptop's own idea of the edge",
          not inbox.wait_for(FOCUS_SWITCH_AT, 0.3))

    # k. The manual game lock Debian reports holds the cursor here.
    span = edge_map(LEFT, 0, 5000, 30)
    peer.send(span)
    peer.send(game_lock(True))
    peer.give_focus_to_laptop(28, 300)
    time.sleep(0.4)
    inbox.clear()
    peer.pump(0.6, heartbeat, lambda: game_lock(True), lambda: active_view(WINDOWS), lambda: mouse_move(-400, 0))
    check("Debian's game lock keeps the cursor on the laptop", not inbox.wait_for(FOCUS_SWITCH_AT, 0.3))

    # l. An EdgeMap from Debian opens the edge, and the crossing comes back as a focus packet in
    # Debian's own coordinates: the laptop's left side meets Debian's rightmost monitor.
    peer.send(game_lock(False))
    peer.give_focus_to_laptop(28, 300)
    time.sleep(0.4)
    inbox.clear()
    peer.pump(0.8, heartbeat, lambda: game_lock(False), lambda: edge_map(LEFT, 0, 5000, 30),
              lambda: active_view(WINDOWS), lambda: mouse_move(-400, 0))
    crossing = inbox.wait_for(FOCUS_SWITCH_AT, 1, lambda f: f["target"] == DEBIAN)
    check("pushing at the edge in Debian's EdgeMap sends focus to Debian", crossing is not None)
    return crossing


class Beats:
    """Plays a peer on this version: numbered heartbeats, each answering the exe's last one."""

    def __init__(self, peer, inbox):
        self.peer, self.inbox, self.beat = peer, inbox, 100
        self.order = 0

    def send(self, keys=(), buttons=()):
        self.beat += 1
        states = [fields for _, kind, fields in list(self.inbox.seen) if kind == LINK_STATE]
        echo = states[-1]["beat"] if states else 0
        self.peer.send(heartbeat())
        self.peer.send(link_state(self.beat, echo, 0, keys, buttons, press_order=self.order))

    def deaf(self, seconds):
        """Reports that have heard nothing, as from a device behind a firewall."""
        end = time.time() + seconds
        while time.time() < end:
            self.beat += 1
            self.peer.send(heartbeat())
            self.peer.send(link_state(self.beat))
            time.sleep(0.2)

    def pump(self, seconds, **held):
        end = time.time() + seconds
        while time.time() < end:
            self.send(**held)
            time.sleep(0.2)


def held_here():
    return local()["link"]["held_here"]


def link_state_checks(peer, inbox):
    # The peer so far is a device from before LinkState: heartbeats and nothing more.
    peer.pump(3.5, heartbeat, lambda: active_view(WINDOWS))
    check("a peer that sends no LinkState is taken for an older version", local()["link"]["peer_version"] == "older",
          str(local()["link"]))

    inbox.clear()
    first = inbox.wait_for(LINK_STATE, 2, keep_alive=lambda: peer.send(heartbeat()))
    check("the exe sends LinkState with every heartbeat, on this protocol, holding nothing",
          bool(first) and first[1]["protocol"] == LINK_PROTOCOL and first[1]["keys"] == [] and first[1]["buttons"] == 0, str(first))
    beats = Beats(peer, inbox)
    beats.send()
    sent = beats.beat
    inbox.clear()
    answer = inbox.wait_for(LINK_STATE, 2, lambda f: f["echo"] >= sent, keep_alive=beats.send)
    check("it answers the peer's heartbeat", answer is not None, str(answer))
    beats.pump(4)
    link = local()["link"]
    check("a peer on this version is known", link["peer_version"] == "same", str(link))
    check("loss and the round trip are measured", link["measured"] and link["round_trip_ms"] is not None and link["loss_in"] < 5, str(link))

    # Focus on the laptop: what the peer's heartbeat says corrects what got lost.
    st = peer.give_focus_to_laptop(400, 300)
    check("focus is on the laptop for this", view(st)[1] == "Windows", str(view(st)))
    peer.send(key_event(30, True))
    peer.send(mouse_button(1, True))
    check("a key and a button are down", wait_for(lambda s: s["link"]["held_here"] == 2, 2) is not None, str(held_here()))
    beats.send(keys=[30], buttons=[1])
    time.sleep(0.2)
    check("a report that agrees changes nothing", held_here() == 2, str(held_here()))
    beats.send()
    check("what the peer no longer holds is let go", wait_for(lambda s: s["link"]["held_here"] == 0, 2) is not None, str(held_here()))

    beats.send(keys=[31])
    time.sleep(0.2)
    check("a missed press isn't pressed on one report", held_here() == 0, str(held_here()))
    beats.send(keys=[31])
    check("but is on the second", wait_for(lambda s: s["link"]["held_here"] == 1, 2) is not None, str(held_here()))
    beats.send()
    wait_for(lambda s: s["link"]["held_here"] == 0, 2)

    # Numbered presses, as a peer on this version sends them.
    for _ in range(3):
        peer.send(presses(1, [(35, True)]))
    check("a numbered press lands, with its copies counted once",
          wait_for(lambda s: s["link"]["held_here"] == 1, 2) is not None, str(held_here()))
    peer.send(presses(3, [(35, True), (35, False), (36, True)]))
    time.sleep(0.2)
    st = local()["link"]
    check("one packet repairs what was lost ahead of it", st["held_here"] == 1, str(st))
    peer.send(presses(5, [(36, False)]))
    check("a gap that stays gives way after a moment", wait_for(lambda s: s["link"]["held_here"] == 0, 1) is not None, str(held_here()))
    beats.order = 5
    beats.send()
    time.sleep(0.2)
    check("a report that agrees leaves it at that", held_here() == 0, str(held_here()))
    peer.send(presses(6, [(37, True)]))
    wait_for(lambda s: s["link"]["held_here"] == 1, 2)
    peer.send(link_state(beats.beat + 1, press_order=5))
    time.sleep(0.2)
    check("a report from before a numbered press doesn't undo it", held_here() == 1, str(held_here()))
    peer.send(presses(7, [(37, True), (37, False)]))
    wait_for(lambda s: s["link"]["held_here"] == 0, 2)
    beats.order = 7

    # Late packets: built first, sent after something newer.
    global seq
    late = key_event(32, True)
    peer.send(key_event(33, False))
    peer.send(late)
    time.sleep(0.3)
    check("a press that arrives after newer input is dropped", held_here() == 0, str(held_here()))
    # Newer than everything, and then a report from before it.
    report = link_state(beats.beat, keys=[])
    peer.send(key_event(32, True))
    peer.send(report)
    time.sleep(0.3)
    check("a report older than a press doesn't undo it", held_here() == 1, str(held_here()))
    peer.send(key_event(32, False))
    wait_for(lambda s: s["link"]["held_here"] == 0, 2)

    # The peer starts again from low numbers, well behind what it sent before.
    seq += 1 << 20
    peer.send(heartbeat())
    peer.send(key_event(34, False))
    seq = 1
    peer.send(key_event(34, True))
    check("a peer that restarts from a low number is heard straight away",
          wait_for(lambda s: s["link"]["held_here"] == 1, 2) is not None, str(held_here()))
    peer.send(key_event(34, False))
    wait_for(lambda s: s["link"]["held_here"] == 0, 2)
    beats.pump(0.5)

    # Packets from a version this one can't read.
    peer.send(b"MKVM" + struct.pack("<QII", 1 << 30, DEBIAN, 99))
    time.sleep(0.2)
    check("a packet that can't be read means another version", local()["link"]["peer_version"] == "other", str(local()["link"]))


def dropout_checks(peer, inbox):
    beats = Beats(peer, inbox)
    # Control is with Debian, so the laptop's keyboard and mouse go there.
    st = peer.drive_to_debian()
    beats.pump(0.5)
    check("focus is with Debian for this", view(st) == (True, "Debian"), str(view(st)))
    st = wait_for(lambda s: not s["peer_online"], 3)
    check("with the link gone the laptop has its own input, and says control is going back",
          view(st) == (False, "Windows") and st["link"]["returning"] is True, str(st and st["link"]))
    check("and it shows no figures", st["link"]["measured"] is False and st["link"]["round_trip_ms"] is None, str(st["link"]))
    end = time.time() + 2
    while time.time() < end and view(local())[1] != "Debian":
        beats.send()
        peer.send(active_view(DEBIAN))
        time.sleep(0.2)
    st = local()
    check("control goes back to Debian with main's view", view(st) == (True, "Debian") and not st["link"]["returning"], str(st["link"]))

    # With the laptop as main, the return is up to it. That's the case with its keyboard at stake.
    reply = call("/api/local/action", {"action": "set_main_device", "device": WINDOWS})
    beats.pump(0.6)
    check("the laptop can be made main", local()["main_device"] == WINDOWS, str(reply))
    st = wait_for(lambda s: True, 1)
    if view(st)[1] != "Debian":
        call("/api/action", {"action": "toggle_focus"})
    beats.pump(0.6)
    check("with control on Debian", view(local()) == (True, "Debian"), str(view(local())))
    st = wait_for(lambda s: not s["peer_online"], 3)
    check("the dropout gives the laptop its input back", view(st) == (False, "Windows") and st["link"]["returning"], str(view(st)))
    peer.send(heartbeat())
    st = wait_for(lambda s: s["peer_online"], 2)
    time.sleep(0.5)
    peer.send(heartbeat())
    check("with the link back and nothing answered it stays local", view(local()) == (True, "Windows"), str(view(local())))
    inbox.clear()
    deadline = time.time() + 3
    while time.time() < deadline and view(local())[1] != "Debian":
        beats.send()
        time.sleep(0.2)
    check("an answer sends control back to Debian", view(local()) == (True, "Debian"), str(view(local())))

    # Reports with no answers in them: the peer has stopped hearing the laptop.
    beats.deaf(3.2)
    st = local()
    check("a peer that can't hear can't keep the laptop's keyboard", view(st) == (True, "Windows"), str(view(st)))
    check("and the link says it's one way, with no stale figures",
          st["link"]["one_way"] is True and st["link"]["round_trip_ms"] is None and st["link"]["loss_out"] is None, str(st["link"]))
    deadline = time.time() + 3
    while time.time() < deadline and view(local())[1] != "Debian":
        beats.send()
        time.sleep(0.2)
    check("and it goes back once it hears", view(local()) == (True, "Debian"), str(view(local())))
    call("/api/action", {"action": "toggle_focus"})
    beats.pump(0.5)
    call("/api/local/action", {"action": "set_main_device", "device": DEBIAN})
    beats.pump(0.6)
    check("back to Debian as main", local()["main_device"] == DEBIAN, str(local()["main_device"]))


def api_checks(peer, inbox):
    # m. Both layout-wide actions work from the laptop, and reach Debian.
    peer.send(heartbeat())
    peer.drive_to_debian()
    inbox.clear()
    call("/api/action", {"action": "toggle_focus"})
    flip = inbox.wait_for(FOCUS_SWITCH_AT, 2, keep_alive=lambda: peer.send(heartbeat()))
    check("/api/action toggle_focus on the laptop sends a focus switch to Debian", flip is not None)
    inbox.clear()
    call("/api/action", {"action": "panic"})
    release = inbox.wait_for(PANIC_RELEASE, 2, keep_alive=lambda: peer.send(heartbeat()))
    check("/api/action panic on the laptop reaches Debian", release is not None)
    # Sets the release aside, as a manual switch is what ends it.
    wait_for(lambda s: s["panic_mode"], 2)
    peer.send(heartbeat())
    call("/api/action", {"action": "toggle_focus"})


def audio_checks():
    # s. The audio bus: quiet about other versions, live both ways, and a fallback that works.
    with other_device():
        ours = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        ours.bind((PEER, AUDIO_PORT))
        ours.settimeout(1)
    audio = local()["audio"]
    check("audio reports its device lists", isinstance(audio["inputs"], list) and isinstance(audio["outputs"], list),
          str(audio))
    check("nothing is chosen, so nothing is sent", audio["tx_enabled"] is False and audio["input"] == "")

    old = b"MKVA" + struct.pack("<Q", 2) + b"\x55" * 960
    for _ in range(5):
        ours.sendto(old, (LAPTOP, AUDIO_PORT))
    seen = wait_for(lambda s: s["audio"]["rx_other_version"], 5)
    check("the old packet format is noticed", seen is not None)
    check("and none of it is played", local()["audio"]["rx_peak"] == 0)
    garbage = b"MKAU\x09\x01" + b"\xff" * 300
    ours.sendto(garbage, (LAPTOP, AUDIO_PORT))
    check("a later version is left alone too", local()["audio"]["rx_packets"] == 0)

    chosen = {"action": "set_audio_device", "role": "input", "id": "default"}
    post_from(PEER, "/api/local/action", chosen)
    check("the other device can't switch on this one's microphone", local()["audio"]["input"] == "")
    reply = call("/api/local/action", {"action": "set_audio_device", "role": "output", "id": "default"})
    check("choosing an output is accepted", reply.get("ok") is True, str(reply))
    audio = wait_for(lambda s: s["audio"]["tx_enabled"], 5)
    check("choosing anything starts sending", audio is not None)
    packets = []
    deadline = time.time() + 4
    while time.time() < deadline and not packets:
        try:
            data, sender = ours.recvfrom(2048)
        except socket.timeout:
            continue
        if data[:4] == AUDIO_MAGIC:
            packets.append((data, sender))
    check("the sender says it's alive even with nothing playing", bool(packets))
    if packets:
        data, sender = packets[0]
        check("in the new format", data[4] == AUDIO_VERSION, str(data[:8]))
        # Tell it the sound is reaching a speaker here, and keep a stream coming the other way.
        report = AUDIO_MAGIC + bytes([AUDIO_VERSION, 4]) + data[6:8] + bytes(4) + struct.pack("<IBBHH", 5, 3, 1, 20, 0)
        quiet = AUDIO_MAGIC + bytes([AUDIO_VERSION, 3]) + struct.pack("<HI", 77, 0)
        deadline = time.time() + 4
        answered = False
        ours.settimeout(0.1)
        while time.time() < deadline:
            ours.sendto(report, sender)
            ours.sendto(quiet, (LAPTOP, AUDIO_PORT))
            try:
                while True:
                    back, _ = ours.recvfrom(2048)
                    answered = answered or back[:6] == AUDIO_MAGIC + bytes([AUDIO_VERSION, 4])
            except socket.timeout:
                pass
        audio = local()["audio"]
        check("an open output on the other device counts as connected", audio["tx_connected"] is True, str(audio))
        check("but not as playing, with nothing to hear", audio["tx_reaching"] is False, str(audio))
        check("and it reports back to the other device", answered)
        audio = local()["audio"]
        # Both directions on one output is what calls for process loopback, which Wine doesn't have.
        check("with process loopback refused, it falls back and says an echo is possible",
              audio["echo_safe"] is False, str(audio["echo_safe"]))
        check("and it keeps on sending", local()["audio"]["tx_enabled"] is True)
        check("and says why nothing is being muted", any("isn't being muted" in n for n in audio["notes"]), str(audio["notes"]))
    call("/api/local/action", {"action": "set_audio_device", "role": "output", "id": ""})
    check("choosing None stops sending", wait_for(lambda s: not s["audio"]["tx_enabled"], 5) is not None)
    ours.close()


def layout_checks(peer, inbox, holder):
    # n. An edit on the laptop raises the revision, and Debian hears of it.
    config = state()["config"]
    before = config["revision"]
    places = [{"node": d["node"], "x": d["x"], "y": 123 if d["node"] == "Windows" else d["y"]} for d in config["devices"]]
    reply = call("/api/layout", {"arrangement": {"devices": places, "attach_side": config["attach_side"]}})
    now = state()["config"]
    check("POST /api/layout on the laptop is taken, as a new revision by the laptop",
          reply.get("ok") and now["revision"] == before + 1 and now["editor"] == "Windows", str(reply))
    inbox.clear()
    told = inbox.wait_for(LAYOUT_REVISION, 2, lambda f: f["revision"] == before + 1,
                          keep_alive=lambda: peer.send(heartbeat()))
    check("the laptop's heartbeat carries the new revision", told is not None and told[1]["editor"] == WINDOWS, str(told))

    # o. A newer copy on Debian is fetched from its API.
    theirs = json.loads(json.dumps(now))
    theirs["revision"] = now["revision"] + 5
    theirs["editor"] = "Debian"
    theirs["attach_side"] = "Top"
    theirs["arrangement_stamp"] = [theirs["revision"], "Debian"]
    next(d for d in theirs["devices"] if d["node"] == "Windows")["y"] = -456
    holder["state"] = {"config": theirs, "telemetry": {}}
    deadline = time.time() + 4
    while time.time() < deadline and state()["config"]["revision"] != theirs["revision"]:
        peer.send(heartbeat())
        peer.send(layout_revision(theirs["revision"], DEBIAN))
        time.sleep(0.2)
    got = state()["config"]
    laptop = next(d for d in got["devices"] if d["node"] == "Windows")
    check("a newer revision from Debian replaces the laptop's copy",
          (got["revision"], got["attach_side"], laptop["y"]) == (theirs["revision"], "Top", -456),
          str((got["revision"], got["attach_side"], laptop["y"])))

    # p. An older one is left alone.
    peer.send(layout_revision(1, DEBIAN))
    time.sleep(1.2)
    check("an older revision from Debian changes nothing", state()["config"]["revision"] == theirs["revision"])


def peer_change_refused(body, headers=None, base=LOCAL):
    return call("/api/local/action", {"action": "set_peer", **body}, headers, base)


def display_checks():
    # An exe with no X server has no monitors, so the list is empty, but the calls are all there.
    listed = call("/api/displays")
    check("/api/displays answers with a list", isinstance(listed.get("displays"), list), str(listed))
    check("/api/local says no screen change is waiting", local()["display_change"]["change"] is None)
    reply = call("/api/local/action", {"action": "set_display_mode", "output": "Display 9", "width": 1, "height": 1, "millihertz": 1})
    check("a mode the monitor doesn't list is refused", reply.get("ok") is False, str(reply))
    reply = call("/api/local/action", {"action": "set_device_name", "name": "Wine box"})
    check("a device can be renamed", reply.get("ok") and local()["device_name"] == "Wine box", str(reply))
    named = next(d for d in state()["config"]["devices"] if d["node"] == "Windows")
    check("the new name reaches the layout at once", named["name"] == "Wine box", named["name"])
    reply = call("/api/local/action", {"action": "set_device_name", "name": "x" * 33})
    check("a name that is too long is refused", reply.get("ok") is False, str(reply))
    call("/api/local/action", {"action": "set_device_name", "name": ""})


def security_checks(pinned_peer):
    # q. Only the app on this device can pair it with something else.
    with other_device():
        reply = peer_change_refused({"peer": STRANGER}, base=f"http://{LAPTOP}:{API_PORT}")
    check("set_peer from the network is refused", reply.get("ok") is False, str(reply))
    reply = peer_change_refused({"peer": STRANGER}, {"Origin": "http://evil.example"})
    check("set_peer from a web page on this device is refused", reply.get("ok") is False, str(reply))

    # r. Changes come from this device or the other one, and from no one else.
    before = state()["config"]
    hostile = {"revision": 2 ** 64 - 1, "edge_resistance": before["edge_resistance"] + 5}
    for path, body in (("/api/layout", hostile), ("/api/action", {"action": "panic"}),
                       ("/api/local/action", {"action": "set_sharing", "mouse": False}),
                       ("/api/local/action", {"action": "set_display_mode", "output": "Display 1"}),
                       ("/api/local/action", {"action": "set_device_name", "name": "Stranger"}),
                       ("/api/peer/api/action", {"action": "panic"})):
        status = post_from(STRANGER, path, body)
        check(f"POST {path} from a stranger is refused", status == 403, str(status))
    check("the stranger changed nothing", state()["config"] == before and local()["sharing"]["mouse"] is True)
    check("the other device can still change the layout", post_from(PEER, "/api/layout", hostile) == 200)
    now = state()["config"]
    check("a huge revision in an edit is ignored", now["revision"] == before["revision"] + 1, str(now["revision"]))
    check("the other device can't send a change back through the proxy",
          post_from(PEER, "/api/peer/api/action", {"action": "panic"}) == 403)
    reply = peer_change_refused({"peer": ANOTHER_PEER})
    check("set_peer is refused while --peer pins the address",
          pinned_peer and reply.get("ok") is False and "--peer" in reply.get("error", ""), str(reply))
    check("none of that changed the peer", local()["peer_ip"] == PEER, local()["peer_ip"])


def saved_peer_checks(exe):
    # s. Started with --peer, the address was saved, so it now starts without it.
    path = settings_file()
    saved = json.load(open(path)) if path else {}
    check("--peer was saved", saved.get("peer_ip") == PEER, str(saved.get("peer_ip")))
    exe.stop()
    exe.start()
    st = wait_for(lambda s: True, 60)
    check("with no flags, the saved peer is used and can be changed",
          bool(st) and st["paired"] and st["peer_ip"] == PEER and not st["peer_pinned"], str(st))

    # t. Bad addresses are refused with a reason, and a good one restarts the service on it.
    for bad in ("laptop", "127.0.0.1", "0.0.0.0", LAPTOP):
        reply = peer_change_refused({"peer": bad})
        check(f"set_peer {bad} is refused", reply.get("ok") is False and reply.get("error"), str(reply))
    reply = peer_change_refused({"peer": ANOTHER_PEER})
    check("set_peer to a new address is taken", reply.get("ok") is True, str(reply))
    st = wait_for(lambda s: s["peer_ip"] == ANOTHER_PEER, 30)
    check("the service restarted itself on the new address", (st or {}).get("peer_ip") == ANOTHER_PEER, str(st))
    check("the new address is saved", json.load(open(settings_file())).get("peer_ip") == ANOTHER_PEER)
    # This copy isn't the installed one, so it writes no launchers and installs nothing.
    programs = glob.glob(f"{PREFIX}/drive_c/users/*/AppData/Roaming/Microsoft/Windows/Start Menu/Programs")
    written = [p for p in glob.glob(f"{programs[0]}/**/mesh-kvm*", recursive=True)] if programs else []
    check("a copy that isn't installed writes no launchers", programs and not written, str(written))
    check("and doesn't install itself", not glob.glob(f"{PREFIX}/drive_c/users/*/AppData/Local/mesh-kvm/mesh-kvm.exe"))


def unpaired_checks(exe):
    # u. Knowing no address, the service waits, and takes one from the app.
    exe.stop()
    os.remove(settings_file())
    exe.start()
    st = wait_for(lambda s: True, 60)
    check("with no address known, the service runs unpaired", bool(st) and st["paired"] is False, str(st))
    check("and still serves the layout", "devices" in state()["config"])
    reply = peer_change_refused({"peer": PEER})
    check("the address is taken while unpaired", reply.get("ok") is True, str(reply))
    st = wait_for(lambda s: s["paired"], 30)
    check("the service picks it up", (st or {}).get("peer_ip") == PEER, str(st))


def second_service_check(exe):
    # v. A second service stops at once and leaves the first alone.
    before = local()
    extra = subprocess.run(["wine", exe.exe, "--role", "windows"], env=exe.env, timeout=30,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    check("a second service stops by itself, with its own status", extra.returncode == 75, str(extra.returncode))
    check("the first one still answers", local()["version"] == before["version"])


def keyboard_checks(peer, inbox, exe):
    # w. The laptop's own keyboard and mouse reach Debian while Debian has focus.
    if not os.environ.get("WINE_LINK_DISPLAY"):
        print("SKIP: the laptop's own keyboard and mouse, as WINE_LINK_DISPLAY isn't set", flush=True)
        return
    st = peer.drive_to_debian()
    check("focus is with Debian", view(st) == (True, "Debian"), str(view(st)))
    inbox.clear()
    env = {**exe.env, "DISPLAY": os.environ["WINE_LINK_DISPLAY"]}
    window = subprocess.run(["xdotool", "search", "--name", DESKTOP_NAME], env=env,
                            capture_output=True, text=True).stdout.split()
    if window:
        subprocess.run(["xdotool", "windowfocus", window[-1]], env=env)
    time.sleep(0.2)
    subprocess.run(["xdotool", "key", "a"], env=env)
    # evdev's KEY_A is 30.
    key = inbox.wait_for(KEY_EVENT, 3, lambda f: f["scancode"] == 30 and f["pressed"],
                         keep_alive=lambda: (peer.send(heartbeat()), peer.send(active_view(DEBIAN))))
    check("a key pressed on the laptop is forwarded to Debian", key is not None)
    inbox.clear()
    subprocess.run(["xdotool", "mousemove_relative", "--", "30", "20"], env=env)
    moved = inbox.wait_for(MOUSE_MOVE, 3, keep_alive=lambda: (peer.send(heartbeat()), peer.send(active_view(DEBIAN))))
    check("the laptop's own mouse is forwarded to Debian", moved is not None)


def held_keyboard_checks(peer, inbox, exe):
    # x. To a peer on this version the laptop's keys go numbered, and its heartbeat says what's held.
    if not os.environ.get("WINE_LINK_DISPLAY"):
        return
    env = {**exe.env, "DISPLAY": os.environ["WINE_LINK_DISPLAY"]}
    st = peer.drive_to_debian()
    check("focus is with Debian", view(st) == (True, "Debian"), str(view(st)))
    beats = Beats(peer, inbox)
    alive = lambda: (beats.send(), peer.send(active_view(DEBIAN)))
    beats.pump(0.6)
    inbox.clear()
    subprocess.run(["xdotool", "key", "a"], env=env)
    numbered = inbox.wait_for(PRESSES, 3, lambda f: (30, 1) in f["events"], keep_alive=alive)
    check("a key on the laptop goes numbered", numbered is not None, str(numbered))
    time.sleep(0.3)
    copies = [f for _, kind, f in list(inbox.seen) if kind == PRESSES and f["events"][-1] == (30, 1)]
    check("in three copies", len(copies) == 3, str(len(copies)))
    check("and no plain key goes with it", not [f for _, kind, f in list(inbox.seen) if kind == KEY_EVENT])
    subprocess.run(["xdotool", "keydown", "b"], env=env)
    inbox.clear()
    held = inbox.wait_for(LINK_STATE, 3, lambda f: 48 in f["keys"], keep_alive=alive)
    check("the laptop's heartbeat says what it holds", held is not None, str(held))
    subprocess.run(["xdotool", "keyup", "b"], env=env)
    inbox.clear()
    clear = inbox.wait_for(LINK_STATE, 3, lambda f: not f["keys"], keep_alive=alive)
    check("and stops once it's let go", clear is not None, str(clear))
    # Past the link timeout the laptop's own keyboard is free again, with nothing left forwarding.
    subprocess.run(["xdotool", "keydown", "b"], env=env)
    time.sleep(0.2)
    st = wait_for(lambda s: not s["peer_online"], 3)
    subprocess.run(["xdotool", "keyup", "b"], env=env)
    check("a stall frees the laptop's keyboard", view(st) == (False, "Windows"), str(view(st)))
    inbox.clear()
    subprocess.run(["xdotool", "key", "c"], env=env)
    time.sleep(0.3)
    stray = [f for _, kind, f in list(inbox.seen) if kind in (KEY_EVENT, PRESSES)]
    check("and its keys stay on the laptop", not stray, str(stray))
    st = peer.drive_to_debian()
    check("focus goes back to Debian", view(st) == (True, "Debian"), str(view(st)))


def as_version(program, version, extra=""):
    """The same program, claiming to be `version`, as the test hooks read it off the end."""
    return program + f"MESH-KVM-TEST-VERSION={version} {extra}\n".encode()


def action(name, **fields):
    return call("/api/local/action", {"action": name, **fields})


def update_state():
    return local()["update"]


def answers_on(version, timeout):
    return wait_for(lambda s: s.get("version") == version, timeout)


def service_command_lines():
    """What each service in this prefix was started with."""
    lines = []
    for pid in glob.glob("/proc/[0-9]*"):
        with contextlib.suppress(OSError):
            environ = pathlib.Path(pid, "environ").read_bytes()
            command = pathlib.Path(pid, "cmdline").read_bytes().split(b"\0")
            if f"WINEPREFIX={PREFIX}".encode() in environ and b"--role" in command and b"--update-watch" not in command:
                lines.append(command)
    return lines


def log_in(env, log):
    """What Windows does at login, with the script the program wrote there."""
    scripts = glob.glob(f"{PREFIX}/drive_c/users/*/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/[Ss]tart[Uu]p/mesh-kvm-autostart.vbs")
    check("the login script is there", len(scripts) == 1, str(scripts))
    relative = pathlib.Path(scripts[0]).relative_to(f"{PREFIX}/drive_c")
    subprocess.Popen(["wine", "wscript", "C:\\" + str(relative).replace("/", "\\")], env=env, stdout=log, stderr=subprocess.STDOUT)


class PortHolder:
    """Takes this device's input port as soon as it's free, as something that lingers would, and
    keeps it for `hold` seconds. `taken_after` is how long it had to wait for it."""

    def __init__(self, port, hold):
        self.port, self.hold = port, hold
        self.taken_after = None
        self.started = time.time()
        threading.Thread(target=self.run, daemon=True).start()

    def run(self):
        while time.time() - self.started < 120:
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                sock.bind(("0.0.0.0", self.port))
            except OSError:
                sock.close()
                time.sleep(0.05)
                continue
            self.taken_after = time.time() - self.started
            time.sleep(self.hold)
            sock.close()
            return


def looks_until(cond, timeout):
    """Every /api/local answer until cond(state) holds, as (version, proving)."""
    seen = []
    deadline = time.time() + timeout
    while time.time() < deadline:
        with contextlib.suppress(Exception):
            st = local()
            seen.append((st.get("version"), st["update"]["proving"]))
            if cond(st):
                break
        time.sleep(0.1)
    return seen


def rejected_versions(folder):
    with contextlib.suppress(OSError):
        return json.loads((folder / "updates/check.json").read_text()).get("rejected", [])
    return []


def update_checks(hooks_exe, env, log, layout):
    # z. The whole self-update: install, watcher and restore, with nothing pretend but the release.
    release = ReleaseServer()
    env = env | {"MESH_KVM_TEST_RELEASE_SERVER": release.address, "MESH_KVM_TEST_RELEASE_KEY": release.public}
    folder = pathlib.Path(glob.glob(f"{PREFIX}/drive_c/users/*/AppData/Local")[0]) / "mesh-kvm"
    shutil.rmtree(folder, ignore_errors=True)
    folder.mkdir()
    installed = folder / "mesh-kvm.exe"
    aside = folder / "mesh-kvm.exe.old"
    marker = folder / "update-pending.json"
    base = pathlib.Path(hooks_exe).read_bytes()
    first, second = as_version(base, "0.7.0"), as_version(base, "0.7.1")
    installed.write_bytes(first)
    windows_path = "C:\\users\\" + folder.parent.parent.parent.name + "\\AppData\\Local\\mesh-kvm\\mesh-kvm.exe"
    exe = Exe(windows_path, env, log)
    # As mesh-kvm-setup.exe leaves it, so the update can be seen to keep it right.
    uninstall_key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\mesh-kvm"
    subprocess.run(["wine", "reg", "add", uninstall_key, "/v", "DisplayVersion", "/d", "0.7.0", "/f"], env=env, stdout=log, stderr=subprocess.STDOUT)
    listed_version = lambda: subprocess.run(["wine", "reg", "query", uninstall_key, "/v", "DisplayVersion"], env=env, capture_output=True, text=True).stdout.split()[-1:]
    try:
        exe.start("--peer", PEER, "--port", "24860")
        check("the installed copy answers on its version", answers_on("0.7.0", 90) is not None, "see wine_link.log")
        scripts = lambda: glob.glob(f"{PREFIX}/drive_c/users/*/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/*/mesh-kvm-autostart.vbs")
        wait_for(lambda s: scripts(), 20)
        check("the installed copy writes the login script, which doesn't pin an address",
              bool(scripts()) and "--peer" not in open(scripts()[0]).read(), str(scripts()))
        check("an update knows what's running", update_state()["current"] == "0.7.0", str(update_state()))
        check("nothing is offered yet", update_state()["ready"] is False, str(update_state()))

        # A real download, through the real checks.
        release.publish("0.7.9", b"not what was signed", signed_exe=b"what was signed")
        action("check_update")
        st = wait_for(lambda s: s["update"]["note"], 30)
        check("a file that doesn't match is turned down and said so", "doesn't match" in (st["update"]["note"] or ""), str(st["update"]))
        check("nothing was offered", st["update"]["ready"] is False, str(st["update"]))
        check("the bad file is gone", not (folder / "updates/0.7.9").exists())

        release.publish("0.7.1", second)
        action("check_update")
        st = wait_for(lambda s: s["update"]["ready"], 60)
        check("a good release ends up ready", st["update"]["ready"] and st["update"]["state"] == "ready", str(st["update"]))
        check("it's the version published", st["update"]["available"] == "0.7.1", str(st["update"]))

        refused = post_from(STRANGER, "/api/local/action", {"action": "install_update"})
        check("a stranger can't start an update", refused == 403, str(refused))
        reply = call("/api/local/action", {"action": "install_update"}, {"Origin": "http://evil.example"})
        check("a web page can't start an update", reply.get("ok") is False, str(reply))
        check("nothing has happened yet", not aside.exists() and local()["version"] == "0.7.0")

        # Updating both from here: the other device goes first, and this one waits for it.
        layout["peer_version"] = "0.7.0"
        reply = action("update_both")
        check("updating both starts", reply.get("ok") is True, str(reply))
        st = wait_for(lambda s: layout.get("asked_to_install"), 10)
        check("the other device is asked to install first", bool(layout.get("asked_to_install")), str(st["update"]))
        check("what it's asked names no file or version", layout.get("asked_to_install") == {"action": "install_update"}, str(layout.get("asked_to_install")))
        time.sleep(3)
        check("this device waits for the other one", local()["version"] == "0.7.0" and not aside.exists(), str(update_state()))
        check("the banner step is the other device's", update_state()["state"] == "updating_peer", str(update_state()))
        layout["peer_version"] = "0.7.1"
        layout["peer_proving"] = True
        time.sleep(5)
        check("a peer still proving itself isn't followed yet", local()["version"] == "0.7.0" and not aside.exists(), str(update_state()))
        layout["peer_proving"] = False
        st = answers_on("0.7.1", 60)
        check("the new version answers from the installed place", st is not None and st["version"] == "0.7.1", str(st and st.get("version")))
        check("the old exe stays beside it", aside.exists() and aside.read_bytes() == first)
        check("the installed file is the new one", installed.read_bytes() == second)
        check("a marker waits for it to prove itself", marker.exists())
        check("the status says it's proving", st["update"]["proving"] is True, str(st["update"]))
        reply = action("install_update")
        check("nothing new installs meanwhile", reply.get("ok") is False, str(reply))
        lines = service_command_lines()
        check("the new service keeps its arguments", any(b"--port" in line and b"24860" in line for line in lines), str(lines))
        time.sleep(20)
        check("it isn't healthy too early", marker.exists())
        deadline = time.time() + 30
        while marker.exists() and time.time() < deadline:
            time.sleep(0.5)
        check("after 30 seconds of answering the marker goes", not marker.exists())
        check("the status stops saying it's proving", wait_for(lambda s: not s["update"]["proving"], 5)["update"]["proving"] is False)
        deadline = time.time() + 5
        while listed_version() != ["0.7.1"] and time.time() < deadline:
            time.sleep(0.5)
        check("Installed apps shows the new version", listed_version() == ["0.7.1"], str(listed_version()))
        time.sleep(35)
        st = local()
        check("the watcher leaves a healthy copy alone", st["version"] == "0.7.1" and installed.read_bytes() == second, st["version"])
        check("nothing is on offer any more", st["update"]["ready"] is False and st["update"]["state"] == "idle", str(st["update"]))

        # A new exe that won't start at all, asked for by the other device.
        release.publish("0.7.2", b"this is no exe")
        action("check_update")
        wait_for(lambda s: s["update"]["ready"], 30)
        status = post_from(PEER, "/api/local/action", {"action": "install_update", "file": "C:\\evil.exe", "version": "9.9.9"})
        check("the other device can start an update, and can't name anything", status == 200, str(status))
        wait_for(lambda s: False, 5)
        check("the swap happened", installed.read_bytes() == b"this is no exe")
        started = time.time()
        st = wait_for(lambda s: s.get("version") == "0.7.1", 150)
        took = time.time() - started
        check("a new exe that won't start is replaced by the old one", st is not None and st.get("version") == "0.7.1", str(st and st.get("version")))
        check("that took about a minute and a half", 70 < took < 125, str(took))
        check("the old exe is back in place", installed.read_bytes() == second)
        check("there's nothing left waiting", not marker.exists() and not aside.exists())
        note = (st or {}).get("update", {}).get("note") or ""
        check("a note says what happened", "0.7.2" in note and "0.7.1" in note, note)
        check("the banner shows it as failed", st["update"]["state"] == "failed", str(st["update"]))
        check("the note says it won't be offered again", "won't be offered again" in note, note)
        check("the version is turned down", "0.7.2" in rejected_versions(folder), str(rejected_versions(folder)))
        check("nothing is on offer after a rollback", st["update"]["ready"] is False, str(st["update"]))
        reply = action("install_update")
        check("installing it again is refused", reply.get("ok") is False, str(reply))
        status = post_from(PEER, "/api/local/action", {"action": "install_update"})
        check("the other device gets no further", local()["version"] == "0.7.1" and installed.read_bytes() == second, str(status))
        action("check_update")
        time.sleep(3)
        check("a later look doesn't bring it back", update_state()["ready"] is False, str(update_state()))

        # A new exe that starts and never gets as far as answering.
        hanging = as_version(base, "0.7.3", "HANG")
        release.publish("0.7.3", hanging)
        action("check_update")
        wait_for(lambda s: s["update"]["available"] == "0.7.3" and s["update"]["ready"], 30)
        reply = action("install_update")
        check("the hanging version starts installing", reply.get("ok") is True, str(reply))
        wait_for(lambda s: False, 5)
        check("it's in place", installed.read_bytes() == hanging)
        st = wait_for(lambda s: s.get("version") == "0.7.1", 150)
        check("a new exe that hangs is stopped and replaced too", st is not None and st.get("version") == "0.7.1", str(st and st.get("version")))
        check("the old exe is back in place again", installed.read_bytes() == second)
        check("the note names the version", "0.7.3" in ((st or {}).get("update", {}).get("note") or ""), str(st and st["update"]))

        # A swap that can't happen leaves this version running, and never an older one.
        fourth = as_version(base, "0.7.4")
        release.publish("0.7.4", fourth)
        action("check_update")
        wait_for(lambda s: s["update"]["available"] == "0.7.4" and s["update"]["ready"], 30)
        aside.write_bytes(first)
        marker.mkdir()
        action("install_update")
        time.sleep(3)
        st = wait_for(lambda s: "couldn't be installed" in (s["update"]["note"] or ""), 30)
        check("a marker that can't be written stops the update and says so", st is not None and "couldn't be installed" in (st["update"]["note"] or ""), str(st and st["update"]))
        check("that leaves this version in place, with no downgrade", installed.read_bytes() == second and local()["version"] == "0.7.1")
        check("the older exe beside it is untouched", aside.read_bytes() == first)
        marker.rmdir()
        aside.unlink()
        aside.mkdir()
        (aside / "held").write_text("in use")
        action("install_update")
        time.sleep(3)
        st = wait_for(lambda s: "move the running copy aside" in (s["update"]["note"] or ""), 30)
        check("an old exe that can't be replaced stops the update and says so", st is not None, str(st and st["update"]))
        check("that leaves this version in place too", installed.read_bytes() == second and local()["version"] == "0.7.1")
        check("nothing is left waiting", not marker.exists() and not (folder / "mesh-kvm.exe.new").exists())
        shutil.rmtree(aside)

        # A reboot while a good new version is proving itself: the login script sees it through.
        action("install_update")
        st = wait_for(lambda s: s.get("version") == "0.7.4", 60)
        check("the swap goes through once it can", st is not None and st.get("version") == "0.7.4", str(st and st.get("version")))
        check("it's proving when the power goes", marker.exists())
        exe.stop()
        check("after the reboot the marker is still there", marker.exists() and aside.exists())
        log_in(env, log)
        st = wait_for(lambda s: s.get("version") == "0.7.4", 60)
        check("the login script starts the new version", st is not None and st.get("version") == "0.7.4", str(st and st.get("version")))
        deadline = time.time() + 60
        while marker.exists() and time.time() < deadline:
            time.sleep(0.5)
        check("it proves itself after the reboot", not marker.exists())
        time.sleep(35)
        check("the watcher from the login script leaves it alone", local()["version"] == "0.7.4" and installed.read_bytes() == fourth)
        exe.stop()

        # A reboot while a new version that hangs has the installed place.
        fifth = as_version(base, "0.7.5", "HANG")
        release.publish("0.7.5", fifth)
        log_in(env, log)
        answers_on("0.7.4", 60)
        action("check_update")
        wait_for(lambda s: s["update"]["available"] == "0.7.5" and s["update"]["ready"], 30)
        action("install_update")
        wait_for(lambda s: False, 4)
        check("the hanging version is in place at the reboot", installed.read_bytes() == fifth and marker.exists())
        exe.stop()
        log_in(env, log)
        st = wait_for(lambda s: s.get("version") == "0.7.4", 150)
        check("after the reboot the old version comes back by itself", st is not None and st.get("version") == "0.7.4", str(st and st.get("version")))
        check("the file is back too", installed.read_bytes() == fourth and not marker.exists())
        check("that version is turned down", "0.7.5" in rejected_versions(folder), str(rejected_versions(folder)))
        exe.stop()

        # A crash between the two renames: no installed exe at all.
        sixth = as_version(base, "0.7.6")
        aside.write_bytes(fourth)
        installed.unlink()
        (folder / "mesh-kvm.exe.new").write_bytes(sixth)
        marker.write_text(json.dumps({"from": "0.7.4", "to": "0.7.6"}))
        log_in(env, log)
        started = time.time()
        st = wait_for(lambda s: s.get("version") == "0.7.4", 60)
        check("with no installed exe at all, login brings the old one back", st is not None and st.get("version") == "0.7.4", str(st and st.get("version")))
        check("and doesn't wait a minute for it", time.time() - started < 40, str(time.time() - started))
        check("the exe is in its place", installed.read_bytes() == fourth and not marker.exists())
        check("the note says it was cut short", "interrupted" in (st or {}).get("update", {}).get("note", ""), str(st and st["update"]))
        check("that version isn't held against it", "0.7.6" not in rejected_versions(folder), str(rejected_versions(folder)))
        check("the half-finished copy is cleared away", not (folder / "mesh-kvm.exe.new").exists())
        exe.stop()

        # An old service that takes its time to end, holding its ports, and the input port still
        # taken for a while after it has gone, as with something that inherited it.
        lingering = as_version(base, "0.7.7", "LINGER")
        installed.write_bytes(lingering)
        aside.unlink(missing_ok=True)
        marker.unlink(missing_ok=True)
        exe.start("--peer", PEER, "--port", "24860")
        check("an old service that will linger answers", answers_on("0.7.7", 90) is not None, "see wine_link.log")
        eighth = as_version(base, "0.7.8")
        release.publish("0.7.8", eighth)
        action("check_update")
        wait_for(lambda s: s["update"]["available"] == "0.7.8" and s["update"]["ready"], 30)
        holder = PortHolder(24860, hold=8)
        reply = action("install_update")
        check("the update starts", reply.get("ok") is True, str(reply))
        seen = looks_until(lambda s: s.get("version") == "0.7.8", 150)
        check("the old service kept its port for a while after the swap", (holder.taken_after or 0) > 5, str(holder.taken_after))
        check("the new version still ends up running", seen and seen[-1][0] == "0.7.8", str(seen[-3:]))
        check("it says it's proving from its first answer", ("0.7.8", False) not in seen, str(sorted(set(seen))))
        deadline = time.time() + 90
        while marker.exists() and time.time() < deadline:
            time.sleep(0.5)
        check("it proves itself once it has its ports", not marker.exists() and installed.read_bytes() == eighth)
        check("and isn't held against it", "0.7.8" not in rejected_versions(folder), str(rejected_versions(folder)))
        probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            probe.bind(("0.0.0.0", 24860))
            has_port = False
        except OSError:
            has_port = True
        probe.close()
        check("and holds its input port", has_port)

        # A new copy that can't get its input port for longer than it waits, like the laptop's
        # 0.7.1: it answers, then dies. It must never look as if it had proved itself.
        # 0.7.9 was turned down at the start, so this one is 0.7.20.
        ninth = as_version(base, "0.7.20")
        release.publish("0.7.20", ninth)
        action("check_update")
        wait_for(lambda s: s["update"]["available"] == "0.7.20" and s["update"]["ready"], 30)
        holder = PortHolder(24860, hold=40)
        action("install_update")
        seen = looks_until(lambda s: s.get("version") == "0.7.8" and s["update"]["state"] == "failed", 200)
        check("the new copy answered before it died", ("0.7.20", True) in seen, str(sorted(set(seen))))
        check("and never said it had proved itself", ("0.7.20", False) not in seen, str(sorted(set(seen))))
        check("the old version is put back and runs", seen and seen[-1] == ("0.7.8", False), str(seen[-3:]))
        check("the old exe is back in place", installed.read_bytes() == eighth and not marker.exists())
        check("that version is turned down", "0.7.20" in rejected_versions(folder), str(rejected_versions(folder)))
        logged = (folder / "mesh-kvm.log").read_text(errors="replace")
        bind_failures = [line for line in logged.splitlines() if "Failed to bind UDP" in line]
        check("the log says why", bool(bind_failures), logged[-500:])
        check("with the time it happened", all(line[:2] == "20" and line[4] == "-" and line[13] == ":" for line in bind_failures), str(bind_failures))

        # The other device comes back on the new version, answers, then dies: this one stays put.
        layout.pop("asked_to_install", None)
        layout["peer_version"] = "0.7.8"
        layout["peer_proving"] = False
        tenth = as_version(base, "0.7.21")
        release.publish("0.7.21", tenth)
        action("check_update")
        wait_for(lambda s: s["update"]["available"] == "0.7.21" and s["update"]["ready"], 30)
        reply = action("update_both")
        check("updating both starts again", reply.get("ok") is True, str(reply))
        wait_for(lambda s: layout.get("asked_to_install"), 10)
        layout["peer_version"] = "0.7.21"
        time.sleep(3)
        layout["peer_gone"] = True
        time.sleep(25)
        st = local()
        check("a peer that answered on the new version and died isn't followed", st["version"] == "0.7.8" and installed.read_bytes() == eighth, st["version"])
        check("this device is still waiting for it", st["update"]["state"] in ("updating_peer", "waiting_for_peer"), str(st["update"]))
        check("nothing was swapped here", not marker.exists())
        layout.pop("peer_gone", None)
    finally:
        exe.stop()


def test(exe_path):
    if not os.path.exists(exe_path):
        sys.exit(f"exe not found: {exe_path}")
    shutil.rmtree(PREFIX, ignore_errors=True)
    os.makedirs(PREFIX, exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k not in ("DISPLAY", "WAYLAND_DISPLAY")}
    env.update(WINEPREFIX=PREFIX, WINEDEBUG="-all", WINEDLLOVERRIDES="mscoree,mshtml=;winemenubuilder.exe=d")
    # Wine looks for the desktop's Wayland socket here, and setting up a prefix can hang on it.
    env["XDG_RUNTIME_DIR"] = os.path.join(os.path.dirname(PREFIX), "wine_link_run")
    os.makedirs(env["XDG_RUNTIME_DIR"], mode=0o700, exist_ok=True)
    if os.environ.get("WINE_LINK_DISPLAY"):
        env["DISPLAY"] = os.environ["WINE_LINK_DISPLAY"]
    log = open(os.path.join(os.path.dirname(PREFIX), "wine_link.log"), "w")
    holder = make_other_device()
    exe = Exe(exe_path, env, log)
    try:
        with other_device():
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.bind((PEER, INPUT_PORT))
            stranger = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            stranger.bind((STRANGER, 0))
        peer, inbox = Peer(sock, stranger), Inbox(sock)
        layout = {"state": {}}
        serve_layout(layout)

        exe.start("--peer", PEER)
        if wait_for(lambda s: True, 90) is None:
            check("daemon answers on /api/local", False, "see wine_link.log")
            return 1

        link_checks(peer)
        heartbeat_checks(peer, inbox)
        reported_monitors_check(peer)
        crossing = input_checks(peer, inbox)
        if crossing:
            # The push stopped at laptop x = 0 and y = 300ish. Debian's report put its rightmost
            # column at 2559, and the laptop still sits where the fresh layout put it.
            fields = crossing[1]
            check("the crossing lands on Debian's own monitor, in from the edge",
                  fields["x"] == 2559 - 28, str(fields))
        keyboard_checks(peer, inbox, exe)
        link_state_checks(peer, inbox)
        dropout_checks(peer, inbox)
        held_keyboard_checks(peer, inbox, exe)
        api_checks(peer, inbox)
        audio_checks()
        layout_checks(peer, inbox, layout)
        display_checks()
        security_checks(pinned_peer=True)
        second_service_check(exe)
        saved_peer_checks(exe)
        unpaired_checks(exe)
        exe.stop()
        hooks_exe = os.environ.get("MESH_KVM_HOOKS_EXE")
        if hooks_exe:
            update_checks(hooks_exe, env, log, layout)
        else:
            print("SKIP: the self-update, as MESH_KVM_HOOKS_EXE isn't set", flush=True)
    finally:
        exe.stop()
        holder.kill()
    return 1 if failures else 0


if __name__ == "__main__":
    path = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("MESH_KVM_EXE", DEFAULT_EXE)
    code = test(path)
    print("ALL PASS" if code == 0 else f"{failures} FAILED")
    sys.exit(code)
