#!/usr/bin/env python3
"""A pretend mesh-kvm service, for looking at the app with no real one running.

It answers on the app's port with two devices and however many monitors you ask for, and it plays
the mode change countdown the way the real service does. Run it where that port is free:

    unshare -rn bash -c 'ip link set lo up && python3 -I tests/fake_service.py --monitors 5 & \
        target/release/mesh-kvm --app'

--name-style long tries names that don't fit.
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
        telemetry = {"active_node": self.active, "peer_online": True, "panic_mode": False, "cursor_x": 2600, "cursor_y": 700,
                     "peer_addr": "192.168.8.241:24850", "game_captive_active": False, "game_reason": "", "main_conflict": False}
        return {"config": config, "telemetry": telemetry}

    def local(self, node):
        change = self.change[node]
        status = {
            "node": node.lower(), "device_name": self.names[node], "hostname": "bread" if node == "Debian" else "THINKPAD",
            "ip": "192.168.8.210" if node == "Debian" else "192.168.8.241", "peer_ip": "192.168.8.241", "paired": True,
            "version": "0.3.0", "os": "", "label": "", "peer_online": True, "active_node": self.active, "panic_mode": False,
            "game_lock": False, "game_reason": "", "displays": self.rects(node), "cursor": [2600, 700], "autostart": True,
            "peer_pinned": False, "main_device": 0, "main_conflict": False,
            "display_change": {"change": None, "note": self.note[node]},
        }
        status.update(self.settings[node])
        if change:
            left = max(0, int((change["until"] - time.time()) * 1000))
            status["display_change"]["change"] = {k: change[k] for k in ("id", "output", "name", "from", "to")} | {"stage": "waiting", "ms_left": left}
        return status


def default_settings():
    return {
        "sharing": {"mouse": True, "keyboard": True},
        "clipboard": {"send": True, "receive": True, "note": "Copied 12 characters from the other device."},
        "pointer_speed": 0.75,
        "audio": {"tx_enabled": False, "tx_volume": 100, "tx_peak": 0, "tx_source": 0, "mute_while_sending": True,
                  "rx_muted": False, "rx_volume": 100, "rx_peak": 45},
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
                if path == "/api/local":
                    self.reply(world.local(self.node()))
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
                elif self.path.endswith("/api/local/action"):
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
    options = parser.parse_args()
    serve(World(options.monitors, options.name_style == "long", options.no_modes))
