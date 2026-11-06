#!/usr/bin/env python3
"""A pretend mesh-kvm service, for looking at the app with no real one running.

It answers on the app's port with two devices and however many monitors you ask for, and it plays
the mode change countdown the way the real service does. Run it where that port is free:

    unshare -rn bash -c 'ip link set lo up && python3 -I tests/fake_service.py --monitors 5 & \
        target/release/mesh-kvm --app'

--name-style long tries names that don't fit. --audio picks how the audio section starts: idle,
sending, failure, echo, missing or old. POST {"state": ...} to /fake/audio to change it while running,
and GET /fake/requests for the audio actions the app has sent. --link picks how the link looks: good,
poor, measuring, returning, down, oneway or older. POST {"state": ...} to /fake/link to change that too.
"""

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = 24880
REVERT_AFTER = 15


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
            "version": "0.3.0", "os": "", "label": "", "peer_online": self.online, "active_node": self.active, "panic_mode": False,
            "game_lock": False, "game_reason": "", "displays": self.rects(node), "cursor": [2600, 700], "autostart": True,
            "peer_pinned": False, "main_device": 0, "main_conflict": False,
            "display_change": {"change": None, "note": self.note[node]},
            "link": self.link,
        }
        status.update(self.settings[node])
        if change:
            left = max(0, int((change["until"] - time.time()) * 1000))
            status["display_change"]["change"] = {k: change[k] for k in ("id", "output", "name", "from", "to")} | {"stage": "waiting", "ms_left": left}
        return status


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


def default_settings():
    return {
        "sharing": {"mouse": True, "keyboard": True},
        "clipboard": {"send": True, "receive": True, "note": "Copied 12 characters from the other device."},
        "pointer_speed": 0.75,
        "audio": audio_idle(),
    }


DEBIAN_DEVICES = {
    "inputs": [{"id": "alsa_input.mic", "name": "USB Audio Microphone"},
               {"id": "alsa_input.line", "name": "USB Audio Line Input"},
               {"id": "alsa_input.brio", "name": "Logitech BRIO 4K Stream Edition Webcam Analog Stereo Microphone Input"}],
    "outputs": [{"id": "alsa_output.front", "name": "USB Audio Front Headphones"},
                {"id": "alsa_output.spdif", "name": "USB Audio S/PDIF Output"},
                {"id": "alsa_output.speaker", "name": "USB Audio Speakers"},
                {"id": "bluez_output.sony", "name": "Sony WH-1000XM5 Wireless Noise Cancelling Headphones"},
                {"id": "alsa_output.hdmi", "name": "HDMI / DisplayPort 2 Output (Alienware AW2725DF)"}],
}
WINDOWS_DEVICES = {
    "inputs": [{"id": "{0.0.1.a}", "name": "Microphone Array (Realtek(R) Audio)"},
               {"id": "{0.0.1.b}", "name": "Microphone (USB Audio Device) (1)"},
               {"id": "{0.0.1.c}", "name": "Microphone (USB Audio Device) (2)"}],
    "outputs": [{"id": "{0.0.0.a}", "name": "Speakers (Realtek(R) Audio)"},
                {"id": "{0.0.0.b}", "name": "Headphones (USB Audio Device) (1)"},
                {"id": "{0.0.0.c}", "name": "Headphones (USB Audio Device) (2)"},
                {"id": "{0.0.0.d}", "name": "Digital Audio (HDMI) (NVIDIA High Definition Audio)"}],
}


def audio_idle():
    return {"tx_enabled": False, "input": "", "output": "", "play": "default", "notes": [], "echo_safe": True,
            "bitrate": 96, "tx_volume": 100, "tx_peak": 0, "tx_reaching": False, "tx_connected": False, "tx_loss": 0,
            "play_only_on_other": True, "rx_muted": False, "rx_volume": 100, "rx_peak": 0, "rx_packets": 0,
            "rx_live": False, "rx_loss": 0.0, "rx_unrepaired": 0.0, "rx_buffer_ms": 0, "rx_other_version": False}


def audio_states():
    """What each device's audio says in each state, on top of `audio_idle`."""
    sending = {"tx_enabled": True, "output": "default", "tx_peak": 62, "tx_reaching": True, "tx_connected": True, "tx_loss": 3,
               "bitrate": 128}
    receiving = {"rx_peak": 58, "rx_packets": 91200, "rx_live": True, "rx_loss": 2.6, "rx_unrepaired": 0.3, "rx_buffer_ms": 30}
    return {
        "idle": ({}, {}),
        "sending": (sending | {"input": "alsa_input.brio", "output": "bluez_output.sony"}, receiving),
        "failure": ({"tx_enabled": True, "input": "alsa_input.mic", "tx_connected": True},
                    {"tx_enabled": True, "input": "{0.0.1.a}", "tx_connected": True,
                     "notes": ["The microphone or line-in couldn't be opened. Check that Microphone access is on in Windows privacy settings."]}),
        "echo": (sending | receiving, sending | receiving | {"echo_safe": False}),
        "missing": ({"tx_enabled": True, "input": "alsa_input.gone", "output": "bluez_output.gone", "play": "bluez_output.gone",
                     "tx_connected": True,
                     "notes": ["The chosen microphone or line-in isn't connected, so the system default is being used.",
                               "The chosen output to send isn't connected, so the system default is being used.",
                               "The chosen output to play on isn't connected, so the system default is being used."]}, {}),
        "old": ({"rx_other_version": True}, None),
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
                if path == "/api/local":
                    self.reply(world.local(self.node()))
                elif path == "/fake/requests":
                    self.reply(world.requests)
                elif path == "/api/state":
                    self.reply(world.state())
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
                elif self.path.endswith("/api/local/action"):
                    if "audio" in body.get("action", "") or "play_only" in body.get("action", ""):
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
    if action == "set_device_name":
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
        audio[body["role"]] = body["id"]
        audio["tx_enabled"] = audio["input"] != "" or audio["output"] != ""
    elif action == "set_audio_bitrate":
        world.settings[node]["audio"]["bitrate"] = body["kbps"]
    elif action == "set_play_only_on_other":
        world.settings[node]["audio"]["play_only_on_other"] = body["enabled"]
    elif action == "set_audio_rx_muted":
        world.settings[node]["audio"]["rx_muted"] = body["muted"]


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
    options = parser.parse_args()
    world = World(options.monitors, options.name_style == "long", options.no_modes)
    set_audio_state(world, options.audio)
    set_link_state(world, options.link)
    serve(world)
