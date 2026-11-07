//! The listener on TCP 24852, and connections to the peer's. Clipboard and the peer API share it:
//! the first thing on a secure stream says which one a connection is for.

use super::keys::Pairing;
use super::stream::{Purpose, SecureStream, Wire};
use crate::webui::{self, Handler, HttpRequest};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// The most one request or answer may hold. Longer is refused before anything is allocated.
const MAX_FRAME: usize = 4 * 1024 * 1024;
/// To connect, the handshake included.
const CONNECT_WAIT: Duration = Duration::from_millis(350);
const ANSWER_WAIT: Duration = Duration::from_millis(1200);
const WRITE_WAIT: Duration = Duration::from_millis(600);
/// The connection is replaced after this, so its keys rotate too.
const HOUR: Duration = Duration::from_secs(3600);
/// How long the serving side waits for a request. The caller replaces its connection every hour,
/// so one that is quiet for longer than this has gone.
const IDLE_WAIT: Duration = Duration::from_secs(2 * 3600);

#[derive(Serialize, Deserialize)]
struct ApiRequest {
    method: String,
    path: String,
    body: String,
}

#[derive(Serialize, Deserialize)]
struct ApiAnswer {
    status: u16,
    body: String,
}

fn bad(text: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, text)
}

/// A `u32` length, then bincode.
fn write_msg<T: Serialize>(wire: &mut impl Write, msg: &T) -> Result<(), String> {
    let body = bincode::serialize(msg).map_err(|e| e.to_string())?;
    if body.len() > MAX_FRAME {
        return Err("the message is too large".into());
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    wire.write_all(&out).and_then(|_| wire.flush()).map_err(|e| e.to_string())
}

fn read_msg<T: for<'a> Deserialize<'a>>(wire: &mut impl Read) -> std::io::Result<T> {
    let mut len = [0u8; 4];
    wire.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(bad("the message is too large"));
    }
    let mut body = Vec::new();
    wire.take(len as u64).read_to_end(&mut body)?;
    if body.len() != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    bincode::deserialize(&body).map_err(|_| bad("not a message"))
}

/// Answers the paired device's requests on `stream` until it ends. The requests come in as
/// `Source::Peer`: this is the only place that makes one.
pub fn serve_api(mut stream: SecureStream, handler: Handler) {
    let _ = stream.set_write_wait(Some(WRITE_WAIT));
    let _ = stream.set_read_wait(Some(IDLE_WAIT));
    while let Ok(request) = read_msg::<ApiRequest>(&mut stream) {
        let request = HttpRequest::from_peer(&request.method, &request.path, &request.body);
        let (status, body) = webui::answer(&request, &handler);
        if write_msg(&mut stream, &ApiAnswer { status, body }).is_err() {
            return;
        }
    }
}

struct ApiConnection {
    stream: SecureStream,
    opened: Instant,
}

/// The one connection requests to the peer go over, and whether it is still good.
struct Link {
    connection: Mutex<Option<ApiConnection>>,
    max_age: Duration,
}

impl Link {
    const fn new(max_age: Duration) -> Link {
        Link { connection: Mutex::new(None), max_age }
    }

    /// One request, with one in flight at a time. A failure drops the connection and gives the error:
    /// the call isn't tried again, as a change must not be sent twice, and the next one reconnects.
    fn call(&self, pairing: Option<&Pairing>, target: Option<SocketAddr>, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
        let request = ApiRequest { method: method.into(), path: path.into(), body: body.into() };
        let size = bincode::serialized_size(&request).map_err(|e| e.to_string())?;
        if size > MAX_FRAME as u64 {
            return Err("the message is too large".into());
        }
        let mut slot = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|c| c.opened.elapsed() >= self.max_age) {
            *slot = None;
        }
        if slot.is_none() {
            let target = target.ok_or("no peer")?;
            let stream = open_with(pairing, target, Purpose::Api, CONNECT_WAIT)?;
            *slot = Some(ApiConnection { stream, opened: Instant::now() });
        }
        let Some(connection) = slot.as_mut() else { return Err("no connection".into()) };
        let result = exchange(&mut connection.stream, &request);
        if result.is_err() {
            *slot = None;
        }
        result
    }
}

fn exchange(stream: &mut SecureStream, request: &ApiRequest) -> Result<(u16, String), String> {
    stream.set_write_wait(Some(WRITE_WAIT)).map_err(|e| e.to_string())?;
    stream.set_read_wait(Some(ANSWER_WAIT)).map_err(|e| e.to_string())?;
    write_msg(stream, request).map_err(|e| format!("peer unreachable: {e}"))?;
    let answer: ApiAnswer = read_msg(stream).map_err(|e| format!("peer didn't answer: {e}"))?;
    Ok((answer.status, answer.body))
}

static TARGET: OnceLock<SocketAddr> = OnceLock::new();
static PEER_LINK: Link = Link::new(HOUR);

/// Where the peer's control listener is. Until it's set, the peer can't be called.
pub fn set_target(target: SocketAddr) {
    let _ = TARGET.set(target);
}

/// A request to the other device's API. Fails when this device isn't paired.
pub fn peer_call(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    PEER_LINK.call(super::services().pairing.as_deref(), TARGET.get().copied(), method, path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::keys::PeerKey;
    use std::io::{Read, Write};
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

    /// A listener that serves the peer API, counting the connections it takes. The first
    /// `drop_first` are closed as soon as they are handed on.
    fn serving_api(server: Pairing, drop_first: usize) -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let handler: webui::Handler = Arc::new(|req| match req.path.as_str() {
            "/api/state" => Some(format!(r#"{{"method":"{}","len":{}}}"#, req.method, req.body.len())),
            _ => None,
        });
        let incoming: Incoming = Arc::new(move |stream, purpose| {
            if seen.fetch_add(1, Ordering::SeqCst) < drop_first {
                return;
            }
            if purpose == Purpose::Api {
                serve_api(stream, Arc::clone(&handler));
            }
        });
        serve(listener, "127.0.0.1".parse().unwrap(), Arc::new(server), incoming).unwrap();
        (addr, count)
    }

    #[test]
    fn calls_share_one_connection() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 0);
        let link = Link::new(HOUR);
        for n in 0..3 {
            let (status, body) = link.call(Some(&a), Some(addr), "POST", "/api/state", &"x".repeat(n)).unwrap();
            assert_eq!(status, 200);
            assert_eq!(body, format!(r#"{{"method":"POST","len":{n}}}"#));
        }
        assert_eq!(link.call(Some(&a), Some(addr), "GET", "/api/other", "").unwrap().0, 404);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_dropped_connection_reconnects_on_the_next_call() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 1);
        let link = Link::new(HOUR);
        assert!(link.call(Some(&a), Some(addr), "POST", "/api/state", "").is_err(), "the failed call is not sent again");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(link.call(Some(&a), Some(addr), "POST", "/api/state", "").unwrap().0, 200);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_connection_is_replaced_after_an_hour() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 0);
        let link = Link::new(Duration::ZERO);
        link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap();
        link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_body_over_4_mib_is_refused() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 0);
        let link = Link::new(HOUR);
        let big = "x".repeat(MAX_FRAME);
        let err = link.call(Some(&a), Some(addr), "POST", "/api/state", &big).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 0, "nothing was sent");
        // A frame that claims more is closed on, before anything is allocated for it.
        let mut raw = open_with(Some(&a), addr, Purpose::Api, WAIT).unwrap();
        raw.write_all(&((MAX_FRAME as u32) + 1).to_be_bytes()).unwrap();
        raw.set_read_wait(Some(WAIT)).unwrap();
        let mut byte = [0u8; 1];
        assert!(matches!(raw.read(&mut byte), Ok(0) | Err(_)));
    }

    #[test]
    fn a_stale_connection_does_not_hold_up_a_new_one() {
        let (a, b) = paired_pair();
        let (addr, _) = serving_api(b, 0);
        let _idle = open_with(Some(&a), addr, Purpose::Api, WAIT).unwrap();
        let link = Link::new(HOUR);
        assert_eq!(link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap().0, 200);
    }

    #[test]
    fn without_a_pairing_or_a_target_a_call_is_an_error() {
        let (a, _) = paired_pair();
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let link = Link::new(HOUR);
        assert!(link.call(None, Some(addr), "GET", "/api/state", "").is_err());
        assert!(link.call(Some(&a), None, "GET", "/api/state", "").is_err());
    }

    #[test]
    fn without_a_pairing_nothing_is_opened() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(open_with(None, addr, Purpose::Clipboard, WAIT).is_err());
    }
}
