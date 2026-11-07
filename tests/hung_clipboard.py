#!/usr/bin/env python3
"""A Wayland clipboard owner that takes a paste request and never answers.

It offers text, and when an app asks for it, keeps the pipe open and writes nothing, as a hung app
does. It talks the wire protocol itself, so it needs nothing installed. Run it with WAYLAND_DISPLAY
pointing at a compositor with a data control protocol, for as long as the hang should last:

    timeout 20 python3 tests/hung_clipboard.py
"""
import array
import os
import socket
import struct


def text(value):
    data = value.encode() + b"\0"
    return struct.pack("<I", len(data)) + data + b"\0" * (-len(data) % 4)


class Wayland:
    def __init__(self):
        path = os.path.join(os.environ["XDG_RUNTIME_DIR"], os.environ["WAYLAND_DISPLAY"])
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.buffer = b""
        self.fds = []
        self.next_id = 2

    def new_id(self):
        self.next_id += 1
        return self.next_id - 1

    def send(self, target, opcode, body=b""):
        self.sock.sendall(struct.pack("<II", target, (len(body) + 8) << 16 | opcode) + body)

    def event(self):
        while True:
            if len(self.buffer) >= 8:
                target, sized = struct.unpack("<II", self.buffer[:8])
                size = sized >> 16
                if len(self.buffer) >= size:
                    body, self.buffer = self.buffer[8:size], self.buffer[size:]
                    return target, sized & 0xFFFF, body
            data, ancillary, _, _ = self.sock.recvmsg(4096, socket.CMSG_SPACE(16 * 4))
            if not data:
                raise SystemExit("the compositor went away")
            self.buffer += data
            for level, kind, fds in ancillary:
                if (level, kind) == (socket.SOL_SOCKET, socket.SCM_RIGHTS):
                    self.fds.extend(array.array("i", fds[: len(fds) // 4 * 4]))

    def roundtrip(self, on_event):
        done = self.new_id()
        self.send(1, 0, struct.pack("<I", done))
        while True:
            target, opcode, body = self.event()
            if target == done:
                return
            on_event(target, opcode, body)


def main():
    wl = Wayland()
    registry = wl.new_id()
    wl.send(1, 1, struct.pack("<I", registry))
    found = {}

    def on_global(target, opcode, body):
        if target == registry and opcode == 0:
            name, length = struct.unpack("<II", body[:8])
            interface = body[8:8 + length - 1].decode()
            found[interface] = (name, struct.unpack("<I", body[8 + (length + 3) // 4 * 4:][:4])[0])

    wl.roundtrip(on_global)
    manager_name = next(n for n in ("ext_data_control_manager_v1", "zwlr_data_control_manager_v1") if n in found)

    def bind(interface, version):
        new = wl.new_id()
        wl.send(registry, 0, struct.pack("<I", found[interface][0]) + text(interface) + struct.pack("<II", version, new))
        return new

    seat, manager = bind("wl_seat", 1), bind(manager_name, 1)
    source, device = wl.new_id(), wl.new_id()
    wl.send(manager, 0, struct.pack("<I", source))
    wl.send(manager, 1, struct.pack("<II", device, seat))
    for mime in ("text/plain;charset=utf-8", "text/plain", "UTF8_STRING"):
        wl.send(source, 0, text(mime))
    wl.send(device, 0, struct.pack("<I", source))
    print("owning the clipboard", flush=True)
    while True:
        target, opcode, _ = wl.event()
        if target == source and opcode == 0:
            print(f"asked, and not answering: {len(wl.fds)} pipes held", flush=True)
        elif target == source and opcode == 1:
            print("replaced, and still holding the pipes", flush=True)


if __name__ == "__main__":
    main()
