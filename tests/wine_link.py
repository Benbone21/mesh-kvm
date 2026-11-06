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
"""
import contextlib
import glob
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
PREFIX = "/tmp/claude-1000/mk/wineprefix"
LAPTOP = "10.77.0.1"
PEER = "10.77.0.2"
STRANGER = "10.77.0.3"
ANOTHER_PEER = "10.77.0.9"
INPUT_PORT = 24850
API_PORT = 24880
LOCAL = f"http://127.0.0.1:{API_PORT}"
DESKTOP_NAME = "wine-link-desktop"

DEBIAN, WINDOWS = 0, 1
LEFT, RIGHT, TOP, BOTTOM = 0, 1, 2, 3
(MOUSE_MOVE, MOUSE_BUTTON, MOUSE_WHEEL, KEY_EVENT, _FOCUS_SWITCH, _FOCUS_ACK, PANIC_RELEASE, HEARTBEAT,
 PEER_DISPLAY_INFO, EDGE_MAP, FOCUS_SWITCH_AT, GAME_LOCK, ACTIVE_VIEW, LAYOUT_REVISION) = range(14)

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
    """The other device's API: `holder["state"]` is what it says at /api/state."""
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            body = json.dumps(holder["state"]).encode() if self.path == "/api/state" else b"{}"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
        daemon_threads = True
        allow_reuse_address = True

    with other_device():
        server = Server((PEER, API_PORT), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()


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
    # The login script a real install writes is the same either way, and never holds the address.
    scripts = glob.glob(f"{PREFIX}/drive_c/users/*/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/*/mesh-kvm-autostart.vbs")
    check("the login script is there and doesn't pin an address",
          bool(scripts) and "--peer" not in open(scripts[0]).read(), str(scripts))


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


def test(exe_path):
    if not os.path.exists(exe_path):
        sys.exit(f"exe not found: {exe_path}")
    shutil.rmtree(PREFIX, ignore_errors=True)
    os.makedirs(PREFIX, exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k not in ("DISPLAY", "WAYLAND_DISPLAY")}
    env.update(WINEPREFIX=PREFIX, WINEDEBUG="-all", WINEDLLOVERRIDES="mscoree,mshtml=")
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
        api_checks(peer, inbox)
        layout_checks(peer, inbox, layout)
        display_checks()
        security_checks(pinned_peer=True)
        second_service_check(exe)
        saved_peer_checks(exe)
        unpaired_checks(exe)
    finally:
        exe.stop()
        holder.kill()
    return 1 if failures else 0


if __name__ == "__main__":
    path = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("MESH_KVM_EXE", DEFAULT_EXE)
    code = test(path)
    print("ALL PASS" if code == 0 else f"{failures} FAILED")
    sys.exit(code)
