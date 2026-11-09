//! The listener on TCP 24852, and connections to the peer's. Clipboard and the peer API share it:
//! the first thing on a secure stream says which one a connection is for.

use super::keys::Pairing;
use super::stream::{Purpose, SecureStream, Wire};
use crate::webui::{self, Handler, HttpRequest};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
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
    let bind_addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = crate::app::launch::bind_patiently(|| TcpListener::bind(bind_addr)).map_err(|e| format!("Couldn't open TCP {port}: {e}"))?;
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
/// For a request that matters enough to wait out a slow link, like installing an update.
const PATIENT_CONNECT_WAIT: Duration = Duration::from_secs(3);
const PATIENT_ANSWER_WAIT: Duration = Duration::from_secs(10);
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

/// What the link log says of the connection: not yet used, or how it went last.
const UNUSED: u8 = 0;
const UP: u8 = 1;
const DOWN: u8 = 2;

/// The one connection requests to the peer go over, and whether it is still good.
struct Link {
    connection: Mutex<Option<ApiConnection>>,
    max_age: Duration,
    state: AtomicU8,
}

impl Link {
    const fn new(max_age: Duration) -> Link {
        Link { connection: Mutex::new(None), max_age, state: AtomicU8::new(UNUSED) }
    }

    /// Logs `control_up` and `control_down` as the connection changes, and nothing in between.
    fn note(&self, result: Result<(), &str>, target: Option<SocketAddr>) {
        let now = if result.is_ok() { UP } else { DOWN };
        if self.state.swap(now, Ordering::Relaxed) == now {
            return;
        }
        let to = target.map_or("-".to_string(), |t| t.to_string());
        let details = crate::linklog::Details::new().text("to", &to);
        match result {
            Ok(()) => crate::linklog::record("control_up", details),
            Err(error) => crate::linklog::record("control_down", details.text("error", error)),
        }
    }

    fn state_text(&self) -> &'static str {
        match self.state.load(Ordering::Relaxed) {
            UP => "up",
            DOWN => "down",
            _ => "unused",
        }
    }

    /// One request, with one in flight at a time. A failure drops the connection and gives the error:
    /// the call isn't tried again, as a change must not be sent twice, and the next one reconnects.
    fn call(&self, pairing: Option<&Pairing>, target: Option<SocketAddr>, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
        self.call_within(pairing, target, method, path, body, CONNECT_WAIT, ANSWER_WAIT)
    }

    /// `call` with its own waits for connecting and for the answer.
    #[allow(clippy::too_many_arguments)]
    fn call_within(
        &self,
        pairing: Option<&Pairing>,
        target: Option<SocketAddr>,
        method: &str,
        path: &str,
        body: &str,
        connect: Duration,
        answer: Duration,
    ) -> Result<(u16, String), String> {
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
            let stream = open_with(pairing, target, Purpose::Api, connect).inspect_err(|e| self.note(Err(e), Some(target)))?;
            self.note(Ok(()), Some(target));
            *slot = Some(ApiConnection { stream, opened: Instant::now() });
        }
        let Some(connection) = slot.as_mut() else { return Err("no connection".into()) };
        let result = exchange(&mut connection.stream, &request, answer);
        if let Err(e) = &result {
            *slot = None;
            self.note(Err(e), target);
        }
        result
    }
}

fn exchange(stream: &mut SecureStream, request: &ApiRequest, wait: Duration) -> Result<(u16, String), String> {
    stream.set_write_wait(Some(WRITE_WAIT)).map_err(|e| e.to_string())?;
    write_msg(stream, request).map_err(|e| format!("peer unreachable: {e}"))?;
    // One limit for the whole answer, so a trickle can't hold the link.
    stream.set_read_deadline(Some(Instant::now() + wait));
    let answer = read_msg::<ApiAnswer>(stream).map_err(|e| format!("peer didn't answer: {e}"));
    stream.set_read_deadline(None);
    let answer = answer?;
    Ok((answer.status, answer.body))
}

static TARGET: OnceLock<SocketAddr> = OnceLock::new();
static PEER_LINK: Link = Link::new(HOUR);

/// Where the peer's control listener is. Until it's set, the peer can't be called.
pub fn set_target(target: SocketAddr) {
    let _ = TARGET.set(target);
}

/// The connection to the peer's API as it last went: `up`, `down`, or `unused` while nothing has
/// needed it. Nothing tries it just to find out.
pub fn control_state() -> &'static str {
    PEER_LINK.state_text()
}

/// A request to the other device's API. Fails when this device isn't paired.
pub fn peer_call(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    PEER_LINK.call(super::services().pairing.as_deref(), TARGET.get().copied(), method, path, body)
}

/// `peer_call` for a request that matters enough to wait out a slow link.
pub fn peer_call_patiently(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    let pairing = super::services().pairing.as_deref();
    PEER_LINK.call_within(pairing, TARGET.get().copied(), method, path, body, PATIENT_CONNECT_WAIT, PATIENT_ANSWER_WAIT)
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

    /// What a reconnect takes against the plain connect it replaced, which is `TcpStream::connect_timeout`
    /// with the same limit. `tests/netem_link.py` puts this on a lossy link.
    #[test]
    #[ignore = "run by tests/netem_link.py"]
    fn reconnects_against_a_plain_connect() {
        let tries = std::env::var("MK_TRIES").ok().and_then(|n| n.parse().ok()).unwrap_or(200);
        let (a, b) = paired_pair();
        let (secure, _handed) = serving("127.0.0.1".parse().unwrap(), b.clone());
        // Whole calls too, as a reconnect now happens only after a failure.
        let api_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = api_listener.local_addr().unwrap();
        let handler: Handler = Arc::new(|_| Some("{}".to_string()));
        let incoming: Incoming = Arc::new(move |stream, _| serve_api(stream, handler.clone()));
        serve(api_listener, "127.0.0.1".parse().unwrap(), Arc::new(b), incoming).unwrap();
        let link = Link::new(HOUR);
        let mut failed_calls = 0;
        let plain_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let plain = plain_listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for connection in plain_listener.incoming() {
                drop(connection);
            }
        });
        let timed = |connect: &mut dyn FnMut() -> bool| {
            let started = Instant::now();
            let made = connect();
            (made, started.elapsed().as_secs_f64() * 1000.0)
        };
        let (mut secure_times, mut plain_times) = (Vec::new(), Vec::new());
        let (mut secure_misses, mut plain_misses) = (0, 0);
        for _ in 0..tries {
            let (made, took) = timed(&mut || open_with(Some(&a), secure, Purpose::Api, CONNECT_WAIT).is_ok());
            secure_misses += u32::from(!made);
            if made {
                secure_times.push(took);
            }
            let (made, took) = timed(&mut || TcpStream::connect_timeout(&plain, CONNECT_WAIT).is_ok());
            plain_misses += u32::from(!made);
            if made {
                plain_times.push(took);
            }
            failed_calls += u32::from(link.call(Some(&a), Some(api), "GET", "/api/state", "").is_err());
            // As often as the app asks.
            std::thread::sleep(Duration::from_millis(200));
        }
        let middle = |times: &mut Vec<f64>| {
            times.sort_by(f64::total_cmp);
            times.get(times.len() / 2).copied().unwrap_or(0.0)
        };
        println!(
            "RECONNECT {}",
            serde_json::json!({
                "tries": tries,
                "secure_misses": secure_misses,
                "plain_misses": plain_misses,
                "failed_calls": failed_calls,
                "secure_median_ms": middle(&mut secure_times),
                "plain_median_ms": middle(&mut plain_times),
                "secure_most_ms": secure_times.last(),
                "plain_most_ms": plain_times.last(),
            })
        );
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
    fn the_connection_is_said_to_be_up_or_down_as_it_last_went() {
        let (a, b) = paired_pair();
        let (addr, _) = serving_api(b, 1);
        let link = Link::new(HOUR);
        assert_eq!(link.state_text(), "unused");
        assert!(link.call(Some(&a), Some(addr), "GET", "/api/state", "").is_err(), "the first is closed on it");
        assert_eq!(link.state_text(), "down");
        link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap();
        assert_eq!(link.state_text(), "up");
        let nobody = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let fresh = Link::new(HOUR);
        assert!(fresh.call(Some(&a), Some(nobody), "GET", "/api/state", "").is_err());
        assert_eq!(fresh.state_text(), "down", "a connect that fails");
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
    }

    #[test]
    fn a_frame_that_claims_over_4_mib_is_refused_before_it_is_read() {
        let too_long = ((MAX_FRAME + 1) as u32).to_be_bytes();
        let err = read_msg::<ApiRequest>(&mut std::io::Cursor::new(too_long.to_vec())).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let exact = (MAX_FRAME as u32).to_be_bytes();
        let err = read_msg::<ApiRequest>(&mut std::io::Cursor::new(exact.to_vec())).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "the limit itself is allowed");
    }

    #[test]
    fn an_answer_that_trickles_fails_at_the_deadline_and_the_next_call_reconnects() {
        let (a, b) = paired_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let incoming: Incoming = Arc::new(move |mut stream, _| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                // One byte of the length every 300 ms, which never completes in time.
                let _ = read_msg::<ApiRequest>(&mut stream);
                for _ in 0..10 {
                    std::thread::sleep(Duration::from_millis(300));
                    if stream.write_all(&[0]).is_err() {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            } else {
                serve_api(stream, Arc::new(|_| Some("{}".to_string())));
            }
        });
        serve(listener, "127.0.0.1".parse().unwrap(), Arc::new(b), incoming).unwrap();
        let link = Link::new(HOUR);
        let started = Instant::now();
        assert!(link.call(Some(&a), Some(addr), "GET", "/api/state", "").is_err());
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(1100) && took < Duration::from_millis(1600), "{took:?}");
        assert_eq!(link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap().0, 200);
        assert_eq!(count.load(Ordering::SeqCst), 2);
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
