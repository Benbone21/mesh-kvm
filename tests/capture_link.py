#!/usr/bin/env python3
"""What someone on the network gets from two paired devices: nothing they can read, and no answer.

Three network namespaces, with device A and device B joined by a veth pair and an `AF_PACKET` socket
on A's end of it, as `tcpdump` isn't installed. There are three runs:

1. A control. Markers go across in the clear through a throwaway UDP echo, and the sniffer has to find
   all of them. The key events are the exact bincode bytes `input::live::whole_device` sealed.
2. The real thing. Two real builds pair with `--pair-show` and `--pair` and call each other's API,
   while `whole_device` types and plays a tone and `clipboard::live::whole_sender` sends text, all
   on the same cable. Each has to arrive, and no marker may show up in the capture.
3. A stranger. Device B goes, and a third namespace takes its address. It tries plain packets, a
   handshake from the wrong key, and TCP to 24852, 24853 and 24880. Any answer fails.

The real builds run hidden from this desk's input and sound, as `wine_link.py` runs them. Everything
else has a recorder for a keyboard, a pretend clipboard, and private sound servers:

    unshare -rnpf --kill-child --mount-proc python3 tests/capture_link.py path/to/test-binary [path/to/mesh-kvm]

The test binary is what `cargo test --release --no-run` prints, and the Linux build comes from the
second argument, then $MESH_KVM_LINUX, then target/release/mesh-kvm. numpy must be importable. Set
AUDIO_TMP as for `netem_link.py` to keep the files somewhere other than /tmp.

A 4-byte magic turns up by chance in about one run in a few thousand here. That reads as a failure,
and the frame it prints shows which it was.
"""
import contextlib
import errno
import json
import os
import random
import secrets
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import audio_devices as audio
import netem_link as netem
import wine_link as wine
from netem_link import check
from noise_peer import CLIPBOARD, DATA_PACKET, HANDSHAKE_1, HANDSHAKE_2, PROLOGUES, Datagrams, Handshake, Keys, Stream

DEVICE_A, DEVICE_B = "10.78.0.1", "10.78.0.2"
INPUT_PORT, AUDIO_PORT, CONTROL_PORT, PAIRING_PORT, API_PORT = 24850, 24851, 24852, 24853, 24880
ECHO_PORT, CLIPBOARD_PORT = 24970, 24971
MAGICS = {"MKVM": b"MKVM", "MKAU": b"MKAU"}
# What comes before the events in a `Presses` packet: magic, sequence, source, kind, newest, sent, count.
PRESSES_HEADER = 4 + 8 + 4 + 4 + 4 + 8 + 8
PRESS_EVENT = 12
TYPING_SECONDS = 8
HANDSHAKE_WAIT = 3


class Namespace:
    def __init__(self):
        self.holder = subprocess.Popen(["unshare", "-n", "sleep", "3600"])
        own = os.readlink("/proc/self/ns/net")
        while os.readlink(f"/proc/{self.holder.pid}/ns/net") == own:
            time.sleep(0.01)
        self.file = os.open(f"/proc/{self.holder.pid}/ns/net", os.O_RDONLY)
        self.around = ["nsenter", f"--net=/proc/{self.holder.pid}/ns/net"]
        self.run("ip", "link", "set", "lo", "up")

    def run(self, *command):
        subprocess.run([*self.around, *command], check=True)

    @contextlib.contextmanager
    def inside(self):
        """Runs a block in this namespace. Sockets stay here once made."""
        here = os.open("/proc/self/ns/net", os.O_RDONLY)
        os.setns(self.file, os.CLONE_NEWNET)
        try:
            yield
        finally:
            os.setns(here, os.CLONE_NEWNET)
            os.close(here)

    def address(self, device, ip):
        self.run("ip", "addr", "add", f"{ip}/24", "dev", device)
        self.run("ip", "link", "set", device, "up")


class Sniffer(threading.Thread):
    """Every frame on A's end of the cable, in both directions."""

    def __init__(self, namespace):
        super().__init__(daemon=True)
        with namespace.inside():
            self.sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(3))
            self.sock.bind(("veth0", 0))
        self.sock.settimeout(0.2)
        self.lock = threading.Lock()
        self.frames = []
        self.dropped = 0
        self.start()

    def run(self):
        while True:
            try:
                frame = self.sock.recv(1 << 16)
            except TimeoutError:
                continue
            except OSError as e:
                if e.errno != errno.ENETDOWN:
                    raise
                continue
            with self.lock:
                self.frames.append(frame)

    def take(self):
        """What came since the last time, and whether any of it was lost."""
        with self.lock:
            frames, self.frames = self.frames, []
        _, dropped = struct.unpack("II", self.sock.getsockopt(263, 6, 8))
        return Capture(frames), dropped


class Capture:
    def __init__(self, frames):
        self.frames = frames
        self.all = b"\0".join(frames)

    def has(self, marker):
        return marker in self.all

    def found(self, markers):
        return [name for name, marker in markers.items() if self.has(marker)]

    def show(self, marker):
        for frame in self.frames:
            if marker in frame:
                return frame.hex()
        return ""


def udp_frames(capture, source):
    """The UDP in `capture` sent from `source`, as (destination port, data), and everything else
    from `source` over IPv4 as (protocol, payload length)."""
    source = socket.inet_aton(source)
    udp, others = [], []
    for frame in capture.frames:
        if len(frame) < 34 or frame[12:14] != b"\x08\x00" or frame[26:30] != source:
            continue
        header = (frame[14] & 15) * 4
        total = struct.unpack(">H", frame[16:18])[0]
        protocol, body = frame[23], frame[14 + header:14 + total]
        if protocol == 17:
            udp.append((struct.unpack(">H", body[2:4])[0], body[8:]))
        elif protocol == 6:
            others.append((protocol, len(body) - (body[12] >> 4) * 4))
        else:
            others.append((protocol, len(body)))
    return udp, others


def key_markers(packets):
    """The packets with keys in them as they were sealed, and each key event in them on its own."""
    markers = {f"key packet {i}": packet for i, packet in enumerate(packets)}
    events = set()
    for packet in packets:
        body = packet[PRESSES_HEADER:]
        events.update(body[at:at + PRESS_EVENT] for at in range(0, len(body) - PRESS_EVENT + 1, PRESS_EVENT))
    markers.update({f"key event {event.hex()}": event for event in events})
    return markers


def typing(a, b, seconds, **env):
    """A person typing on A with B as the target. Returns what got there and what was sealed."""
    def make(me, listen, peer, device_seconds, main):
        return netem.Device(me, listen, peer, device_seconds, main, around=(a, b)[me].around, MK_HOST=(DEVICE_A, DEVICE_B)[me],
                            MK_PEER_HOST=(DEVICE_B, DEVICE_A)[me], MK_SHOW_KEYS="1", **env)

    source, target, _, _ = netem.forwarding_pair(seconds + 8, make=make)
    person = netem.Person(source, target)
    typist = threading.Thread(target=person.typing, args=(random.Random("capture"),), daemon=True)
    typist.start()
    time.sleep(seconds)
    person.running = False
    typist.join()
    person.stop()
    time.sleep(0.5)
    total, in_order, _, _ = person.delivery()
    source.stop()
    target.stop()
    return total, in_order, source.keys_sent


def echoing(namespace):
    """The throwaway echo. Whatever comes in goes back, in the clear."""
    with namespace.inside():
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind((DEVICE_B, ECHO_PORT))

    def echo():
        while True:
            data, sender = sock.recvfrom(65536)
            sock.sendto(data, sender)

    threading.Thread(target=echo, daemon=True).start()


def control_run(a, b, sniffer, words):
    print("\n=== control: the same markers, in the clear", flush=True)
    echoing(b)
    with a.inside():
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sniffer.take()
    for marker in words.values():
        sock.sendto(marker, (DEVICE_B, ECHO_PORT))
    total, in_order, sealed = typing(a, b, TYPING_SECONDS / 2, MK_PLAIN_TO=f"{DEVICE_B}:{ECHO_PORT}")
    time.sleep(0.3)
    capture, dropped = sniffer.take()
    check("the sniffer lost nothing", dropped == 0, str(dropped))
    check("keys were typed", total >= 20 and in_order == total, f"{in_order} of {total}")
    check("the keys were sealed in packets", len(sealed) >= 20, str(len(sealed)))
    keys = key_markers(sealed)
    missed = [name for name, marker in {**words, **keys}.items() if not capture.has(marker)]
    check("the sniffer sees every marker in the clear", not missed, f"{len(keys) + len(words) - len(missed)} of {len(keys) + len(words)}, {missed[:5]}")
    print(f"    {len(capture.frames)} frames, {len(capture.all)} bytes, {len(keys)} key markers, {len(words)} others", flush=True)


class Service(wine.LinuxDevice):
    def __init__(self, binary, namespace, name, peer, role, folder):
        home = os.path.join(folder, f"service_{name}")
        os.makedirs(home)
        self.flags = ("--role", role, "--peer", peer)
        self.namespace = namespace
        super().__init__(binary, namespace.holder, home, open(os.path.join(home, "log"), "wb"))

    def get(self, path):
        """The status and the body, with no opinion of either."""
        with self.namespace.inside():
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{API_PORT}{path}", timeout=3) as r:
                    return r.status, r.read().decode()
            except urllib.error.HTTPError as e:
                return e.code, e.read().decode()

    def call(self, path):
        return json.loads(self.get(path)[1])


def real_pairing(first, second):
    """Returns the code that went by and the fingerprint they agreed on."""
    showing = first.client("--pair-show")
    threading.Timer(60, showing.kill).start()
    code = wine.read_code(showing)
    check("--pair-show prints a code", code is not None)
    done = wine.finish(second.client("--pair", code or ""))
    shown = wine.finish(showing)
    check("the two builds pair", shown[1] == 0 and done[1] == 0, f"{shown}, {done}")
    prints = wine.shown_fingerprint(shown[0]), wine.shown_fingerprint(done[0])
    check("and both print one fingerprint", prints[0] is not None and prints[0] == prints[1], str(prints))
    return code


def clipboard_receiver(namespace, keys, peer_public, got):
    """Plays the far end of a copy with the other Noise implementation, so that the text is seen to
    have crossed. A real receiver would need a display."""
    with namespace.inside():
        listener = socket.socket()
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((DEVICE_B, CLIPBOARD_PORT))
        listener.listen()

    def take():
        tcp, _ = listener.accept()
        tcp.settimeout(10)
        stream = Stream(tcp, keys, peer_public, None)
        check("the copy says it's for the clipboard", stream.purpose == CLIPBOARD, str(stream.purpose))
        data = b""
        while True:
            # Frames of a little-endian u32 length and then bincode: the stamp, and the text.
            at = 0
            frames = []
            while len(data) >= at + 4 and len(data) >= at + 4 + struct.unpack("<I", data[at:at + 4])[0]:
                length = struct.unpack("<I", data[at:at + 4])[0]
                frames.append(data[at + 4:at + 4 + length])
                at += 4 + length
            if len(frames) == 2:
                break
            data += stream.recv()
        got.append(frames[1][12:].decode())
        stream.send(bytes([1]))
        stream.close()

    thread = threading.Thread(target=take, daemon=True)
    thread.start()
    return thread


def send_clipboard(a, b, folder, text):
    keys, far = Keys.make(), Keys.make()
    home = os.path.join(folder, "clipboard_home")
    os.makedirs(os.path.join(home, ".config/mesh-kvm"))
    keys.write_pairing(os.path.join(home, ".config/mesh-kvm/pairing.json"), far.public, "Far")
    got = []
    receiver = clipboard_receiver(b, far, keys.public, got)
    gone = ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR")
    env = {k: v for k, v in os.environ.items() if k not in gone}
    env.update(HOME=home, MK_PEER=f"{DEVICE_B}:{CLIPBOARD_PORT}")
    sender = subprocess.Popen([*a.around, audio.BIN, "--ignored", "--exact", "clipboard::live::whole_sender", "--nocapture"], env=env,
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    time.sleep(1)
    sender.stdin.write(text + "\n")
    sender.stdin.flush()
    receiver.join(10)
    time.sleep(1)
    output, _ = sender.communicate(timeout=10)
    check("the text crossed, through a separate Noise implementation", got == [text], str(got))
    check("and the sender heard it was taken", "NOTE Sent text" in output, output[-300:])


def real_run(a, b, sniffer, words, binary, folder):
    print("\n=== encrypted: two real builds, keys, sound, clipboard and API calls", flush=True)
    first = Service(binary, a, "a", DEVICE_B, "debian", folder)
    second = Service(binary, b, "b", DEVICE_A, "windows", folder)
    first.start()
    second.start()
    check("both builds start unpaired", all((d.wait_for(lambda s: s["secure"] == "unpaired", 20) or {}).get("secure") == "unpaired" for d in (first, second)))
    sniffer.take()
    code = real_pairing(first, second)
    up = [d.wait_for(lambda s: s["secure"] == "encrypted" and s["peer_online"], 30) for d in (first, second)]
    check("the link comes up on both", all(up), str(up and [u and u["secure"] for u in up]))

    status, body = first.get("/api/peer/api/local")
    check("an API call gets the other device's answer", status == 200 and json.loads(body).get("peer_ip") == DEVICE_A, f"{status} {body[:120]}")
    status, back = second.get("/api/peer/api/local")
    check("and one the other way", status == 200 and json.loads(back).get("peer_ip") == DEVICE_B, f"{status} {back[:120]}")
    status, _ = first.get("/api/peer" + words["API path"].decode())
    check("a path that only the other device can turn down gets there", status == 404, str(status))

    sound = [audio.device(name, 24951 + i, 24952 - i, TYPING_SECONDS + 16, around=ns.around, MK_PEER_HOST=peer, MK_CHOOSE_OUTPUT="default")
             for i, (name, ns, peer) in enumerate((("a", a, DEVICE_B), ("b", b, DEVICE_A)))]
    time.sleep(1.5)
    tone = audio.play("a", netem.TONE, TYPING_SECONDS + 12, "tone")
    heard = {}
    listening = threading.Thread(target=lambda: (time.sleep(6), heard.update(pcm=audio.record("b", 3))), daemon=True)
    listening.start()
    total, in_order, sealed = typing(a, b, TYPING_SECONDS)
    check("keys got there, in order", total >= 20 and in_order == total, f"{in_order} of {total}")
    check("the keys were sealed in packets", len(sealed) >= 20, str(len(sealed)))
    send_clipboard(a, b, folder, words["clipboard"].decode())
    listening.join()
    tone.kill()
    tone.wait()
    audio.stop(*sound)
    level = audio.level(heard["pcm"], netem.TONE)
    check("the tone reached the other device", level > -20, f"{level:.1f} dB")

    time.sleep(0.3)
    capture, dropped = sniffer.take()
    check("the sniffer lost nothing", dropped == 0, str(dropped))
    markers = {**words, **key_markers(sealed), "pairing code": code.replace(" ", "").encode(), "pairing code as shown": code.encode(),
               "the API's answer": body[:64].encode(), "the API's answer back": back[:64].encode()}
    seen = capture.found(markers)
    check("no marker is in the capture", not seen, str(seen[:5]))
    for name, magic in MAGICS.items():
        check(f"and no {name}", not capture.has(magic), capture.show(magic)[:200])
    udp, others = udp_frames(capture, DEVICE_A)
    ports = {port: sum(1 for p, _ in udp if p == port) for port in (INPUT_PORT, AUDIO_PORT, netem.PORT_B, 24952)}
    check("and there was plenty to look through", all(n > 0 for n in ports.values()) and len(capture.frames) > 1000, str(ports))
    print(f"    {len(capture.frames)} frames, {len(capture.all)} bytes, {len(markers)} markers, from A by port {ports}, "
          f"TCP segments {sum(1 for p, _ in others if p == 6)}", flush=True)
    return first, second


def listen_for(sock, seconds):
    sock.settimeout(0.1)
    got = []
    end = time.time() + seconds
    while time.time() < end:
        with contextlib.suppress(TimeoutError, ConnectionError):
            got.append(sock.recvfrom(65536))
    return got


def probes(port, wrong_key, public):
    """Everything a stranger has to send a UDP port, which it can read the public key of."""
    label = "input" if port == INPUT_PORT else "audio"
    return [("plain MKVM", wine.key_event(30, True)), ("plain link state", wine.link_state(1)),
            ("plain MKAU", wine.AUDIO_MAGIC + bytes([wine.AUDIO_VERSION]) + bytes(40)),
            ("a handshake from the wrong key", Datagrams(wrong_key, public, label).handshake())]


def stranger_udp(stranger, wrong_key, public):
    for port in (INPUT_PORT, AUDIO_PORT):
        with stranger.inside():
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        for name, data in probes(port, wrong_key, public):
            sock.sendto(data, (DEVICE_A, port))
            got = listen_for(sock, 1)
            check(f"no answer on UDP {port} to {name}", not got, str(got[:2]))
        sock.close()
    # From the peer's own ports too, where A is sending anyway as it looks for its peer.
    for port in (INPUT_PORT, AUDIO_PORT):
        with stranger.inside():
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.bind((DEVICE_B, port))
        for _, data in probes(port, wrong_key, public):
            sock.sendto(data, (DEVICE_A, port))
        got = listen_for(sock, 3)
        answers = [data[:8].hex() for data, _ in got if data[:1] not in (bytes([HANDSHAKE_1]), bytes([DATA_PACKET]))]
        check(f"and from the peer's own port {port}, only A's own search for its peer", not answers, f"{answers[:3]}, of {len(got)}")
        sock.close()


def stranger_tcp(stranger, wrong_key, public):
    noise = Handshake(True, PROLOGUES["tcp"], wrong_key.private, public).write_first(b"")
    for name, data in (("nothing", b""), ("plain MKVM", b"MKVM" + bytes(32)), ("a handshake from the wrong key", struct.pack(">H", len(noise)) + noise)):
        with stranger.inside():
            sock = socket.socket()
        sock.settimeout(HANDSHAKE_WAIT + 1)
        sock.connect((DEVICE_A, CONTROL_PORT))
        sock.sendall(data)
        got = b""
        with contextlib.suppress(TimeoutError, ConnectionError):
            got = sock.recv(4096)
        check(f"no bytes back on TCP {CONTROL_PORT} to {name}", got == b"", got[:16].hex())
        sock.close()
    for port in (PAIRING_PORT, API_PORT):
        with stranger.inside():
            sock = socket.socket()
        sock.settimeout(2)
        result = sock.connect_ex((DEVICE_A, port))
        sock.close()
        check(f"TCP {port} is refused", result == errno.ECONNREFUSED, errno.errorcode.get(result, str(result)))


def stranger_run(a, b, stranger, sniffer, first, second):
    print("\n=== a stranger on the peer's address", flush=True)
    public = bytes.fromhex(first.pairing()["public_key"])
    second.stop()
    down = first.wait_for(lambda s: s["secure"] == "connecting" or not s["peer_online"], 10)
    check("A sees its peer go", bool(down), str(down and (down["secure"], down["peer_online"])))
    b.run("ip", "link", "set", "veth1", "netns", str(stranger.holder.pid))
    stranger.address("veth1", DEVICE_B)
    time.sleep(3)
    sniffer.take()
    wrong_key = Keys.make()
    stranger_udp(stranger, wrong_key, public)
    told = first.call("/api/local")
    check("the plain packets reached A, which shows the note and nothing more", told.get("old_peer") is True, str(told.get("old_peer")))
    stranger_tcp(stranger, wrong_key, public)
    time.sleep(0.3)
    capture, dropped = sniffer.take()
    check("the sniffer lost nothing", dropped == 0, str(dropped))
    udp, others = udp_frames(capture, DEVICE_A)
    answers = [(port, data[:8].hex()) for port, data in udp if port not in (INPUT_PORT, AUDIO_PORT) or data[:1] not in (bytes([HANDSHAKE_1]), bytes([DATA_PACKET]))]
    check("the capture shows no answer over UDP", not answers, str(answers[:3]))
    check("no handshake was answered", not any(data[:1] == bytes([HANDSHAKE_2]) for _, data in udp))
    check("no TCP payload left A", not any(protocol == 6 and length > 0 for protocol, length in others), str([o for o in others if o[1]][:3]))
    check("no ICMP left A", not any(protocol == 1 for protocol, _ in others), str(others[:3]))
    print(f"    from A in {len(capture.frames)} frames: {len(udp)} UDP, of which {sum(1 for _, d in udp if d[:1] == bytes([HANDSHAKE_1]))} its own handshakes, "
          f"{len(others)} others", flush=True)
    # The session with the real peer stays until it ages out, so `secure` still says encrypted.
    st = first.call("/api/local")
    check("A is up, and the stranger didn't bring its link back", st.get("peer_online") is False, str(st.get("peer_online")))


def main():
    audio.BIN = sys.argv[1]
    binary = sys.argv[2] if len(sys.argv) > 2 else os.environ.get("MESH_KVM_LINUX", wine.DEFAULT_LINUX)
    for path in (audio.BIN, binary):
        if not os.path.exists(path):
            sys.exit(f"not found: {path}")
    subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
    audio.BASE = tempfile.mkdtemp(prefix="mkcap.", dir=os.environ.get("AUDIO_TMP", "/tmp"))
    a, b, stranger = Namespace(), Namespace(), Namespace()
    services = []
    try:
        a.run("ip", "link", "add", "veth0", "type", "veth", "peer", "name", "veth1")
        a.run("ip", "link", "set", "veth1", "netns", str(b.holder.pid))
        a.address("veth0", DEVICE_A)
        b.address("veth1", DEVICE_B)
        audio.start_sound_server("a")
        audio.start_sound_server("b")
        sniffer = Sniffer(a)
        mark = secrets.token_hex(8)
        words = {"clipboard": f"mesh-kvm capture clipboard {mark}".encode(), "API path": f"/api/capture-{mark}".encode(),
                 "API route": b"/api/local", "API answer": b'"secure":"', "the device name": socket.gethostname().encode(), **MAGICS}
        control_run(a, b, sniffer, words)
        services = real_run(a, b, sniffer, words, binary, audio.BASE)
        stranger_run(a, b, stranger, sniffer, *services)
    finally:
        for service in services:
            service.stop()
        for namespace in (a, b, stranger):
            namespace.holder.kill()
        shutil.rmtree(audio.BASE, ignore_errors=True)
    print("ALL PASS" if netem.failures == 0 else f"{netem.failures} FAILED", flush=True)
    sys.exit(1 if netem.failures else 0)


if __name__ == "__main__":
    main()
