//! The listener on TCP 24852, and connections to the peer's. Clipboard and the peer API share it:
//! the first thing on a secure stream says which one a connection is for.

use super::keys::Pairing;
use super::stream::{Purpose, SecureStream};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Hands on a connection that has proved it is the paired device.
pub type Incoming = Arc<dyn Fn(SecureStream, Purpose) + Send + Sync>;

/// Connections that have not yet shown the paired key, held at once. More are closed.
const MAX_UNAUTHENTICATED: usize = 4;
const HANDSHAKE_WAIT: Duration = Duration::from_secs(3);

/// Gives a place back when it goes, whichever way the handshake ended.
struct Place(Arc<AtomicUsize>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Listens on `port` for the paired device alone. Each connection gets its own thread, so a long
/// transfer holds nothing up.
pub fn start(port: u16, peer: IpAddr, pairing: Arc<Pairing>, incoming: Incoming) -> Result<(), String> {
    let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).map_err(|e| format!("Couldn't open TCP {port}: {e}"))?;
    println!("[Control] Listening on TCP {port}");
    serve(listener, peer, pairing, incoming)
}

fn serve(listener: TcpListener, peer: IpAddr, pairing: Arc<Pairing>, incoming: Incoming) -> Result<(), String> {
    let peer = peer.to_canonical();
    let unauthenticated = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("mesh-kvm-control".to_string())
        .spawn(move || {
            for tcp in listener.incoming().flatten() {
                // Files land on disk, so the address is checked before a byte is read.
                if tcp.peer_addr().map(|a| a.ip().to_canonical()).ok() != Some(peer) {
                    continue;
                }
                if unauthenticated.fetch_add(1, Ordering::SeqCst) >= MAX_UNAUTHENTICATED {
                    unauthenticated.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let place = Place(Arc::clone(&unauthenticated));
                let pairing = Arc::clone(&pairing);
                let incoming = Arc::clone(&incoming);
                let _ = std::thread::Builder::new().name("mesh-kvm-control-in".to_string()).spawn(move || {
                    let accepted = SecureStream::accept(tcp, &pairing, HANDSHAKE_WAIT);
                    drop(place);
                    if let Ok((stream, purpose)) = accepted {
                        incoming(stream, purpose);
                    }
                });
            }
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A connection to the peer's listener, with the handshake done inside `wait`.
pub fn open(target: SocketAddr, purpose: Purpose, wait: Duration) -> Result<SecureStream, String> {
    open_with(super::services().pairing.as_deref(), target, purpose, wait)
}

fn open_with(pairing: Option<&Pairing>, target: SocketAddr, purpose: Purpose, wait: Duration) -> Result<SecureStream, String> {
    let pairing = pairing.ok_or("not paired")?;
    SecureStream::connect(target, pairing, purpose, wait)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::keys::PeerKey;
    use std::io::Read;
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(5);

    fn paired_pair() -> (Pairing, Pairing) {
        let mut a = Pairing::fresh();
        let mut b = Pairing::fresh();
        a.peer = Some(PeerKey { public: b.identity.public, name: "b".into(), paired_at: 0 });
        b.peer = Some(PeerKey { public: a.identity.public, name: "a".into(), paired_at: 0 });
        (a, b)
    }

    /// A listener on a free port that reports each connection it hands on.
    fn serving(peer: IpAddr, server: Pairing) -> (SocketAddr, mpsc::Receiver<Purpose>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let incoming: Incoming = Arc::new(move |_stream, purpose| {
            let _ = tx.lock().unwrap().send(purpose);
        });
        serve(listener, peer, Arc::new(server), incoming).unwrap();
        (addr, rx)
    }

    #[test]
    fn only_the_peer_address_is_read() {
        let (a, b) = paired_pair();
        let elsewhere: IpAddr = "10.9.9.9".parse().unwrap();
        let (addr, handed) = serving(elsewhere, b);
        let started = Instant::now();
        assert!(open_with(Some(&a), addr, Purpose::Clipboard, WAIT).is_err());
        assert!(started.elapsed() < Duration::from_secs(2), "it was closed at once");
        assert!(handed.try_recv().is_err());
    }

    #[test]
    fn the_peer_is_found_however_its_address_is_written() {
        let (a, b) = paired_pair();
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let (addr, handed) = serving(mapped, b);
        open_with(Some(&a), addr, Purpose::Api, WAIT).unwrap();
        assert_eq!(handed.recv_timeout(WAIT).unwrap(), Purpose::Api);
    }

    #[test]
    fn the_peer_gets_through_with_its_purpose() {
        let (a, b) = paired_pair();
        let (addr, handed) = serving("127.0.0.1".parse().unwrap(), b);
        open_with(Some(&a), addr, Purpose::Clipboard, WAIT).unwrap();
        assert_eq!(handed.recv_timeout(WAIT).unwrap(), Purpose::Clipboard);
    }

    #[test]
    fn a_fifth_silent_connection_is_closed_at_once() {
        let (_, b) = paired_pair();
        let (addr, handed) = serving("127.0.0.1".parse().unwrap(), b);
        let silent: Vec<TcpStream> = (0..MAX_UNAUTHENTICATED).map(|_| TcpStream::connect(addr).unwrap()).collect();
        let mut fifth = TcpStream::connect(addr).unwrap();
        fifth.set_read_timeout(Some(Duration::from_millis(1500))).unwrap();
        let started = Instant::now();
        let got = fifth.read(&mut [0u8; 1]);
        assert!(matches!(got, Ok(0)) || got.is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset));
        assert!(started.elapsed() < Duration::from_millis(1400), "closed without waiting out the handshake");
        assert!(handed.try_recv().is_err());
        drop(silent);
    }

    #[test]
    fn a_connection_that_finishes_its_handshake_frees_its_place() {
        let (a, b) = paired_pair();
        let (addr, handed) = serving("127.0.0.1".parse().unwrap(), b);
        let _silent: Vec<TcpStream> = (0..MAX_UNAUTHENTICATED - 1).map(|_| TcpStream::connect(addr).unwrap()).collect();
        // The next one is opened once the last has been handed on, which is after its place is given back.
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(open_with(Some(&a), addr, Purpose::Clipboard, WAIT).unwrap());
            assert_eq!(handed.recv_timeout(WAIT).unwrap(), Purpose::Clipboard);
        }
        drop(held);
    }

    #[test]
    fn a_stranger_key_gets_no_bytes() {
        let (_, b) = paired_pair();
        let mut stranger = Pairing::fresh();
        stranger.peer = Some(PeerKey { public: b.identity.public, name: "s".into(), paired_at: 0 });
        let (addr, handed) = serving("127.0.0.1".parse().unwrap(), b);
        let err = open_with(Some(&stranger), addr, Purpose::Clipboard, WAIT).err().unwrap();
        assert!(err.contains("closed"), "it saw the connection close, with nothing sent back: {err}");
        assert!(handed.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn without_a_pairing_nothing_is_opened() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(open_with(None, addr, Purpose::Clipboard, WAIT).is_err());
    }
}
