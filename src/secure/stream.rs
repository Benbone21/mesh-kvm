//! An encrypted `Read + Write` over a `TcpStream`: a Noise IK handshake, then frames of a big-endian
//! `u16` length and ciphertext. A frame holding an empty message ends the stream, and a stream that
//! ends any other way is an error, so a cut connection can't pass for a finished transfer.

use super::keys::{Pairing, NOISE_IK};
use snow::{Builder, TransportState};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// The most plaintext in one frame: what Noise takes in a message, less its tag.
pub const MAX_PLAIN: usize = 65519;

const TAG_LEN: usize = 16;
const LEN_PREFIX: usize = 2;
const PROLOGUE: &[u8] = b"mesh-kvm 1 tcp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Clipboard = 1,
    Api = 2,
}

impl Purpose {
    fn from_byte(byte: u8) -> Option<Purpose> {
        match byte {
            1 => Some(Purpose::Clipboard),
            2 => Some(Purpose::Api),
            _ => None,
        }
    }
}

/// What the clipboard and the peer API need of a connection, so they run on either kind.
pub trait Wire: Read + Write + Send {
    /// Says nothing more will be written. The other side reads end of stream.
    fn end_writing(&mut self) -> io::Result<()>;
    fn set_read_wait(&mut self, wait: Option<Duration>) -> io::Result<()>;
    /// How long one write may stall before it fails, so a stuck peer can't hold a sender for ever.
    fn set_write_wait(&mut self, wait: Option<Duration>) -> io::Result<()>;
}

/// For tests that can go either way, plain or secure.
#[cfg(test)]
impl Wire for Box<dyn Wire> {
    fn end_writing(&mut self) -> io::Result<()> {
        (**self).end_writing()
    }

    fn set_read_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        (**self).set_read_wait(wait)
    }

    fn set_write_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        (**self).set_write_wait(wait)
    }
}

impl Wire for TcpStream {
    fn end_writing(&mut self) -> io::Result<()> {
        self.shutdown(std::net::Shutdown::Write)
    }

    fn set_read_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(wait)
    }

    fn set_write_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        self.set_write_timeout(wait)
    }
}

pub struct SecureStream {
    tcp: TcpStream,
    noise: TransportState,
    /// Bytes off the socket that don't yet make a whole frame.
    raw: Vec<u8>,
    /// Decrypted bytes not yet handed out, from `used` on.
    plain: Vec<u8>,
    used: usize,
    /// The end frame came.
    ended: bool,
    /// A frame failed, or a write stopped partway: Noise's counters no longer match the peer's.
    broken: bool,
    /// The end frame was sent.
    closed: bool,
    /// Reads fail once this passes, however the bytes arrive. None: each socket read has its own wait.
    read_deadline: Option<Instant>,
}

fn bad(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "the wait ran out")
}

/// Points the socket's timeouts at what is left until `deadline`.
fn limit(tcp: &TcpStream, deadline: Option<Instant>) -> io::Result<()> {
    if let Some(deadline) = deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timed_out());
        }
        tcp.set_read_timeout(Some(left))?;
        tcp.set_write_timeout(Some(left))?;
    }
    Ok(())
}

/// One whole frame's body. Bytes read so far stay in `raw` if this fails, a timeout included.
pub(super) fn read_frame(tcp: &mut TcpStream, raw: &mut Vec<u8>, deadline: Option<Instant>) -> io::Result<Vec<u8>> {
    loop {
        if let Some(head) = raw.get(..LEN_PREFIX) {
            let len = u16::from_be_bytes([head[0], head[1]]) as usize;
            if let Some(body) = raw.get(LEN_PREFIX..LEN_PREFIX + len) {
                let body = body.to_vec();
                raw.drain(..LEN_PREFIX + len);
                return Ok(body);
            }
        }
        limit(tcp, deadline)?;
        let mut chunk = [0u8; 16 * 1024];
        match tcp.read(&mut chunk) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection closed mid-stream")),
            Ok(n) => raw.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

pub(super) fn write_frame(tcp: &mut TcpStream, body: &[u8], deadline: Option<Instant>) -> io::Result<()> {
    let len = u16::try_from(body.len()).map_err(|_| bad("frame too long"))?;
    let mut out = Vec::with_capacity(LEN_PREFIX + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    limit(tcp, deadline)?;
    tcp.write_all(&out)
}

fn builder<'a>(pairing: &'a Pairing) -> Result<Builder<'a>, String> {
    let params = NOISE_IK.parse().map_err(|_| "bad noise pattern".to_string())?;
    Builder::new(params)
        .local_private_key(&pairing.identity.private)
        .and_then(|b| b.prologue(PROLOGUE))
        .map_err(|e| format!("noise setup: {e}"))
}

fn io_text(what: &str, e: io::Error) -> String {
    format!("{what}: {e}")
}

impl SecureStream {
    /// One limit on everything read from now on, for an answer that must come whole in time.
    pub fn set_read_deadline(&mut self, deadline: Option<Instant>) {
        self.read_deadline = deadline;
    }

    fn new(tcp: TcpStream, noise: TransportState, raw: Vec<u8>) -> SecureStream {
        SecureStream { tcp, noise, raw, plain: Vec::new(), used: 0, ended: false, broken: false, closed: false, read_deadline: None }
    }

    pub fn connect(target: SocketAddr, pairing: &Pairing, purpose: Purpose, wait: Duration) -> Result<SecureStream, String> {
        let peer = pairing.peer.as_ref().ok_or("not paired")?.public;
        let deadline = Instant::now() + wait;
        let mut tcp = TcpStream::connect_timeout(&target, wait).map_err(|e| io_text("connect", e))?;
        let _ = tcp.set_nodelay(true);
        let mut noise = builder(pairing)?
            .remote_public_key(&peer)
            .and_then(|b| b.build_initiator())
            .map_err(|e| format!("noise setup: {e}"))?;
        let mut msg = vec![0u8; 65535];
        let n = noise.write_message(&[], &mut msg).map_err(|e| format!("handshake: {e}"))?;
        write_frame(&mut tcp, msg.get(..n).unwrap_or(&[]), Some(deadline)).map_err(|e| io_text("handshake", e))?;
        let mut raw = Vec::new();
        let reply = read_frame(&mut tcp, &mut raw, Some(deadline)).map_err(|e| io_text("handshake", e))?;
        let mut scratch = vec![0u8; 65535];
        noise.read_message(&reply, &mut scratch).map_err(|e| format!("handshake: {e}"))?;
        if noise.get_remote_static() != Some(&peer[..]) {
            return Err("handshake: wrong key".into());
        }
        let transport = noise.into_transport_mode().map_err(|e| format!("handshake: {e}"))?;
        let mut stream = SecureStream::new(tcp, transport, raw);
        stream.send(&[purpose as u8], Some(deadline)).map_err(|e| io_text("handshake", e))?;
        stream.tcp.set_read_timeout(None).and_then(|_| stream.tcp.set_write_timeout(None)).map_err(|e| io_text("handshake", e))?;
        Ok(stream)
    }

    /// Answers only a handshake from the paired key. Anything else is closed without a byte sent.
    pub fn accept(tcp: TcpStream, pairing: &Pairing, wait: Duration) -> Result<(SecureStream, Purpose), String> {
        let peer = pairing.peer.as_ref().ok_or("not paired")?.public;
        let deadline = Instant::now() + wait;
        let mut tcp = tcp;
        let _ = tcp.set_nodelay(true);
        let mut noise = builder(pairing)?.build_responder().map_err(|e| format!("noise setup: {e}"))?;
        let mut raw = Vec::new();
        let first = read_frame(&mut tcp, &mut raw, Some(deadline)).map_err(|e| io_text("handshake", e))?;
        let mut scratch = vec![0u8; 65535];
        noise.read_message(&first, &mut scratch).map_err(|e| format!("handshake: {e}"))?;
        if noise.get_remote_static() != Some(&peer[..]) {
            return Err("handshake: not the paired key".into());
        }
        let mut msg = vec![0u8; 65535];
        let n = noise.write_message(&[], &mut msg).map_err(|e| format!("handshake: {e}"))?;
        write_frame(&mut tcp, msg.get(..n).unwrap_or(&[]), Some(deadline)).map_err(|e| io_text("handshake", e))?;
        let transport = noise.into_transport_mode().map_err(|e| format!("handshake: {e}"))?;
        let mut stream = SecureStream::new(tcp, transport, raw);
        let purpose = stream.next_plain(Some(deadline)).map_err(|e| io_text("handshake", e))?;
        let purpose = match purpose.as_slice() {
            [byte] => Purpose::from_byte(*byte).ok_or("handshake: unknown purpose")?,
            _ => return Err("handshake: bad purpose frame".into()),
        };
        stream.tcp.set_read_timeout(None).and_then(|_| stream.tcp.set_write_timeout(None)).map_err(|e| io_text("handshake", e))?;
        Ok((stream, purpose))
    }

    /// Seals one message into a whole frame, length first.
    fn seal(&mut self, plain: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; LEN_PREFIX + plain.len() + TAG_LEN];
        let n = self
            .noise
            .write_message(plain, out.get_mut(LEN_PREFIX..).unwrap_or(&mut []))
            .map_err(|_| bad("could not seal"))?;
        out.truncate(LEN_PREFIX + n);
        let len = u16::try_from(n).map_err(|_| bad("frame too long"))?;
        out[..LEN_PREFIX].copy_from_slice(&len.to_be_bytes());
        Ok(out)
    }

    fn send(&mut self, plain: &[u8], deadline: Option<Instant>) -> io::Result<()> {
        let frame = self.seal(plain)?;
        limit(&self.tcp, deadline)?;
        let sent = self.tcp.write_all(&frame);
        if sent.is_err() {
            self.broken = true;
        }
        sent
    }

    /// The next message, opened. Empty means the end frame.
    fn next_plain(&mut self, deadline: Option<Instant>) -> io::Result<Vec<u8>> {
        if self.broken {
            return Err(bad("the stream is broken"));
        }
        let body = read_frame(&mut self.tcp, &mut self.raw, deadline)?;
        if body.len() < TAG_LEN {
            self.broken = true;
            return Err(bad("frame too short"));
        }
        let mut out = vec![0u8; body.len()];
        match self.noise.read_message(&body, &mut out) {
            Ok(n) => {
                out.truncate(n);
                Ok(out)
            }
            Err(_) => {
                self.broken = true;
                Err(bad("frame failed its check"))
            }
        }
    }
}

impl Read for SecureStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.used >= self.plain.len() {
            if self.ended {
                return Ok(0);
            }
            let next = self.next_plain(self.read_deadline)?;
            if next.is_empty() {
                self.ended = true;
                return Ok(0);
            }
            self.plain = next;
            self.used = 0;
        }
        let rest = self.plain.get(self.used..).unwrap_or(&[]);
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.used += n;
        Ok(n)
    }
}

impl Write for SecureStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.broken {
            return Err(bad("the stream is broken"));
        }
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "writing has ended"));
        }
        // An empty frame means the end, so nothing is sent for an empty write.
        for part in buf.chunks(MAX_PLAIN) {
            self.send(part, None)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

impl Wire for SecureStream {
    fn end_writing(&mut self) -> io::Result<()> {
        if !self.closed {
            self.send(&[], None)?;
            self.closed = true;
        }
        self.tcp.shutdown(std::net::Shutdown::Write)
    }

    fn set_read_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        self.tcp.set_read_timeout(wait)
    }

    fn set_write_wait(&mut self, wait: Option<Duration>) -> io::Result<()> {
        self.tcp.set_write_timeout(wait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::keys::PeerKey;
    use std::net::TcpListener;
    use std::thread;

    fn peer_of(public: [u8; 32]) -> Option<PeerKey> {
        Some(PeerKey { public, name: "test".into(), paired_at: 0 })
    }

    fn paired_pair() -> (Pairing, Pairing) {
        let mut a = Pairing::fresh();
        let mut b = Pairing::fresh();
        a.peer = peer_of(b.identity.public);
        b.peer = peer_of(a.identity.public);
        (a, b)
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// Connected streams: the first is the client's, the second the server's.
    fn linked() -> (SecureStream, SecureStream) {
        let (a, b) = paired_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            SecureStream::accept(tcp, &b, WAIT).unwrap()
        });
        let client = SecureStream::connect(addr, &a, Purpose::Api, WAIT).unwrap();
        let (server, purpose) = server.join().unwrap();
        assert_eq!(purpose, Purpose::Api);
        (client, server)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8)) as u8).collect()
    }

    #[test]
    fn bytes_arrive_intact_across_many_frames() {
        let (mut client, mut server) = linked();
        let data = pattern(5 * 1024 * 1024);
        let sent = data.clone();
        let writer = thread::spawn(move || {
            let mut at = 0;
            let mut size = 1;
            while at < sent.len() {
                let end = (at + size).min(sent.len());
                client.write_all(&sent[at..end]).unwrap();
                at = end;
                size = (size * 7 + 13) % 150_000 + 1;
            }
            client.write_all(&sent[..MAX_PLAIN * 2 + 5]).unwrap();
            client.end_writing().unwrap();
        });
        let mut got = Vec::new();
        let mut size = 3;
        let mut buf = vec![0u8; 70_000];
        loop {
            let n = server.read(&mut buf[..size]).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
            size = (size * 5 + 11) % 69_999 + 1;
        }
        writer.join().unwrap();
        assert_eq!(got.len(), data.len() + MAX_PLAIN * 2 + 5);
        assert_eq!(&got[..data.len()], &data[..]);
        assert_eq!(&got[data.len()..], &data[..MAX_PLAIN * 2 + 5]);
    }

    #[test]
    fn the_end_frame_reads_as_eof() {
        let (mut client, mut server) = linked();
        client.write_all(b"hello").unwrap();
        client.end_writing().unwrap();
        let mut got = Vec::new();
        server.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"hello");
        assert_eq!(server.read(&mut [0u8; 4]).unwrap(), 0);
        // The other direction still works after one side stops writing.
        server.write_all(b"back").unwrap();
        let mut back = [0u8; 4];
        client.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"back");
    }

    #[test]
    fn a_connection_cut_without_an_end_frame_is_an_error() {
        let (mut client, mut server) = linked();
        client.write_all(b"partial").unwrap();
        drop(client);
        let mut got = [0u8; 7];
        server.read_exact(&mut got).unwrap();
        let err = server.read(&mut [0u8; 4]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_peer_that_stops_reading_cannot_hold_a_write_for_ever() {
        let (mut client, _server) = linked();
        client.set_write_wait(Some(Duration::from_millis(300))).unwrap();
        let started = Instant::now();
        let chunk = vec![0u8; 64 * 1024];
        let err = loop {
            if let Err(e) = client.write_all(&chunk) {
                break e;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "the write never gave up");
        };
        assert!(matches!(err.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock), "{err:?}");
        assert!(client.write_all(b"more").is_err(), "a half-written frame leaves the stream unusable");
    }

    #[test]
    fn a_changed_frame_is_an_error() {
        let (mut client, mut server) = linked();
        let mut frame = client.seal(b"tamper with me").unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 1;
        client.tcp.write_all(&frame).unwrap();
        let err = server.read(&mut [0u8; 32]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(server.read(&mut [0u8; 32]).is_err());
    }

    #[test]
    fn a_stranger_key_is_closed_without_a_reply() {
        let (a, b) = paired_pair();
        let mut stranger = Pairing::fresh();
        stranger.peer = peer_of(b.identity.public);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            SecureStream::accept(tcp, &b, WAIT).err()
        });
        let mut tcp_seen = Vec::new();
        // Run the stranger's handshake by hand so the bytes the server sends back can be counted.
        let mut raw = TcpStream::connect(addr).unwrap();
        let mut noise = builder(&stranger).unwrap().remote_public_key(&b_public(&stranger)).unwrap().build_initiator().unwrap();
        let mut msg = vec![0u8; 65535];
        let n = noise.write_message(&[], &mut msg).unwrap();
        write_frame(&mut raw, &msg[..n], None).unwrap();
        raw.read_to_end(&mut tcp_seen).unwrap();
        assert!(tcp_seen.is_empty());
        assert!(server.join().unwrap().is_some());
        let _ = a;
    }

    fn b_public(p: &Pairing) -> [u8; 32] {
        p.peer.as_ref().unwrap().public
    }

    #[test]
    fn a_silent_connection_times_out_in_the_wait_given() {
        let (_, b) = paired_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _silent = TcpStream::connect(addr).unwrap();
        let (tcp, _) = listener.accept().unwrap();
        let started = Instant::now();
        assert!(SecureStream::accept(tcp, &b, Duration::from_millis(300)).is_err());
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn an_oversized_length_is_an_error() {
        // The largest length a frame can state still has to hold a tag and pass its check.
        let (mut client, mut server) = linked();
        let mut frame = vec![0xFF, 0xFF];
        frame.extend(pattern(65535));
        client.tcp.write_all(&frame).unwrap();
        assert_eq!(server.read(&mut [0u8; 8]).unwrap_err().kind(), io::ErrorKind::InvalidData);

        let (mut client, mut server) = linked();
        client.tcp.write_all(&[0, 5, 1, 2, 3, 4, 5]).unwrap();
        assert_eq!(server.read(&mut [0u8; 8]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_read_timeout_keeps_partial_frames() {
        let (mut client, mut server) = linked();
        let frame = client.seal(b"in two halves").unwrap();
        let (first, second) = frame.split_at(frame.len() / 2);
        client.tcp.write_all(first).unwrap();
        client.tcp.flush().unwrap();
        server.set_read_wait(Some(Duration::from_millis(100))).unwrap();
        let err = server.read(&mut [0u8; 32]).unwrap_err();
        assert!(matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{err:?}");
        client.tcp.write_all(second).unwrap();
        server.set_read_wait(Some(WAIT)).unwrap();
        let mut got = [0u8; 13];
        server.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"in two halves");
    }
}
