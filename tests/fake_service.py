#!/usr/bin/env python3
"""A pretend mesh-kvm service, for looking at the app with no real one running.

It answers on the app's port with two devices and however many monitors you ask for, and it plays
the mode change countdown the way the real service does. Run it where that port is free:

    unshare -rn bash -c 'ip link set lo up && python3 -I tests/fake_service.py --monitors 5 & \
        target/release/mesh-kvm --app'

--name-style long tries names that don't fit. --audio picks how the audio section starts: idle,
sending, failure, echo, missing, old, laptop, nomute or wrong. POST {"state": ...} to /fake/audio to change it while running,
and GET /fake/requests for the audio actions the app has sent. --link picks how the link looks: good,
poor, measuring, returning, down, oneway or older. POST {"state": ...} to /fake/link to change that too.
--update picks where an update has got to: none, both, offline, peer-downloading, here-downloading, updating-peer,
waiting-peer, installing, view-only, peer-failed, apt-failed, restored, no-polkit or stale. POST {"state": ...} to
/fake/update to change that too, and GET /fake/requests shows the update actions the app has sent.
--version says what version the service is, which POST {"version": ...} to /fake/version changes, as
an update would, so the app can be seen starting again. POST {"device": "Windows", "stage": ...} to /fake/mode to
hold a change to that device's first screen at applying or going_back, as a service waiting on a screen does.
--secure picks how the link's encryption stands, for both devices: encrypted, connecting, unpaired or old-peer (an
other device too old to pair). Leave it out for a service that doesn't encrypt. POST {"state": ...} to /fake/secure to
change it while running.
--details picks how each device's connection details (GET /api/local/link) look: healthy, wifi-drop (a drop a moment
ago that this device's Wi-Fi caused, with a roam and a stall in the history), unreachable (the other device's details
can't be reached) or older (the other device is too old to have them). POST {"state": ...} to /fake/details to change
that too. GET /fake/requests also counts the asks for the details, so it shows they stop while the panel is closed.
GET /api/pairing answers from a small pairing state machine: pair_show shows the code 482917 counting down from 120,
pair_enter with 482917 pairs with the fingerprint 4F2A-91C3, any other code fails as a wrong code, pair_cancel goes
back to idle, and unpair leaves both devices unpaired.
"""

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = 24880
REVERT_AFTER = 15
PAIRING_CODE = "482917"
CODE_LASTS = 120
FINGERPRINT = "4F2A-91C3"


def modes(sizes_and_rates):
    return [{"width": w, "height": h, "millihertz": hz, "id": f"{i}"} for i, (w, h, hz) in enumerate(sizes_and_rates)]


BIG = modes([(2560, 1440, 359800), (2560, 1440, 240000), (2560, 1440, 144000), (2560, 1440, 60000),
             (1920, 1080, 240000), (1920, 1080, 60000), (1920, 1080, 59940), (1280, 720, 60000)])
SIDE = modes([(1920, 1080, 239760), (1920, 1080, 144000), (1920, 1080, 60000), (1280, 720, 60000)])
PANEL = modes([(2560, 1600, 165000), (2560, 1600, 60000), (1920, 1200, 60000), (1680, 1050, 60000)])
TV = modes([(3840, 2160, 60000), (3840, 2160, 30000), (1920, 1080, 60000)])


def screen(ident, name, x, y, table, can_change=True):
    return {"id": ident, "name": name, "x": x, "y": y, "modes": table, "current": table[0], "can_change": can_change}


def screens(count):
    first = [screen("DP-3", "ASUS VG259QM", 0, 360, SIDE), screen("DP-2", "Alienware AW2725DF", 1920, 0, BIG)]
    second = [screen("Display 1", "Display 1", 0, 0, PANEL)]
    if count >= 5:
        first.append(screen("HDMI-A-1", "LG TV SSCR2", 4480, -300, TV))
        second = [screen("Display 2", "DELL U2720Q", -1920, -200, SIDE), second[0],
                  screen("Display 3", "Portable screen", 2560, 400, modes([(1920, 1080, 60000)]), can_change=False)]
    return {"Debian": first, "Windows": second}


class World:
    def __init__(self, count, long_names, read_only=False):
        self.lock = threading.Lock()
        self.screens = screens(count)
        if read_only:
            self.screens["Debian"] = []
        self.revision = 3
        self.attach_side = "Bottom"
        self.resistance = 55
        self.places = {"Debian": [0, 0], "Windows": [1920, 1640 if count >= 5 else 1440]}
        long_name = "The very long name of this big tower"
        self.names = {"Debian": long_name[:32] if long_names else "Bread's desk", "Windows": "ThinkPad with a long name too" if long_names else "ThinkPad"}
        self.active = "Debian"
        self.change = {"Debian": None, "Windows": None}
        self.note = {"Debian": None, "Windows": None}
        self.settings = {"Debian": default_settings(), "Windows": default_settings()}
        self.requests = []
        self.update = update_states()["none"]
        self.version = "0.3.0"
        self.secure = None
        self.old_peer = False
        self.pairing = {"state": "idle"}
        self.code_until = None
        self.details = "healthy"
        self.details_since = time.time()
        self.details_asked = {"Debian": 0, "Windows": 0}

    def rects(self, node):
        if not self.screens[node]:
            # The desktop isn't answering about modes, and the layout still knows these.
            return [{"name": "DP-3 (240Hz Native)", "x": 0, "y": 360, "w": 1920, "h": 1080},
                    {"name": "DP-2 (360Hz Native)", "x": 1920, "y": 0, "w": 2560, "h": 1440}]
        out = []
        for s in self.screens[node]:
            m = s["current"]
            out.append({"name": s["id"] if node == "Windows" else f'{s["id"]} ({m["millihertz"] // 1000}Hz Native)',
                        "x": s["x"], "y": s["y"], "w": m["width"], "h": m["height"]})
        return out

    def state(self):
        devices = [{"node": n, "name": self.names[n], "x": self.places[n][0], "y": self.places[n][1],
                    "monitors": self.rects(n), "custom_size": None} for n in ("Debian", "Windows")]
        config = {"revision": self.revision, "editor": "Debian", "devices": devices, "attach_side": self.attach_side,
                  "edge_resistance": self.resistance, "auto_game_lock": True, "manual_game_lock": False}
        telemetry = {"active_node": self.active, "peer_online": self.online, "panic_mode": False, "cursor_x": 2600, "cursor_y": 700,
                     "peer_addr": "192.168.8.241:24850", "game_captive_active": False, "game_reason": "", "main_conflict": False}
        return {"config": config, "telemetry": telemetry}

    def local(self, node):
        change = self.change[node]
        status = {
            "node": node.lower(), "device_name": self.names[node], "hostname": "bread" if node == "Debian" else "THINKPAD",
            "ip": "192.168.8.210" if node == "Debian" else "192.168.8.241", "peer_ip": "192.168.8.241", "paired": True,
            "version": self.version, "os": "", "label": "", "peer_online": self.online, "active_node": self.active, "panic_mode": False,
            "game_lock": False, "game_reason": "", "displays": self.rects(node), "cursor": [2600, 700], "autostart": True,
            "peer_pinned": False, "main_device": 0, "main_conflict": False,
            "display_change": {"change": None, "note": self.note[node]},
            "link": self.link,
            "update": self.update[0 if node == "Debian" else 1],
        }
        if self.secure is not None:
            status.update({"secure": self.secure, "old_peer": self.old_peer})
        status.update(self.settings[node])
        if change:
            left = max(0, int((change["until"] - time.time()) * 1000))
            status["display_change"]["change"] = {k: change[k] for k in ("id", "output", "name", "from", "to")} | {"stage": change.get("stage", "waiting"), "ms_left": left}
        return status


def pairing_status(world):
    """What `/api/pairing` says, with the code's time counted down and run out."""
    status = dict(world.pairing)
    if status["state"] == "showing":
        left = world.code_until - time.time()
        if left <= 0:
            world.pairing = {"state": "failed", "error": "The code ran out. Show a new one."}
            return pairing_status(world)
        status["seconds_left"] = int(left) + (left % 1 > 0)
    if world.secure not in (None, "unpaired"):
        status |= {"peer_name": world.names["Windows"], "fingerprint": FINGERPRINT}
    return status


def pair(world, body):
    """The pairing actions, as the real service's state machine takes them."""
    action = body.get("action")
    if action == "pair_show":
        world.pairing = {"state": "showing", "code": PAIRING_CODE}
        world.code_until = time.time() + CODE_LASTS
    elif action == "pair_enter" and body.get("code") == PAIRING_CODE:
        world.secure, world.old_peer = "encrypted", False
        world.pairing = {"state": "paired"}
    elif action == "pair_enter":
        world.pairing = {"state": "failed", "error": "The code was wrong. Show a new code and try again."}
    elif action == "pair_cancel":
        world.pairing = {"state": "idle"}
    elif action == "unpair":
        world.secure = "unpaired"
        world.pairing = {"state": "idle"}


def set_secure_state(world, name):
    world.secure, world.old_peer = {"encrypted": ("encrypted", False), "connecting": ("connecting", False),
                                    "unpaired": ("unpaired", False), "old-peer": ("unpaired", True)}[name]
    world.pairing = {"state": "idle"}


def link_states():
    figures = {"measured": True, "peer_version": "same", "returning": False}
    nothing = {"measured": False, "loss_in": 0, "loss_out": 0, "round_trip_ms": None, "audio_loss_in": None, "audio_loss_out": None}
    return {
        "good": (True, figures | {"loss_in": 0.2, "loss_out": 0.4, "round_trip_ms": 4, "audio_loss_in": None, "audio_loss_out": None}),
        "poor": (True, figures | {"loss_in": 3.5, "loss_out": 6.2, "round_trip_ms": 188, "audio_loss_in": 8.4, "audio_loss_out": 7.0}),
        "measuring": (True, nothing | {"peer_version": "unknown", "returning": False}),
        "returning": (False, nothing | {"peer_version": "unknown", "returning": True}),
        "down": (False, nothing | {"peer_version": "unknown", "returning": False}),
        "oneway": (True, nothing | {"measured": True, "loss_in": 0.2, "peer_version": "same", "returning": False, "one_way": True}),
        "older": (True, figures | {"loss_in": 0.2, "loss_out": 0, "round_trip_ms": None, "peer_version": "older"}),
    }


def set_link_state(world, name):
    world.online, world.link = link_states()[name]


def link_details(world, node):
    """What `/api/local/link` says on `node`, as the real service builds it: (status, body)."""
    state = world.details
    if node == "Windows" and state == "unreachable":
        return 502, None
    if node == "Windows" and state == "older":
        return 404, None
    now = int(time.time() * 1000)
    started = int(world.details_since * 1000) - (2 * 3600 + 12 * 60) * 1000
    windows = node == "Windows"
    rtt = {"min_ms": 3.0, "avg_ms": 5.6, "max_ms": 14.0 if windows else 11.0, "jitter_ms": 1.3, "answers": 40}
    sound = {"loss_percent": 0.4, "buffer_ms": 34 if windows else 42, "buffer_peak_ms": 61, "received": 1000, "lost": 1, "repaired": 1, "late": 0}
    latest = {"at_ms": now - 3000, "online": True, "rtt": rtt, "loss_in": 0.2, "loss_out": 0.0 if windows else 0.4,
              "in_per_s": 62.0, "out_per_s": 60.4, "failed_sends": 0, "failed_last": None, "worst_freeze_ms": 3, "sound": sound}
    ten_min = {"from_ms": now - 597_000, "samples": 60,
               "rtt": {"min_ms": 2.0, "avg_ms": 6.1, "max_ms": 48.0, "jitter_ms": 2.1, "answers": 2400},
               "failed_sends": 0, "worst_freeze_ms": 9, "sound": {"buffer_peak_ms": 88, "received": 60000, "lost": 14, "repaired": 9, "late": 2}}
    events = [{"at_ms": started, "kind": "start", "fields": {"version": world.version, "os": "windows" if windows else "linux", "pid": 4120}},
              {"at_ms": started + 900, "kind": "wifi", "fields": {"addrs": "wlan0=192.168.8.210/24", "ssid": "Bread's Wi-Fi", "signal": 74}},
              {"at_ms": started + 2400, "kind": "linked", "fields": {"down_ms": 2400}}]
    stability = {"drops_today": 0, "last_drop": None}
    wifi = {"state": "connected", "ssid": "Bread's Wi-Fi", "band": "5 GHz", "channel": "36" if windows else "149",
            "freq_mhz": None if windows else 5745, "signal": 81 if windows else 74}
    if state == "wifi-drop":
        dropped = int(world.details_since * 1000) - 22_000
        drop = {"at_ms": dropped, "detected_ms": dropped + 1100, "back_ms": dropped + 2400, "length_ms": 2400,
                "cause": "wifi" if not windows else "unknown", "restarted": None}
        events += [
            {"at_ms": dropped - 40_000, "kind": "stall", "fields": {"late_ms": 420, "loop": "input"}},
            {"at_ms": dropped - 1500, "kind": "roam", "fields": {"from": "aa:bb:cc:00:00:01", "to": "aa:bb:cc:00:00:02", "ssid": "Bread's Wi-Fi", "signal": 41}},
            {"at_ms": dropped + 1100, "kind": "drop", "fields": {"silent_ms": 1100, "rtt_ms": 6, "signal": 41, "length_ms": 2400}, "cause": drop["cause"]},
            {"at_ms": dropped + 2400, "kind": "return", "fields": {"down_ms": 1300, "length_ms": 2400}},
            {"at_ms": dropped + 9000, "kind": "gap", "fields": {"silent_ms": 640, "while_busy": True}},
        ]
        stability = {"drops_today": 3, "last_drop": drop}
        latest["loss_in"], latest["rtt"] = 6.3, rtt | {"max_ms": 212.0, "avg_ms": 31.0, "jitter_ms": 18.4}
        ten_min["worst_freeze_ms"] = 420
        wifi["signal"] = 41
    scans = {"state": "paused"} if windows else {"state": "not_here"}
    marks = ([{"socket": s, "applied": "qwave", "dscp": "56" if s == "input" else "40", "error": None} for s in ("audio_rx", "audio_tx", "input")]
             if windows else [{"socket": s, "applied": "ip_tos", "dscp": "46" if s == "input" else "34", "error": None} for s in ("audio_rx", "audio_tx", "input")])
    return 200, {"version": world.version, "pid": 4120, "now_ms": now, "started_ms": started, "online": True, "since_ms": started + 2400,
                 "latest": latest, "ten_min": ten_min, "stability": stability, "wifi": wifi, "scans": scans, "marks": marks,
                 "control": {"state": "up", "error": None}, "events": events[-20:]}


def default_settings():
    return {
        "sharing": {"mouse": True, "keyboard": True},
        "clipboard": {"send": True, "receive": True, "note": "Copied 12 characters from the other device."},
        "pointer_speed": 0.75,
        "low_latency_wifi": True,
        "audio": audio_idle(),
    }


DEBIAN_DEVICES = {
    "outputs": [{"id": "alsa_output.front", "name": "USB Audio Front Headphones"},
                {"id": "alsa_output.spdif", "name": "USB Audio S/PDIF Output"},
                {"id": "alsa_output.speaker", "name": "USB Audio Speakers"},
                {"id": "bluez_output.sony", "name": "Sony WH-1000XM5 Wireless Noise Cancelling Headphones"},
                {"id": "alsa_output.hdmi", "name": "HDMI / DisplayPort 2 Output (Alienware AW2725DF)"}],
}
WINDOWS_DEVICES = {
    "outputs": [{"id": "{0.0.0.a}", "name": "Speakers (Realtek(R) Audio)"},
                {"id": "{0.0.0.b}", "name": "Headphones (USB Audio Device) (1)"},
                {"id": "{0.0.0.c}", "name": "Headphones (USB Audio Device) (2)"},
                {"id": "{0.0.0.d}", "name": "Digital Audio (HDMI) (NVIDIA High Definition Audio)"}],
}


def audio_idle():
    return {"tx_enabled": False, "output": "", "play": "default", "notes": [], "echo_safe": True,
            "bitrate": 96, "tx_volume": 100, "tx_peak": 0, "tx_reaching": False, "tx_connected": False, "tx_loss": 0,
            "play_only_on_other": True, "rx_muted": False, "rx_volume": 100, "rx_peak": 0, "rx_packets": 0,
            "rx_live": False, "rx_loss": 0.0, "rx_unrepaired": 0.0, "rx_buffer_ms": 0, "rx_other_version": False, "hint": ""}


def audio_states():
    """What each device's audio says in each state, on top of `audio_idle`."""
    sending = {"tx_enabled": True, "output": "default", "tx_peak": 62, "tx_reaching": True, "tx_connected": True, "tx_loss": 3,
               "bitrate": 128}
    receiving = {"rx_peak": 58, "rx_packets": 91200, "rx_live": True, "rx_loss": 2.6, "rx_unrepaired": 0.3, "rx_buffer_ms": 30}
    return {
        "idle": ({}, {}),
        "sending": (sending | {"output": "bluez_output.sony"}, receiving),
        "failure": ({"tx_enabled": True, "output": "alsa_output.front", "tx_connected": True},
                    {"tx_enabled": True, "output": "{0.0.0.a}", "tx_connected": True,
                     "notes": ["The output to send couldn't be opened."]}),
        "echo": (sending | receiving, sending | receiving | {"echo_safe": False}),
        "missing": ({"tx_enabled": True, "output": "bluez_output.gone", "play": "bluez_output.gone",
                     "tx_connected": True,
                     "notes": ["The chosen output to send isn't connected, so the system default is being used.",
                               "The chosen output to play on isn't connected, so the system default is being used."]}, {}),
        "old": ({"rx_other_version": True}, None),
        "laptop": ({}, sending | {"output": "{0.0.0.b}", "hint": "All of this device's sound is sent, from every app and output."}),
        "nomute": ({}, sending | {"tx_reaching": False, "notes": [
            "This output isn't being muted, because the capture that keeps working when muted isn't running. Sound plays on both devices."]}),
        "wrong": ({}, sending | {"output": "{0.0.0.b}", "play_only_on_other": False, "tx_reaching": False, "notes": [
            "Nothing is playing on Headphones (USB Audio Device) (1). Windows is playing on Speakers (Realtek(R) Audio)."]}),
    }


def set_audio_state(world, name):
    debian, windows = audio_states()[name]
    world.settings["Debian"]["audio"] = audio_idle() | DEBIAN_DEVICES | debian
    if windows is None:
        # A service from before the choice of devices. Its status has none of the new fields.
        world.settings["Windows"]["audio"] = {"tx_enabled": True, "tx_volume": 100, "tx_peak": 30, "tx_source": None,
                                              "mute_while_sending": True, "rx_muted": False, "rx_volume": 100, "rx_peak": 0}
    else:
        world.settings["Windows"]["audio"] = audio_idle() | WINDOWS_DEVICES | windows


def update_status(state="idle", available="0.7.1", ready=False, note=None, progress=None, **more):
    return {"current": "0.7.0", "available": available, "ready": ready, "state": state, "note": note, "progress": progress} | more


def update_states():
    """Each device's `update` in each state. `None` for the second means it can't be reached."""
    idle = update_status(available=None)
    ready = update_status("ready", ready=True)
    going = update_status("updating_peer", ready=True)
    return {
        "none": (idle, idle),
        "both": (ready, ready),
        "offline": (ready, None),
        "peer-downloading": (ready, update_status("downloading", progress=45)),
        "here-downloading": (update_status("downloading", progress=12), ready),
        "updating-peer": (going, update_status("installing", ready=True)),
        "waiting-peer": (update_status("waiting_for_peer", ready=True), None),
        "installing": (update_status("installing", ready=True), update_status("idle", available=None)),
        "view-only": (update_status("view_only"), ready),
        "peer-failed": (update_status("failed", ready=True, note="ThinkPad didn't come back on version 0.7.1 within 5 minutes, so this device stayed as it is."), ready),
        "apt-failed": (update_status("failed", ready=True, note="The update failed, and the old version keeps running. E: Sub-process /usr/bin/dpkg returned an error code (1) To go back by hand, run: sudo apt-get install --reinstall --allow-downgrades /var/cache/mesh-kvm/kept/mesh-kvm_0.7.0_amd64.deb"), ready),
        "restored": (update_status("failed", ready=True, note="Version 0.7.1 didn't start properly, so version 0.7.0 was put back."), None),
        "no-polkit": (update_status("ready", ready=True, preflight="needs_password", by_hand="sudo apt-get install /home/bread/.config/mesh-kvm/updates/0.7.1/mesh-kvm_0.7.1_amd64.deb"), ready),
        "stale": (update_status(available=None, note="mesh-kvm hasn't been able to check for updates for 3 days."), idle),
    }


def serve(world):
    class Handler(BaseHTTPRequestHandler):
        def reply(self, body):
            data = json.dumps(body).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)

        def node(self):
            return "Windows" if self.path.startswith("/api/peer") else "Debian"

        def do_GET(self):
            with world.lock:
                expire(world)
                path = self.path.removeprefix("/api/peer")
                if self.path.startswith("/api/peer") and world.update[1] is None:
                    self.send_error(502)
                elif path == "/api/local":
                    self.reply(world.local(self.node()))
                elif path == "/fake/requests":
                    self.reply(world.requests + [{"details_asked": world.details_asked}])
                elif path == "/api/local/link":
                    world.details_asked[self.node()] += 1
                    status, body = link_details(world, self.node())
                    if status == 200:
                        self.reply(body)
                    else:
                        self.send_error(status)
                elif path == "/api/state":
                    self.reply(world.state())
                elif self.path == "/api/pairing":
                    self.reply(pairing_status(world))
                elif path == "/api/displays":
                    self.reply({"displays": world.screens[self.node()]})
                else:
                    self.send_error(404)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
            with world.lock:
                if self.path == "/api/layout":
                    apply_edit(world, body)
                elif self.path == "/api/action" and body.get("action") == "toggle_focus":
                    world.active = "Windows" if world.active == "Debian" else "Debian"
                elif self.path == "/fake/audio":
                    set_audio_state(world, body["state"])
                elif self.path == "/fake/link":
                    set_link_state(world, body["state"])
                elif self.path == "/fake/update":
                    world.update = update_states()[body["state"]]
                elif self.path == "/fake/secure":
                    set_secure_state(world, body["state"])
                elif self.path == "/fake/details":
                    world.details, world.details_since = body["state"], time.time()
                elif self.path == "/fake/version":
                    world.version = body["version"]
                elif self.path == "/fake/mode":
                    hold_mode_change(world, body["device"], body["stage"])
                elif self.path.endswith("/api/local/action"):
                    if any(word in body.get("action", "") for word in ("audio", "play_only", "update")):
                        world.requests.append({"device": self.node()} | body)
                    act(world, self.node(), body)
                self.reply({"ok": True})

        def log_message(self, *args):
            pass

    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()


def apply_edit(world, edit):
    arrangement = edit.get("arrangement")
    if arrangement:
        for d in arrangement["devices"]:
            world.places[d["node"]] = [d["x"], d["y"]]
        world.attach_side = arrangement["attach_side"]
    world.resistance = edit.get("edge_resistance", world.resistance)
    world.revision += 1


def set_mode(world, node, output, mode):
    for s in world.screens[node]:
        if s["id"] == output:
            s["current"] = mode


def act(world, node, body):
    action = body.get("action")
    if action in ("pair_show", "pair_enter", "pair_cancel", "unpair"):
        pair(world, body)
    elif action == "set_device_name":
        world.names[node] = body["name"] or world.names[node]
    elif action == "set_display_mode":
        s = next(s for s in world.screens[node] if s["id"] == body["output"])
        mode = next(m for m in s["modes"] if (m["width"], m["height"], m["millihertz"]) == (body["width"], body["height"], body["millihertz"]))
        world.change[node] = {"output": s["id"], "name": s["name"], "from": s["current"], "to": mode, "until": time.time() + REVERT_AFTER, "id": int(time.time() * 1000)}
        s["current"] = mode
    elif action == "keep_display_mode" and node == "Debian":
        world.change[node] = None
    elif action == "undo_display_mode" and world.change[node]:
        world.change[node]["until"] = 0
        expire(world)
    elif action == "set_sharing":
        world.settings[node]["sharing"].update({k: v for k, v in body.items() if k != "action"})
    elif action == "set_audio_device":
        audio = world.settings[node]["audio"]
        if body["role"] in ("output", "play"):
            audio[body["role"]] = body["id"]
        audio["tx_enabled"] = audio["output"] != ""
    elif action == "set_audio_bitrate":
        world.settings[node]["audio"]["bitrate"] = body["kbps"]
    elif action == "set_low_latency_wifi":
        world.settings[node]["low_latency_wifi"] = body["enabled"]
    elif action == "set_play_only_on_other":
        world.settings[node]["audio"]["play_only_on_other"] = body["enabled"]
    elif action == "set_audio_rx_muted":
        world.settings[node]["audio"]["rx_muted"] = body["muted"]


def hold_mode_change(world, node, stage):
    """A change to `node`'s first screen at `stage`. Only a waiting one counts down."""
    s = world.screens[node][0]
    to = next(m for m in s["modes"] if m != s["current"])
    world.change[node] = {"output": s["id"], "name": s["name"], "from": s["current"], "to": to, "stage": stage,
                          "until": time.time() + (REVERT_AFTER if stage == "waiting" else 24 * 3600), "id": int(time.time() * 1000)}


def expire(world):
    for node, change in world.change.items():
        if change and time.time() >= change["until"]:
            set_mode(world, node, change["output"], change["from"])
            m = change["from"]
            world.note[node] = f'{change["name"]} went back to {m["width"]} × {m["height"]} at {round(m["millihertz"] / 1000)} Hz.'
            world.change[node] = None


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--monitors", type=int, default=2, help="under 5 gives 3 screens, 5 or more gives 6")
    parser.add_argument("--name-style", default="short")
    parser.add_argument("--no-modes", action="store_true", help="this device's desktop isn't answering about modes")
    parser.add_argument("--audio", default="idle", choices=list(audio_states()))
    parser.add_argument("--link", default="good", choices=list(link_states()))
    parser.add_argument("--update", default="none", choices=list(update_states()))
    parser.add_argument("--version", default="0.3.0")
    parser.add_argument("--secure", choices=["encrypted", "connecting", "unpaired", "old-peer"])
    parser.add_argument("--details", default="healthy", choices=["healthy", "wifi-drop", "unreachable", "older"])
    options = parser.parse_args()
    world = World(options.monitors, options.name_style == "long", options.no_modes)
    set_audio_state(world, options.audio)
    set_link_state(world, options.link)
    world.update = update_states()[options.update]
    world.version = options.version
    if options.secure:
        set_secure_state(world, options.secure)
    world.details = options.details
    serve(world)
