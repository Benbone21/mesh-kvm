#!/usr/bin/env python3
"""End-to-end link test: the Windows exe under Wine, driven by hand-made UDP packets.

Run inside a network namespace so the daemon's ports are free and loopback aliases work:

    unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_link.py [path/to/mesh-kvm.exe]'

The exe path comes from the first argument, then $MESH_KVM_EXE, then the default below.
"""
import json
import os
import pathlib
import shutil
import socket
import struct
import subprocess
import sys
import time
import urllib.request

DEFAULT_EXE = str(pathlib.Path(__file__).resolve().parent.parent
                  / "target/x86_64-pc-windows-gnu/release/mesh-kvm.exe")
PREFIX = "/tmp/claude-1000/mk/wineprefix"
PEER = "127.0.0.2"
STRANGER = "127.0.0.3"
INPUT_PORT = 24850
API = "http://127.0.0.1:24880/api/local"

DEBIAN, WINDOWS = 0, 1
HEARTBEAT, FOCUS_SWITCH_AT, ACTIVE_VIEW = 7, 10, 12

failures = 0
seq = 0


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


def sender(ip):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((ip, 0))
    return s


def send(sock, data):
    sock.sendto(data, ("127.0.0.1", INPUT_PORT))


def local():
    with urllib.request.urlopen(API, timeout=2) as r:
        return json.load(r)


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


def pump(sock, seconds, *builders):
    """Send the given packets every 200 ms for a while."""
    end = time.time() + seconds
    while time.time() < end:
        for b in builders:
            send(sock, b())
        time.sleep(0.2)


def drive_to_debian(peer, timeout=3):
    """Send heartbeats and Debian views until active_node is Debian; return the last state seen.
    A slow or failed /api/local read counts as no state, so the check fails instead of raising."""
    stop = time.time() + timeout
    st = None
    while time.time() < stop:
        send(peer, heartbeat())
        send(peer, active_view(DEBIAN))
        try:
            st = local()
        except Exception:
            st = None
        else:
            if st.get("active_node") == "Debian":
                break
        time.sleep(0.2)
    return st


def view(st):
    return (st or {}).get("peer_online"), (st or {}).get("active_node")


def run():
    exe = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("MESH_KVM_EXE", DEFAULT_EXE)
    if not os.path.exists(exe):
        sys.exit(f"exe not found: {exe}")
    shutil.rmtree(PREFIX, ignore_errors=True)
    os.makedirs(PREFIX, exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k not in ("DISPLAY", "WAYLAND_DISPLAY")}
    env.update(WINEPREFIX=PREFIX, WINEDEBUG="-all", WINEDLLOVERRIDES="mscoree,mshtml=")
    log = open(os.path.join(os.path.dirname(PREFIX), "wine_link.log"), "w")
    proc = subprocess.Popen(["wine", exe, "--role", "windows", "--peer", PEER],
                            env=env, stdout=log, stderr=subprocess.STDOUT)
    try:
        st = wait_for(lambda s: True, 90)
        if st is None:
            check("daemon answers on /api/local", False, "see wine_link.log")
            return 1
        peer, stranger = sender(PEER), sender(STRANGER)

        # a. Packets from an unknown address are ignored.
        pump(stranger, 1.0, heartbeat, lambda: active_view(DEBIAN))
        online, active = view(wait_for(lambda s: True, 2))
        check("packets from 127.0.0.3 leave peer_online false, active Windows",
              online is False and active == "Windows", f"{online}, {active}")

        # b. Heartbeats alone bring the link up, but the laptop stays active.
        send(peer, heartbeat())
        st = wait_for(lambda s: s["peer_online"], 3)
        online, active = view(st)
        check("heartbeats from 127.0.0.2 make peer_online true, active Windows",
              online is True and active == "Windows", f"{online}, {active}")

        # c. An ActiveView naming Debian hands the view over.
        st = drive_to_debian(peer)
        check("ActiveView Debian gives active_node Debian", view(st) == (True, "Debian"), str(view(st)))

        # d. Heartbeats without views: one-way link fallback after 3 s.
        pump(peer, 3.0, heartbeat)
        online, active = view(wait_for(lambda s: True, 2))
        check("3 s of heartbeats with no views: online, back to Windows",
              online is True and active == "Windows", f"{online}, {active}")

        # e. A view again, then silence drops the link.
        st = drive_to_debian(peer)
        check("ActiveView Debian again gives Debian", view(st) == (True, "Debian"), str(view(st)))
        time.sleep(2.0)
        st = wait_for(lambda s: not s["peer_online"], 1.5)
        online, active = view(st)
        check("2 s of silence: peer_online false, active Windows",
              online is False and active == "Windows", f"{online}, {active}")

        # f. A focus switch to Windows warps the cursor.
        send(peer, heartbeat())
        send(peer, focus_switch_at(WINDOWS, 100, 200))
        st = wait_for(lambda s: s["cursor"] == [100, 200], 2)
        cur = st["cursor"] if st else None
        check("FocusSwitchAt Windows 100,200 puts the cursor at 100,200", cur == [100, 200], f"cursor {cur}")
    finally:
        proc.kill()
        try:
            proc.wait(timeout=5)
        except Exception:
            pass
        subprocess.run(["wineserver", "-k"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return 1 if failures else 0


if __name__ == "__main__":
    code = run()
    print("ALL PASS" if code == 0 else f"{failures} FAILED")
    sys.exit(code)
