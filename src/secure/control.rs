//! The listener on TCP 24852, and connections to the peer's. Clipboard and the peer API share it:
//! the first thing on a secure stream says which one a connection is for.

use super::keys::Pairing;
use super::stream::{Purpose, SecureStream, Wire};
use crate::webui::{self, Handler, HttpRequest};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
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
    /// The link's generation it was made in. One made before the link last went down or up is
    /// dropped rather than used.
    generation: u64,
}

/// The link as the router last said, for calls to fail at once while it's down.
const LINK_UNKNOWN: u8 = 0;
const LINK_UP: u8 = 1;
const LINK_DOWN: u8 = 2;
/// What a call says while the link is down, without trying anything.
const OFFLINE: &str = "the other device is offline";
/// The budget for making the connection again in the background when the link comes back.
const RECONNECT_WAIT: Duration = Duration::from_millis(1750);
/// The most a call waits for a reconnect under way, which may be finishing behind another call.
const RECONNECT_SETTLE: Duration = Duration::from_secs(2);

/// Whether a call heeds what the link is said to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Fails at once while the link is down: the app's calls and the layout sync.
    WhileLinked,
    /// Tries whatever the link is said to be: updating and unpairing, which must get through even
    /// when the input link can't, as after an update that changed it.
    AnyLink,
}

/// What the link log says of the connection: not yet used, or how it went last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Unused,
    Up,
    Down,
}

/// A `control_down` whose error has changed is logged again, but no more often than this.
const DOWN_AGAIN_AFTER: Duration = Duration::from_secs(10);

/// When the connection's ups and downs are worth a line: each change, and a new error while it
/// stays down, so a timeout and a refusal can be told apart.
struct ControlNote {
    state: State,
    /// The error last logged.
    error: String,
    logged_at: Option<Instant>,
}

impl ControlNote {
    const fn new() -> ControlNote {
        ControlNote { state: State::Unused, error: String::new(), logged_at: None }
    }

    /// The word to log for this outcome, if any.
    fn note(&mut self, result: Result<(), &str>, now: Instant) -> Option<&'static str> {
        match result {
            Ok(()) if self.state == State::Up => None,
            Ok(()) => {
                self.state = State::Up;
                self.logged_at = Some(now);
                Some("control_up")
            }
            Err(error) => {
                let again = error != self.error && self.logged_at.is_none_or(|at| now.duration_since(at) >= DOWN_AGAIN_AFTER);
                if self.state == State::Down && !again {
                    return None;
                }
                self.state = State::Down;
                self.error = error.to_string();
                self.logged_at = Some(now);
                Some("control_down")
            }
        }
    }
}

/// The one connection requests to the peer go over, and whether it is still good.
struct Link {
    connection: Mutex<Option<ApiConnection>>,
    max_age: Duration,
    note: Mutex<ControlNote>,
    /// Its ups and downs go in the link log.
    logged: bool,
    /// `LINK_UP`, `LINK_DOWN`, or `LINK_UNKNOWN` until told, which counts as up.
    online: AtomicU8,
    /// Goes up each time the link goes down or up.
    generation: AtomicU64,
    /// Background reconnects under way. A call waits for them, up to `RECONNECT_SETTLE`, instead of
    /// racing them, and before it takes `connection`, which they need to put their connection in.
    reconnects: Mutex<u32>,
    settled: Condvar,
}

impl Link {
    const fn new(max_age: Duration) -> Link {
        Link::with_logging(max_age, true)
    }

    const fn with_logging(max_age: Duration, logged: bool) -> Link {
        Link {
            connection: Mutex::new(None),
            max_age,
            note: Mutex::new(ControlNote::new()),
            logged,
            online: AtomicU8::new(LINK_UNKNOWN),
            generation: AtomicU64::new(0),
            reconnects: Mutex::new(0),
            settled: Condvar::new(),
        }
    }

    /// A connection of its own for something that only asks now and then, so that it never holds up
    /// the app's requests, and nothing it meets counts as news of the connection.
    const fn unlogged(max_age: Duration) -> Link {
        Link::with_logging(max_age, false)
    }

    /// Logs `control_up` and `control_down` as the connection changes.
    fn note(&self, result: Result<(), &str>, target: Option<SocketAddr>) {
        if !self.logged {
            return;
        }
        let word = self.note.lock().unwrap_or_else(|e| e.into_inner()).note(result, Instant::now());
        let Some(word) = word else { return };
        let to = target.map_or("-".to_string(), |t| t.to_string());
        let details = crate::linklog::Details::new().text("to", &to);
        match result {
            Ok(()) => crate::linklog::record(word, details),
            Err(error) => crate::linklog::record(word, details.text("error", error)),
        }
    }

    fn offline(&self) -> bool {
        self.online.load(Ordering::SeqCst) == LINK_DOWN
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Hears from the input loop whether the link is up, every time round, so it must not wait:
    /// what a change needs done is done on a thread of its own. A connection made before a change
    /// is never used after it. Going down drops it; coming back makes a new one with
    /// `reconnect_with`, if given, within `RECONNECT_WAIT`, and puts it in place unless a call has
    /// made one meanwhile, so nothing is left open unused.
    fn set_online(&'static self, online: bool, reconnect_with: Option<(Arc<Pairing>, SocketAddr)>) {
        let state = if online { LINK_UP } else { LINK_DOWN };
        if self.online.swap(state, Ordering::SeqCst) == state {
            return;
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let reconnect = if online { reconnect_with } else { None };
        if reconnect.is_some() {
            *self.reconnects.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        }
        let counted = reconnect.is_some();
        let spawned = std::thread::Builder::new().name("mesh-kvm-control-link".to_string()).spawn(move || {
            let _done = counted.then(|| Settles(self));
            match reconnect {
                None => self.drop_stale(),
                Some((pairing, target)) => self.reconnect(generation, &pairing, target),
            }
        });
        if spawned.is_err() && counted {
            drop(Settles(self));
        }
    }

    /// Drops a connection made before the link last changed.
    fn drop_stale(&self) {
        let mut slot = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|c| c.generation != self.generation()) {
            *slot = None;
        }
    }

    fn reconnect(&self, generation: u64, pairing: &Pairing, target: SocketAddr) {
        let current = || self.generation() == generation;
        if !current() {
            return;
        }
        let opened = open_with(Some(pairing), target, Purpose::Api, RECONNECT_WAIT);
        // A link that has changed again since makes this outcome old news.
        if !current() {
            return;
        }
        match opened {
            Ok(stream) => {
                let mut slot = self.connection.lock().unwrap_or_else(|e| e.into_inner());
                // A call that couldn't wait made one already: this one would only sit open.
                if current() && !slot.as_ref().is_some_and(|c| c.generation == generation) {
                    self.note(Ok(()), Some(target));
                    *slot = Some(ApiConnection { stream, opened: Instant::now(), generation });
                }
            }
            Err(e) => self.note(Err(&e), Some(target)),
        }
    }

    /// Waits, up to `RECONNECT_SETTLE`, for reconnects under way.
    fn wait_for_reconnects(&self) {
        let count = self.reconnects.lock().unwrap_or_else(|e| e.into_inner());
        drop(self.settled.wait_timeout_while(count, RECONNECT_SETTLE, |n| *n > 0));
    }

    fn state_text(&self) -> &'static str {
        match self.note.lock().unwrap_or_else(|e| e.into_inner()).state {
            State::Up => "up",
            State::Down => "down",
            State::Unused => "unused",
        }
    }

    /// One request, with one in flight at a time. A failure drops the connection and gives the error:
    /// the call isn't tried again, as a change must not be sent twice, and the next one reconnects.
    fn call(&self, pairing: Option<&Pairing>, target: Option<SocketAddr>, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
        self.call_within(pairing, target, method, path, body, CONNECT_WAIT, ANSWER_WAIT)
    }

    /// `call_within` whatever the link is said to be.
    #[allow(clippy::too_many_arguments)]
    fn call_any_link(&self, pairing: Option<&Pairing>, target: Option<SocketAddr>, method: &str, path: &str, body: &str, connect: Duration, answer: Duration) -> Result<(u16, String), String> {
        self.call_gated(Gate::AnyLink, pairing, target, method, path, body, connect, answer)
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
        self.call_gated(Gate::WhileLinked, pairing, target, method, path, body, connect, answer)
    }

    #[allow(clippy::too_many_arguments)]
    fn call_gated(
        &self,
        gate: Gate,
        pairing: Option<&Pairing>,
        target: Option<SocketAddr>,
        method: &str,
        path: &str,
        body: &str,
        connect: Duration,
        answer: Duration,
    ) -> Result<(u16, String), String> {
        // With the link down nothing is tried, and nothing waits its turn for a connection that can't work.
        let refused = || gate == Gate::WhileLinked && self.offline();
        if refused() {
            return Err(OFFLINE.into());
        }
        let request = request_for(method, path, body)?;
        self.wait_for_reconnects();
        if refused() {
            return Err(OFFLINE.into());
        }
        let mut slot = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        if refused() {
            return Err(OFFLINE.into());
        }
        self.call_on(&mut slot, pairing, target, &request, connect, answer)
    }

    fn call_on(
        &self,
        slot: &mut Option<ApiConnection>,
        pairing: Option<&Pairing>,
        target: Option<SocketAddr>,
        request: &ApiRequest,
        connect: Duration,
        answer: Duration,
    ) -> Result<(u16, String), String> {
        let generation = self.generation();
        if slot.as_ref().is_some_and(|c| c.opened.elapsed() >= self.max_age || c.generation != generation) {
            *slot = None;
        }
        if slot.is_none() {
            let target = target.ok_or("no peer")?;
            let stream = open_with(pairing, target, Purpose::Api, connect).inspect_err(|e| self.note(Err(e), Some(target)))?;
            self.note(Ok(()), Some(target));
            *slot = Some(ApiConnection { stream, opened: Instant::now(), generation });
        }
        let Some(connection) = slot.as_mut() else { return Err("no connection".into()) };
        let result = exchange(&mut connection.stream, request, answer);
        if let Err(e) = &result {
            *slot = None;
            self.note(Err(e), target);
        }
        result
    }
}

/// Counts a reconnect as done when it goes, however it ended, and wakes the calls waiting on it.
struct Settles(&'static Link);

impl Drop for Settles {
    fn drop(&mut self) {
        let mut count = self.0.reconnects.lock().unwrap_or_else(|e| e.into_inner());
        *count = count.saturating_sub(1);
        self.0.settled.notify_all();
    }
}

fn request_for(method: &str, path: &str, body: &str) -> Result<ApiRequest, String> {
    let request = ApiRequest { method: method.into(), path: path.into(), body: body.into() };
    let size = bincode::serialized_size(&request).map_err(|e| e.to_string())?;
    if size > MAX_FRAME as u64 {
        return Err("the message is too large".into());
    }
    Ok(request)
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
static CLOCK_LINK: Link = Link::unlogged(HOUR);
/// The clock look's whole budget: an answer later than this says little about the clock anyway.
const CLOCK_CONNECT_WAIT: Duration = Duration::from_millis(350);
const CLOCK_ANSWER_WAIT: Duration = Duration::from_millis(1000);

/// Where the peer's control listener is. Until it's set, the peer can't be called.
pub fn set_target(target: SocketAddr) {
    let _ = TARGET.set(target);
}

/// Whether the router has the link up, from the input loop every time round. Calls fail at once
/// while it's down, and the connection is made again in the background when it returns.
pub fn set_peer_online(online: bool) {
    let reconnect = || super::services().pairing.clone().zip(TARGET.get().copied());
    let changed = |link: &Link| link.online.load(Ordering::Relaxed) != if online { LINK_UP } else { LINK_DOWN };
    if changed(&PEER_LINK) {
        PEER_LINK.set_online(online, if online { reconnect() } else { None });
    }
    if changed(&CLOCK_LINK) {
        CLOCK_LINK.set_online(online, None);
    }
}

/// The connection to the peer's API as it last went: `up`, `down`, or `unused` while nothing has
/// needed it. Nothing tries it just to find out: the link log's look at the other device's clock
/// has a connection of its own, which isn't counted.
pub fn control_state() -> &'static str {
    PEER_LINK.state_text()
}

/// A request for the link log's look at the other device's clock, on a connection of its own and
/// within a short budget, so it neither waits behind nor holds up the app's requests, and its
/// failures are no `control_down`. `None` when this device has no peer to call.
pub fn peer_clock_call(path: &str) -> Option<Result<(u16, String), String>> {
    let target = TARGET.get().copied()?;
    let pairing = super::services().pairing.as_deref();
    Some(CLOCK_LINK.call_within(pairing, Some(target), "GET", path, "", CLOCK_CONNECT_WAIT, CLOCK_ANSWER_WAIT))
}

/// A request to the other device's API. Fails when this device isn't paired.
pub fn peer_call(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    PEER_LINK.call(super::services().pairing.as_deref(), TARGET.get().copied(), method, path, body)
}

/// `peer_call` for updating and unpairing: tried even while the input link is said to be down, as
/// a device on a newer version may not speak its UDP, and the TCP connection may still get there.
pub fn peer_call_any_link(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    PEER_LINK.call_any_link(super::services().pairing.as_deref(), TARGET.get().copied(), method, path, body, CONNECT_WAIT, ANSWER_WAIT)
}

/// `peer_call_any_link` for a request that matters enough to wait out a slow link, like installing.
pub fn peer_call_any_link_patiently(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    let pairing = super::services().pairing.as_deref();
    PEER_LINK.call_any_link(pairing, TARGET.get().copied(), method, path, body, PATIENT_CONNECT_WAIT, PATIENT_ANSWER_WAIT)
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
    fn a_new_error_while_down_is_logged_again_but_not_too_often() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut note = ControlNote::new();
        assert_eq!(note.note(Err("timed out"), at(0)), Some("control_down"));
        assert_eq!(note.note(Err("timed out"), at(20)), None, "the same error");
        assert_eq!(note.note(Err("refused"), at(25)), Some("control_down"), "a new one");
        assert_eq!(note.note(Err("timed out"), at(30)), None, "too soon after the last");
        assert_eq!(note.note(Err("timed out"), at(36)), Some("control_down"));
        assert_eq!(note.note(Ok(()), at(37)), Some("control_up"));
        assert_eq!(note.note(Ok(()), at(38)), None);
        assert_eq!(note.note(Err("timed out"), at(39)), Some("control_down"), "going down is always news");
    }

    #[test]
    fn a_connection_of_its_own_is_no_news_of_the_shared_one() {
        let (a, b) = paired_pair();
        let (addr, _) = serving_api(b, 1);
        let quiet = Link::unlogged(HOUR);
        assert!(quiet.call(Some(&a), Some(addr), "GET", "/api/state", "").is_err(), "the first is closed on it");
        assert_eq!(quiet.state_text(), "unused", "its failures go unnoted");
        assert_eq!(quiet.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap().0, 200);
        assert_eq!(quiet.state_text(), "unused");
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

    fn leaked() -> &'static Link {
        Box::leak(Box::new(Link::new(HOUR)))
    }

    fn until(what: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < WAIT {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn while_the_link_is_down_a_call_fails_at_once_and_tries_nothing() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 0);
        let link = leaked();
        link.set_online(false, None);
        let started = Instant::now();
        let err = link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(50), "{:?}", started.elapsed());
        assert!(err.contains("offline"), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 0, "no connection was tried");
        assert_eq!(link.state_text(), "unused", "no control_down for it");
    }

    #[test]
    fn a_call_waiting_its_turn_when_the_link_goes_down_fails_at_once() {
        let (a, b) = paired_pair();
        let (addr, _) = serving_api(b, 0);
        let link = leaked();
        let slot = link.connection.lock().unwrap();
        let caller = std::thread::spawn(move || link.call(Some(&a), Some(addr), "GET", "/api/state", ""));
        std::thread::sleep(Duration::from_millis(50));
        link.set_online(false, None);
        drop(slot);
        let err = caller.join().unwrap().unwrap_err();
        assert!(err.contains("offline"), "{err}");
    }

    #[test]
    fn when_the_link_comes_back_the_connection_is_made_again_in_the_background() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 0);
        let link = leaked();
        link.set_online(true, None);
        link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        link.set_online(false, None);
        assert!(until(|| link.connection.lock().unwrap().is_none()), "the old connection goes with the link");
        let started = Instant::now();
        link.set_online(true, Some((Arc::new(a.clone()), addr)));
        assert!(started.elapsed() < Duration::from_millis(20), "the caller is the input loop");
        assert!(until(|| count.load(Ordering::SeqCst) == 2), "reconnected without being asked");
        assert_eq!(link.state_text(), "up");
        assert_eq!(link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap().0, 200);
        assert_eq!(count.load(Ordering::SeqCst), 2, "the call used the new connection");
        // Staying up is no reason to connect again.
        link.set_online(true, Some((Arc::new(a.clone()), addr)));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_update_or_unpair_call_goes_whatever_the_link_is_said_to_be() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 0);
        let link = leaked();
        link.set_online(false, None);
        let answer = link.call_any_link(Some(&a), Some(addr), "GET", "/api/state", "", CONNECT_WAIT, ANSWER_WAIT);
        assert_eq!(answer.unwrap().0, 200, "UDP may be down while TCP works");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap_err().contains("offline"), "the app's calls still fail fast");
    }

    /// A listener that takes connections and never says a word, so a handshake waits out its limit.
    fn silent() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for tcp in listener.incoming() {
                held.push(tcp);
            }
        });
        addr
    }

    #[test]
    fn a_link_that_flaps_keeps_the_newest_reconnect() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 0);
        let link = leaked();
        let pairing = Arc::new(a.clone());
        for _ in 0..20 {
            link.set_online(false, None);
            link.set_online(true, Some((Arc::clone(&pairing), addr)));
        }
        std::thread::sleep(Duration::from_millis(500));
        let made = count.load(Ordering::SeqCst);
        assert_eq!(link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap().0, 200);
        assert_eq!(count.load(Ordering::SeqCst), made, "the call found the newest reconnect still there");
    }

    #[test]
    fn a_call_straight_after_the_link_comes_up_uses_the_reconnect_and_leaves_nothing_open() {
        let (a, b) = paired_pair();
        let (addr, count) = serving_api(b, 0);
        let link = leaked();
        link.set_online(false, None);
        link.set_online(true, Some((Arc::new(a.clone()), addr)));
        assert_eq!(link.call(Some(&a), Some(addr), "GET", "/api/state", "").unwrap().0, 200);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(count.load(Ordering::SeqCst), 1, "one connection, not the call's and an idle one");
    }

    #[test]
    fn a_call_waiting_for_a_reconnect_fails_once_the_link_drops_and_never_waits_long() {
        let (a, _) = paired_pair();
        let nobody = silent();
        let link = leaked();
        link.set_online(false, None);
        link.set_online(true, Some((Arc::new(a.clone()), nobody)));
        let started = Instant::now();
        let pairing = a.clone();
        let caller = std::thread::spawn(move || link.call(Some(&pairing), Some(nobody), "GET", "/api/state", ""));
        std::thread::sleep(Duration::from_millis(100));
        link.set_online(false, None);
        let err = caller.join().unwrap().unwrap_err();
        assert!(err.contains("offline"), "{err}");
        assert!(started.elapsed() < Duration::from_millis(2300), "{:?}", started.elapsed());
        // And with the link staying up, the wait for the reconnect is bounded too.
        link.set_online(true, Some((Arc::new(a.clone()), nobody)));
        let started = Instant::now();
        let _ = link.call(Some(&a), Some(nobody), "GET", "/api/state", "");
        assert!(started.elapsed() < Duration::from_millis(2600), "{:?}", started.elapsed());
    }

    #[test]
    fn a_reconnect_that_fails_while_the_link_drops_again_is_no_news() {
        let (a, _) = paired_pair();
        let nobody = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let link = leaked();
        link.set_online(false, None);
        link.set_online(true, Some((Arc::new(a), nobody)));
        link.set_online(false, None);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(link.state_text(), "unused");
    }

    #[test]
    fn without_a_pairing_nothing_is_opened() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(open_with(None, addr, Purpose::Clipboard, WAIT).is_err());
    }
}
