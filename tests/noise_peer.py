#!/usr/bin/env python3
"""A second Noise implementation, so that the Rust side has something to disagree with.

It is Noise_IK_25519_ChaChaPoly_BLAKE2s written from revision 34 of the Noise specification, on
`cryptography` and `hashlib` with no Noise library, and on top of it mesh-kvm's UDP and TCP framing.
`tests/wine_link.py` plays the other device with it. Run on its own, it checks itself against the
two published vectors for this protocol that `snow` ships, cacophony's and its own:

    python3 -I tests/noise_peer.py
"""
import contextlib
import hashlib
import hmac
import json
import os
import secrets
import socket
import struct
import sys
import threading

from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305

PROTOCOL = b"Noise_IK_25519_ChaChaPoly_BLAKE2s"
KEY_LEN = 32
HASH_LEN = 32
TAG_LEN = 16

HANDSHAKE_1, HANDSHAKE_2, DATA_PACKET = 0xE1, 0xE2, 0xE3
INDEX_LEN = 4
DATA_HEADER = 1 + INDEX_LEN + 8
PROLOGUES = {"input": b"mesh-kvm 1 input", "audio": b"mesh-kvm 1 audio", "tcp": b"mesh-kvm 1 tcp"}

CLIPBOARD, API = 1, 2
MAX_PLAIN = 65519

# What `Datagrams.open` makes of a packet, as in `Opened`: the plaintext, something to send back to
# where this came from, a keepalive or a handshake with no answer, or a failed check.
DATA, REPLY, HEARD, REFUSED = "data", "reply", "heard", "refused"


class Refused(Exception):
    """A message that failed a check."""


# ---- The specification's objects, chapter 5 ----

def blake2s(data):
    return hashlib.blake2s(data).digest()


def hkdf(chaining_key, key_material, outputs):
    """Section 4.3, with HMAC over BLAKE2s."""
    temp = hmac.new(chaining_key, key_material, hashlib.blake2s).digest()
    out, last = [], b""
    for i in range(1, outputs + 1):
        last = hmac.new(temp, last + bytes([i]), hashlib.blake2s).digest()
        out.append(last)
    return out


def nonce_bytes(n):
    """Section 12.3, for ChaChaPoly: 32 bits of zeros, then the counter as a little-endian 64 bits."""
    return b"\0" * 4 + struct.pack("<Q", n)


def encrypt(key, n, ad, plain):
    return ChaCha20Poly1305(key).encrypt(nonce_bytes(n), plain, ad)


def decrypt(key, n, ad, sealed):
    try:
        return ChaCha20Poly1305(key).decrypt(nonce_bytes(n), sealed, ad)
    except InvalidTag as e:
        raise Refused("bad tag") from e


def dh(private, public):
    return X25519PrivateKey.from_private_bytes(private).exchange(X25519PublicKey.from_public_bytes(public))


def public_of(private):
    return X25519PrivateKey.from_private_bytes(private).public_key().public_bytes_raw()


class CipherState:
    def __init__(self, key=None):
        self.key, self.n = key, 0

    def encrypt_with_ad(self, ad, plain):
        if self.key is None:
            return plain
        sealed = encrypt(self.key, self.n, ad, plain)
        self.n += 1
        return sealed

    def decrypt_with_ad(self, ad, sealed):
        if self.key is None:
            return sealed
        plain = decrypt(self.key, self.n, ad, sealed)
        self.n += 1
        return plain


class SymmetricState:
    def __init__(self, name):
        self.h = name.ljust(HASH_LEN, b"\0") if len(name) <= HASH_LEN else blake2s(name)
        self.ck = self.h
        self.cipher = CipherState()

    def mix_key(self, key_material):
        self.ck, temp = hkdf(self.ck, key_material, 2)
        self.cipher = CipherState(temp)

    def mix_hash(self, data):
        self.h = blake2s(self.h + data)

    def encrypt_and_hash(self, plain):
        sealed = self.cipher.encrypt_with_ad(self.h, plain)
        self.mix_hash(sealed)
        return sealed

    def decrypt_and_hash(self, sealed):
        plain = self.cipher.decrypt_with_ad(self.h, sealed)
        self.mix_hash(sealed)
        return plain

    def split(self):
        first, second = hkdf(self.ck, b"", 2)
        return CipherState(first), CipherState(second)


class Handshake:
    """The IK pattern:

        <- s
        ...
        -> e, es, s, ss
        <- e, ee, se
    """

    def __init__(self, initiator, prologue, private, remote_public=None, ephemeral=None):
        self.initiator, self.s, self.rs = initiator, private, remote_public
        self.e = ephemeral or X25519PrivateKey.generate().private_bytes_raw()
        self.re = None
        self.symmetric = SymmetricState(PROTOCOL)
        self.symmetric.mix_hash(prologue)
        self.symmetric.mix_hash(remote_public if initiator else public_of(private))

    def write_first(self, payload):
        sym = self.symmetric
        message = public_of(self.e)
        sym.mix_hash(message)
        sym.mix_key(dh(self.e, self.rs))
        message += sym.encrypt_and_hash(public_of(self.s))
        sym.mix_key(dh(self.s, self.rs))
        return message + sym.encrypt_and_hash(payload)

    def read_first(self, message):
        sym = self.symmetric
        if len(message) < KEY_LEN + KEY_LEN + TAG_LEN + TAG_LEN:
            raise Refused("too short")
        self.re, sealed_static = message[:KEY_LEN], message[KEY_LEN:KEY_LEN * 2 + TAG_LEN]
        sym.mix_hash(self.re)
        sym.mix_key(dh(self.s, self.re))
        self.rs = sym.decrypt_and_hash(sealed_static)
        sym.mix_key(dh(self.s, self.rs))
        return sym.decrypt_and_hash(message[KEY_LEN * 2 + TAG_LEN:])

    def write_second(self, payload):
        sym = self.symmetric
        message = public_of(self.e)
        sym.mix_hash(message)
        sym.mix_key(dh(self.e, self.re))
        sym.mix_key(dh(self.e, self.rs))
        return message + sym.encrypt_and_hash(payload)

    def read_second(self, message):
        sym = self.symmetric
        if len(message) < KEY_LEN + TAG_LEN:
            raise Refused("too short")
        self.re = message[:KEY_LEN]
        sym.mix_hash(self.re)
        sym.mix_key(dh(self.e, self.re))
        sym.mix_key(dh(self.s, self.re))
        return sym.decrypt_and_hash(message[KEY_LEN:])

    def split(self):
        """The ciphers to send and to receive with, and the hash that binds the session."""
        first, second = self.symmetric.split()
        pair = (first, second) if self.initiator else (second, first)
        return (*pair, self.symmetric.h)


# ---- mesh-kvm's keys and framing ----

def write_pairing(path, private, public, peer_public, peer_name):
    """A `pairing.json` as `src/secure/keys.rs` writes it, from raw keys."""
    data = {"version": 1, "private_key": private.hex(), "public_key": public.hex(),
            "peer": {"public_key": peer_public.hex(), "name": peer_name, "paired_at": 1}}
    with open(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as f:
        json.dump(data, f, indent=2)


class Keys:
    def __init__(self, private):
        self.private, self.public = private, public_of(private)

    @classmethod
    def make(cls):
        return cls(X25519PrivateKey.generate().private_bytes_raw())

    def write_pairing(self, path, peer_public, name):
        write_pairing(path, self.private, self.public, peer_public, name)


def fingerprint(public_a, public_b):
    """As shown on both devices, for comparing with what they say."""
    d = blake2s(b"mesh-kvm fingerprint 1" + b"".join(sorted((public_a, public_b))))
    return f"{d[0]:02X}{d[1]:02X}-{d[2]:02X}{d[3]:02X}"


class Session:
    """One side of a finished handshake. Each packet carries its counter, so they open in any order."""

    def __init__(self, handshake, ours, theirs):
        sending, receiving, _ = handshake.split()
        self.sending, self.receiving = sending.key, receiving.key
        self.ours, self.theirs = ours, theirs
        self.counter = 0
        self.seen = set()

    def seal(self, plain):
        counter = self.counter
        self.counter += 1
        header = bytes([DATA_PACKET]) + struct.pack(">IQ", self.theirs, counter)
        return header + encrypt(self.sending, counter, b"", plain)

    def open(self, counter, sealed):
        if counter in self.seen:
            raise Refused("seen before")
        plain = decrypt(self.receiving, counter, b"", sealed)
        self.seen.add(counter)
        return plain


class Datagrams:
    """One UDP channel. It initiates when asked to and answers whatever handshakes come, and it owns
    no socket: what `handshake` and `seal` return goes to the other device, and so does a REPLY.

    It keeps every session it has made, as a test doesn't run long enough for that to matter, and
    it has no timers: the caller says when to try again.
    """

    def __init__(self, keys, peer_public, label):
        self.keys, self.peer, self.prologue = keys, peer_public, PROLOGUES[label]
        self.lock = threading.Lock()
        self.sessions = {}
        self.current = None
        self.pending = []
        self.outstanding = {}

    def fresh_index(self):
        while True:
            index = secrets.randbits(32)
            if index not in self.sessions and index not in self.outstanding:
                return index

    def handshake(self):
        """A handshake 1. Only the newest 4 can be answered."""
        with self.lock:
            index = self.fresh_index()
            noise = Handshake(True, self.prologue, self.keys.private, self.peer)
            self.outstanding[index] = noise
            while len(self.outstanding) > 4:
                self.outstanding.pop(next(iter(self.outstanding)))
        return bytes([HANDSHAKE_1]) + noise.write_first(struct.pack(">I", index))

    def ready(self):
        with self.lock:
            return self.current is not None

    def seal(self, plain):
        """The packet for `plain`, or None with no session yet."""
        with self.lock:
            return self.current.seal(plain) if self.current else None

    def open(self, datagram):
        """A (kind, bytes) pair, the kind being DATA, REPLY, HEARD or REFUSED."""
        with self.lock:
            try:
                kind = datagram[0] if datagram else None
                if kind == DATA_PACKET:
                    return self.open_data(datagram)
                if kind == HANDSHAKE_1:
                    return self.answer(datagram)
                if kind == HANDSHAKE_2:
                    return self.finish(datagram)
            except (Refused, ValueError, struct.error):
                pass
            return REFUSED, b""

    def open_data(self, datagram):
        if len(datagram) < DATA_HEADER + TAG_LEN:
            raise Refused("too short")
        index, counter = struct.unpack(">IQ", datagram[1:DATA_HEADER])
        session = self.sessions.get(index)
        if session is None:
            raise Refused("no such session")
        plain = session.open(counter, datagram[DATA_HEADER:])
        if session in self.pending:
            # Rule 4: data proves this wasn't a replayed handshake 1. What came before it goes.
            self.current = session
            self.pending.clear()
            self.outstanding.clear()
        return (DATA, plain) if plain else (HEARD, b"")

    def answer(self, datagram):
        noise = Handshake(False, self.prologue, self.keys.private)
        payload = noise.read_first(datagram[1:])
        if noise.rs != self.peer or len(payload) != INDEX_LEN:
            raise Refused("not the paired key")
        # Rule 5: with both ends trying, the lower key's handshake is the one that goes ahead.
        if self.outstanding:
            if self.keys.public < self.peer:
                return HEARD, b""
            self.outstanding.clear()
        ours = self.fresh_index()
        (theirs,) = struct.unpack(">I", payload)
        reply = bytes([HANDSHAKE_2]) + payload + noise.write_second(struct.pack(">I", ours))
        session = Session(noise, ours, theirs)
        self.sessions[ours] = session
        self.pending.append(session)
        return REPLY, reply

    def finish(self, datagram):
        if len(datagram) < 1 + INDEX_LEN:
            raise Refused("too short")
        (answers,) = struct.unpack(">I", datagram[1:1 + INDEX_LEN])
        noise = self.outstanding.get(answers)
        if noise is None:
            raise Refused("not ours")
        payload = noise.read_second(datagram[1 + INDEX_LEN:])
        if len(payload) != INDEX_LEN:
            raise Refused("no index")
        self.outstanding.clear()
        session = Session(noise, answers, struct.unpack(">I", payload)[0])
        self.sessions[answers] = session
        self.current = session
        return REPLY, session.seal(b"")


class Stream:
    """A secure stream over a connected TCP socket: the IK handshake, then frames of a big-endian
    u16 length and ciphertext. With a purpose this side connected and says what for. With None it
    answers, and `purpose` is what the other side said.
    """

    def __init__(self, sock, keys, peer_public, purpose):
        self.sock, self.raw = sock, b""
        prologue = PROLOGUES["tcp"]
        if purpose is None:
            noise = Handshake(False, prologue, keys.private)
            noise.read_first(self.read_frame())
            if noise.rs != peer_public:
                raise Refused("not the paired key")
            self.write_frame(noise.write_second(b""))
        else:
            noise = Handshake(True, prologue, keys.private, peer_public)
            self.write_frame(noise.write_first(b""))
            noise.read_second(self.read_frame())
        self.sending, self.receiving, _ = noise.split()
        if purpose is None:
            said = self.recv()
            if len(said) != 1:
                raise Refused("no purpose")
            self.purpose = said[0]
        else:
            self.purpose = purpose
            self.send(bytes([purpose]))

    def read_frame(self):
        while True:
            if len(self.raw) >= 2:
                (length,) = struct.unpack(">H", self.raw[:2])
                if len(self.raw) >= 2 + length:
                    body, self.raw = self.raw[2:2 + length], self.raw[2 + length:]
                    return body
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("the connection ended with no end frame")
            self.raw += chunk

    def write_frame(self, body):
        self.sock.sendall(struct.pack(">H", len(body)) + body)

    def send(self, data):
        for at in range(0, len(data), MAX_PLAIN):
            self.write_frame(self.sending.encrypt_with_ad(b"", data[at:at + MAX_PLAIN]))

    def recv(self):
        """The next frame's plaintext. Empty is the end frame."""
        return self.receiving.decrypt_with_ad(b"", self.read_frame())

    def close(self):
        """Sends the end frame, if the connection is still there, and closes it."""
        try:
            self.write_frame(self.sending.encrypt_with_ad(b"", b""))
        except OSError:
            pass
        self.sock.close()


# ---- The self-test ----

VECTORS = json.loads(r"""
{
 "cacophony": {
  "protocol_name": "Noise_IK_25519_ChaChaPoly_BLAKE2s",
  "init_prologue": "4a6f686e2047616c74",
  "init_static": "e61ef9919cde45dd5f82166404bd08e38bceb5dfdfded0a34c8df7ed542214d1",
  "init_ephemeral": "893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a",
  "init_remote_static": "31e0303fd6418d2f8c0e78b91f22e8caed0fbe48656dcf4767e4834f701b8f62",
  "resp_prologue": "4a6f686e2047616c74",
  "resp_static": "4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893",
  "resp_ephemeral": "bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b",
  "handshake_hash": "48f3cb8bc9319da4ba1e9933991b1c4ed4034f1f126a76d3a1fbcfd7f94248d4",
  "messages": [
   {
    "payload": "4c756477696720766f6e204d69736573",
    "ciphertext": "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c79440b03ddc7aac5123d06a1b23b71670e32e76c28239a7ca4ac8f784de7e44c1adbfc6e83fef7352a58d9d56157400c0a737b1d171ce368229c7b752ac25b8faf4eca690f6d896f543be02c996ab2b86b76"
   },
   {
    "payload": "4d757272617920526f746862617264",
    "ciphertext": "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f144808843d9b5a8927f0ac9655ef76833bc7e5561f42e691ac8404efd6fbd6308b6a27c"
   },
   {
    "payload": "462e20412e20486179656b",
    "ciphertext": "2c256ed08fcd08c2980f954ee4beaccb61c9581340f5dd2fd1cf3b"
   },
   {
    "payload": "4361726c204d656e676572",
    "ciphertext": "d6033f70eee20945c7c9dba304e397ee3b284ff5e00fd9efb095d3"
   },
   {
    "payload": "4a65616e2d426170746973746520536179",
    "ciphertext": "a9c068ca5d8babf72560652d8e851adbfac35c8a66e810d560863173e96adf4cfe"
   },
   {
    "payload": "457567656e2042f6686d20766f6e2042617765726b",
    "ciphertext": "2a09d8f459e5927e40fdd2eddc99bdafb04e13a26f145cb5cfe9e6ba34c94331ebc17d5156"
   }
  ]
 },
 "snow": {
  "protocol_name": "Noise_IK_25519_ChaChaPoly_BLAKE2s",
  "init_prologue": "5468657265206973206e6f20726967687420616e642077726f6e672e2054686572652773206f6e6c792066756e20616e6420626f72696e672e",
  "init_static": "b7e117ce8ede06ceb89500799a3778d097fc54a3f90bea744493dfc24ec21f32",
  "init_ephemeral": "cc95b4ccc4912c5a52c8d2f6b808e13712392c4468f4e3f02a7d2d1590cb9178",
  "init_remote_static": "ea82fd2e81d1285f1b2029e46ca7bcaeeeafed15396d002bd434624a4d580655",
  "resp_prologue": "5468657265206973206e6f20726967687420616e642077726f6e672e2054686572652773206f6e6c792066756e20616e6420626f72696e672e",
  "resp_static": "e3187f5c10734e934be3a73c398c7fae07ba3e0cde2fcd9ab03a1c93dcae10f8",
  "resp_ephemeral": "587f83fe736432043d2665fbd47b0506b2cd103b6ba8577f72e117c7ffeb5105",
  "messages": [
   {
    "payload": "910ef4a7c04090f66403fcd8ffaed066e70ed38b576792c4a554cc5016fe5120",
    "ciphertext": "df44a475167152d3e4767b9ad5b28468fb8bd6aaa0b0e7181afd87ede7a8c971e818de301c97393bfa71ac250becfc8acef64a969e039f407fe44c5ffc3d66d94ff57627ad4cc53882a1b2993babac3940edd1eaff34a3d3f08c53570166b3d29d06bd5de3cfb9b85b66001a60a5bb98d72b80da9485655f86db72b46f745c4e"
   },
   {
    "payload": "3ce1e4d6e5f02bfeea1d6620cbf1473b5f55372d6954e98d3e12b6ffd04879d5",
    "ciphertext": "2818be348d38de5f9e3ef545c62e8e276c12e2a64410801a0284e02dfb0d450a848d1001977ef31dfe04b5c824acc04c7bda8490eab6cb6aaa844c4ddf0fda83ec84ed26cd6bee22fab17360b73de5b8"
   },
   {
    "payload": "31e7554ebce419a76bf5cd464e00594ccf55bdc09c234c450850d26ab164238c",
    "ciphertext": "89545eb9b2477d482156692214def7d5aecc00b9244b4435bdb768173841d2a2a2e52bcee32d64fe938c87594b17ee69"
   },
   {
    "payload": "794f643ba08cc7ee7aa39bc055560a0850dbc77bf34d8ab22019bf160ab2c573",
    "ciphertext": "fe35983c049b535cf9ca61cfad5b48e461da90c7c51cb5b873be10e58ae34a384aaebba7f96958b0ef0ac043c5e9c622"
   }
  ]
 }
}
""")


def run_vector(vector):
    b = bytes.fromhex
    prologue = b(vector["init_prologue"])
    initiator = Handshake(True, prologue, b(vector["init_static"]), b(vector["init_remote_static"]), b(vector["init_ephemeral"]))
    responder = Handshake(False, b(vector["resp_prologue"]), b(vector["resp_static"]), None, b(vector["resp_ephemeral"]))
    first, second, *rest = vector["messages"]

    message = initiator.write_first(b(first["payload"]))
    assert message.hex() == first["ciphertext"], "message 1"
    assert responder.read_first(message).hex() == first["payload"]
    assert responder.rs == public_of(b(vector["init_static"]))

    message = responder.write_second(b(second["payload"]))
    assert message.hex() == second["ciphertext"], "message 2"
    assert initiator.read_second(message).hex() == second["payload"]

    i_send, i_receive, i_hash = initiator.split()
    r_send, r_receive, r_hash = responder.split()
    assert i_hash == r_hash
    if "handshake_hash" in vector:
        assert i_hash.hex() == vector["handshake_hash"], "handshake hash"

    # The initiator writes the first message after the handshake, and they take turns.
    for turn, message in enumerate(rest):
        send, receive = (i_send, r_receive) if turn % 2 == 0 else (r_send, i_receive)
        sealed = send.encrypt_with_ad(b"", b(message["payload"]))
        assert sealed.hex() == message["ciphertext"], f"transport message {turn}"
        assert receive.decrypt_with_ad(b"", sealed).hex() == message["payload"]


def exchange(first, second):
    """Handshakes `first` with `second`, as the two sides of one channel."""
    kind, answer = second.open(first.handshake())
    assert kind == REPLY, kind
    kind, keepalive = first.open(answer)
    assert kind == REPLY, kind
    assert not second.ready(), "a handshake 1 alone proves nothing"
    assert second.open(keepalive) == (HEARD, b"")
    assert first.ready() and second.ready()


def datagram_checks():
    a, b = Keys.make(), Keys.make()
    first, second = Datagrams(a, b.public, "input"), Datagrams(b, a.public, "input")
    exchange(first, second)
    packets = [first.seal(bytes([i]) * 40) for i in range(3)]
    for i in (2, 0, 1):
        assert second.open(packets[i]) == (DATA, bytes([i]) * 40), "out of order"
    assert second.open(packets[0])[0] == REFUSED, "a replay"
    assert first.open(second.seal(b"back")) == (DATA, b"back")
    packet = bytearray(first.seal(b"x"))
    for at in range(len(packet)):
        packet[at] ^= 1
        assert second.open(bytes(packet))[0] == REFUSED, f"tampering at {at}"
        packet[at] ^= 1
    assert second.open(first.seal(b"z")) == (DATA, b"z")
    assert first.open(first.seal(b"mine"))[0] == REFUSED, "a reflected packet"

    assert Datagrams(b, Keys.make().public, "input").open(first.handshake())[0] == REFUSED, "the wrong peer"
    assert Datagrams(b, a.public, "audio").open(first.handshake())[0] == REFUSED, "the wrong channel"

    # Both start at once: the lower key's goes ahead, and the two end up on the one session.
    low, high = sorted((a, b), key=lambda k: k.public)
    lower, higher = Datagrams(low, high.public, "audio"), Datagrams(high, low.public, "audio")
    from_lower, from_higher = lower.handshake(), higher.handshake()
    assert lower.open(from_higher) == (HEARD, b"")
    kind, answer = higher.open(from_lower)
    assert kind == REPLY, kind
    kind, keepalive = lower.open(answer)
    assert kind == REPLY, kind
    higher.open(keepalive)
    assert higher.open(lower.seal(b"up")) == (DATA, b"up") and lower.open(higher.seal(b"up")) == (DATA, b"up")


def stream_checks():
    a, b = Keys.make(), Keys.make()
    near, far = socket.socketpair()
    answered = []
    thread = threading.Thread(target=lambda: answered.append(Stream(far, b, a.public, None)))
    thread.start()
    stream = Stream(near, a, b.public, API)
    thread.join()
    other = answered[0]
    assert other.purpose == API
    big = os.urandom(MAX_PLAIN + 10)
    threading.Thread(target=stream.send, args=(big,)).start()
    assert other.recv() + other.recv() == big, "split into frames"
    other.send(b"answer")
    assert stream.recv() == b"answer"
    stream.close()
    assert other.recv() == b"", "the end frame"
    near, far = socket.socketpair()
    threading.Thread(target=lambda: near.close()).start()
    try:
        Stream(far, b, a.public, None)
    except EOFError:
        pass
    else:
        raise AssertionError("a cut connection passed for a finished one")

    near, far = socket.socketpair()
    def intruder():
        with contextlib.suppress(EOFError, OSError):
            Stream(near, Keys.make(), b.public, API)

    thread = threading.Thread(target=intruder)
    thread.start()
    try:
        Stream(far, b, a.public, None)
    except Refused:
        far.close()
    else:
        raise AssertionError("the wrong key got in")
    thread.join()


def self_test():
    for name, vector in VECTORS.items():
        run_vector(vector)
        print(f"PASS: {name}'s {vector['protocol_name']} vector, {len(vector['messages'])} messages", flush=True)
    datagram_checks()
    print("PASS: Datagrams, initiator against responder", flush=True)
    stream_checks()
    print("PASS: Stream, over a socket pair", flush=True)


if __name__ == "__main__":
    sys.exit(self_test())
