//! The one-time exchange that turns a 6 digit code into each device knowing the other's public key.
//! SPAKE2 turns the code into a shared key, and `Noise_XXpsk3` with that key as the PSK swaps the
//! long-term public keys and names. `show` and `enter` save nothing; `PairingService` saves the
//! result and runs the exchange for the local API.

use super::datagram::LinkSecurity;
use super::keys::{Pairing, PeerKey};
use super::stream::{read_frame, write_frame};
use snow::{Builder, HandshakeState};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::webui::{HttpRequest, Source};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const PAIRING_PORT: u16 = 24853;
pub const CODE_SECONDS: u64 = 120;

const NOISE_XXPSK3: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"mesh-kvm 1 pair";
const ID_SHOW: &[u8] = b"mesh-kvm show";
const ID_ENTER: &[u8] = b"mesh-kvm enter";
const OK: &[u8] = b"ok";
const MAX_NAME: usize = 64;
const READ_LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    WrongCode,
    Expired,
    Unreachable(String),
    /// The showing device hung up without a word, as its address filter does.
    NotAccepted,
    Failed(String),
    /// The person asked for a new code, or none.
    Cancelled,
}

/// Exactly 6 digits, uniform over 000000 to 999999.
pub fn new_code() -> String {
    loop {
        let mut bytes = [0u8; 4];
        if getrandom::getrandom(&mut bytes).is_err() {
            continue;
        }
        let n = u32::from_be_bytes(bytes);
        // Throw away the top sliver so the remainder is uniform.
        if n < 4_294_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

fn failed(what: &str, e: impl std::fmt::Display) -> PairError {
    PairError::Failed(format!("{what}: {e}"))
}

/// The peer hung up rather than the network failing.
fn closed(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn name_of(payload: &[u8]) -> Result<String, PairError> {
    if payload.len() > MAX_NAME {
        return Err(PairError::Failed("the peer's name is too long".into()));
    }
    String::from_utf8(payload.to_vec()).map_err(|_| PairError::Failed("the peer's name is not text".into()))
}

fn check_own_name(name: &str) -> Result<(), PairError> {
    if name.len() > MAX_NAME {
        return Err(PairError::Failed("this device's name is too long".into()));
    }
    Ok(())
}

struct Exchange {
    tcp: TcpStream,
    raw: Vec<u8>,
    deadline: Instant,
}

impl Exchange {
    fn send(&mut self, body: &[u8]) -> io::Result<()> {
        write_frame(&mut self.tcp, body, Some(self.deadline))
    }

    fn recv(&mut self) -> io::Result<Vec<u8>> {
        read_frame(&mut self.tcp, &mut self.raw, Some(self.deadline))
    }
}

/// SPAKE2 over the code, then the Noise state for the handshake. `showing` picks the side.
fn start(
    me: &Pairing,
    showing: bool,
    spake_in: &[u8],
    spake_out: &[u8],
    spake: Spake2<Ed25519Group>,
) -> Result<HandshakeState, PairError> {
    let key = spake.finish(spake_in).map_err(|e| failed("code exchange", format!("{e:?}")))?;
    let key: [u8; 32] = key.try_into().map_err(|_| PairError::Failed("code exchange: bad key".into()))?;
    // Both sides list the showing device's message first.
    let (first, second) = if showing { (spake_out, spake_in) } else { (spake_in, spake_out) };
    let mut prologue = PROLOGUE.to_vec();
    prologue.extend_from_slice(first);
    prologue.extend_from_slice(second);
    let params = NOISE_XXPSK3.parse().map_err(|_| PairError::Failed("bad noise pattern".into()))?;
    let builder = Builder::new(params)
        .local_private_key(&me.identity.private)
        .and_then(|b| b.prologue(&prologue))
        .and_then(|b| b.psk(3, &key))
        .map_err(|e| failed("noise setup", e))?;
    let state = if showing { builder.build_responder() } else { builder.build_initiator() };
    state.map_err(|e| failed("noise setup", e))
}

fn spake_pair(code: &str, showing: bool) -> (Spake2<Ed25519Group>, Vec<u8>) {
    let password = Password::new(code.as_bytes());
    let (id_show, id_enter) = (Identity::new(ID_SHOW), Identity::new(ID_ENTER));
    if showing {
        Spake2::<Ed25519Group>::start_a(&password, &id_show, &id_enter)
    } else {
        Spake2::<Ed25519Group>::start_b(&password, &id_show, &id_enter)
    }
}

fn remote_key(noise: &HandshakeState) -> Result<[u8; 32], PairError> {
    noise
        .get_remote_static()
        .and_then(|k| <[u8; 32]>::try_from(k).ok())
        .ok_or_else(|| PairError::Failed("the peer sent no key".into()))
}

/// Shows the code: takes one connection on `listener` and returns after it, however it ends.
#[cfg(test)]
pub fn show(
    code: &str,
    me: &Pairing,
    my_name: &str,
    listener: TcpListener,
    only_from: Option<IpAddr>,
    until: Instant,
) -> Result<(PeerKey, IpAddr), PairError> {
    show_with(code, me, my_name, listener, only_from, until, &AtomicBool::new(false), true)
}

/// `show`, which also stops, dropping `listener`, soon after `cancel` is set.
pub fn show_until_cancelled(
    code: &str,
    me: &Pairing,
    my_name: &str,
    listener: TcpListener,
    only_from: Option<IpAddr>,
    until: Instant,
    cancel: &AtomicBool,
) -> Result<(PeerKey, IpAddr), PairError> {
    show_with(code, me, my_name, listener, only_from, until, cancel, true)
}

/// `show`, where `send_ok` false hangs up before the final `ok`, as if it were lost.
fn show_with(
    code: &str,
    me: &Pairing,
    my_name: &str,
    listener: TcpListener,
    only_from: Option<IpAddr>,
    until: Instant,
    cancel: &AtomicBool,
    send_ok: bool,
) -> Result<(PeerKey, IpAddr), PairError> {
    check_own_name(my_name)?;
    listener.set_nonblocking(true).map_err(|e| failed("listen", e))?;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(PairError::Cancelled);
        }
        if Instant::now() >= until {
            return Err(PairError::Expired);
        }
        match listener.accept() {
            Ok((tcp, from)) => {
                if only_from.is_some_and(|ip| ip.to_canonical() != from.ip().to_canonical()) {
                    continue;
                }
                return attempt(tcp, from.ip(), code, me, my_name, until, send_ok);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(failed("accept", e)),
        }
    }
}

fn attempt(
    tcp: TcpStream,
    from: IpAddr,
    code: &str,
    me: &Pairing,
    my_name: &str,
    until: Instant,
    send_ok: bool,
) -> Result<(PeerKey, IpAddr), PairError> {
    tcp.set_nonblocking(false).map_err(|e| failed("connection", e))?;
    let _ = tcp.set_nodelay(true);
    let deadline = (Instant::now() + READ_LIMIT).min(until);
    let mut ex = Exchange { tcp, raw: Vec::new(), deadline };
    let read = |what: &str, e: io::Error| {
        if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) && Instant::now() >= until {
            PairError::Expired
        } else {
            failed(what, e)
        }
    };
    let their_spake = ex.recv().map_err(|e| read("code exchange", e))?;
    let (spake, my_spake) = spake_pair(code, true);
    ex.send(&my_spake).map_err(|e| read("code exchange", e))?;
    let mut noise = start(me, true, &their_spake, &my_spake, spake)?;
    let mut buf = vec![0u8; 65535];
    let m1 = ex.recv().map_err(|e| read("handshake", e))?;
    noise.read_message(&m1, &mut buf).map_err(|e| failed("handshake", e))?;
    let n = noise.write_message(my_name.as_bytes(), &mut buf).map_err(|e| failed("handshake", e))?;
    ex.send(buf.get(..n).unwrap_or(&[])).map_err(|e| read("handshake", e))?;
    let m3 = ex.recv().map_err(|e| read("handshake", e))?;
    // The PSK is mixed in at message 3, so a wrong code only shows here.
    let n = noise.read_message(&m3, &mut buf).map_err(|_| PairError::WrongCode)?;
    let name = name_of(buf.get(..n).unwrap_or(&[]))?;
    let public = remote_key(&noise)?;
    let mut noise = noise.into_transport_mode().map_err(|e| failed("handshake", e))?;
    if send_ok {
        let n = noise.write_message(OK, &mut buf).map_err(|e| failed("ok", e))?;
        ex.send(buf.get(..n).unwrap_or(&[])).map_err(|e| read("ok", e))?;
    }
    Ok((PeerKey { public, name, paired_at: now_secs() }, from))
}

/// Enters the code on the device that connects to the one showing it.
pub fn enter(code: &str, me: &Pairing, my_name: &str, target: SocketAddr) -> Result<PeerKey, PairError> {
    check_own_name(my_name)?;
    // `482 917` is the same code.
    let code = code.replacen(' ', "", 1);
    let tcp = TcpStream::connect_timeout(&target, READ_LIMIT).map_err(|e| {
        if e.kind() == io::ErrorKind::ConnectionRefused {
            PairError::Unreachable(e.to_string())
        } else {
            failed("connect", e)
        }
    })?;
    let _ = tcp.set_nodelay(true);
    let mut ex = Exchange { tcp, raw: Vec::new(), deadline: Instant::now() + READ_LIMIT };
    let (spake, my_spake) = spake_pair(&code, false);
    let no_reply = |e: io::Error| if closed(&e) { PairError::NotAccepted } else { failed("code exchange", e) };
    ex.send(&my_spake).map_err(no_reply)?;
    let their_spake = ex.recv().map_err(no_reply)?;
    let mut noise = start(me, false, &their_spake, &my_spake, spake)?;
    let mut buf = vec![0u8; 65535];
    let n = noise.write_message(&[], &mut buf).map_err(|e| failed("handshake", e))?;
    ex.send(buf.get(..n).unwrap_or(&[])).map_err(|e| failed("handshake", e))?;
    let m2 = ex.recv().map_err(|e| failed("handshake", e))?;
    let n = noise.read_message(&m2, &mut buf).map_err(|e| failed("handshake", e))?;
    let name = name_of(buf.get(..n).unwrap_or(&[]))?;
    let public = remote_key(&noise)?;
    let n = noise.write_message(my_name.as_bytes(), &mut buf).map_err(|e| failed("handshake", e))?;
    ex.send(buf.get(..n).unwrap_or(&[])).map_err(|e| failed("handshake", e))?;
    // A showing device that doesn't like message 3 hangs up without an `ok`.
    let ok = match ex.recv() {
        Ok(ok) => ok,
        Err(e) if closed(&e) => return Err(PairError::WrongCode),
        Err(e) => return Err(failed("ok", e)),
    };
    let mut noise = noise.into_transport_mode().map_err(|e| failed("handshake", e))?;
    let n = noise.read_message(&ok, &mut buf).map_err(|e| failed("ok", e))?;
    if buf.get(..n) != Some(OK) {
        return Err(PairError::Failed("ok: not the expected reply".into()));
    }
    Ok(PeerKey { public, name, paired_at: now_secs() })
}

/// What the service uses of the rest of the program, so that it can be tried on its own.
pub struct Hooks {
    /// This device's name, sent to the other.
    pub name: Box<dyn Fn() -> String + Send + Sync>,
    /// The other device's address, if one is set.
    pub peer_ip: Box<dyn Fn() -> Option<IpAddr> + Send + Sync>,
    /// `--peer` sets the address, so a pairing must not change it.
    pub pinned: Box<dyn Fn() -> bool + Send + Sync>,
    /// Keeps the address the pairing came from.
    pub learn_address: Box<dyn Fn(IpAddr) + Send + Sync>,
    /// Tells the paired device that this one has unpaired.
    pub tell_peer: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
    /// Starts the service again, as the channels are built at start.
    pub restart: Box<dyn Fn() + Send + Sync>,
    pub security: Box<dyn Fn() -> LinkSecurity + Send + Sync>,
}

/// What `tell_peer` sends. It is the one pairing action the peer may use.
pub const PEER_UNPAIRED_BODY: &str = r#"{"action":"peer_unpaired"}"#;

enum Phase {
    Idle,
    Showing { code: String, until: Instant },
    Entering,
    Paired,
    Failed,
}

struct Shared {
    /// This device's keys, and the peer once there is one.
    me: Pairing,
    /// Why `pairing.json` can't be used. It is only replaced by a new pairing.
    problem: Option<String>,
    phase: Phase,
    error: Option<String>,
    /// A pairing finished since this service started, and the restart that follows is yet to come.
    /// The channels still trust whoever they started with, and that device has no say over this one.
    repaired: bool,
    /// Which attempt is current. A thread that finds it moved on drops its result.
    generation: u64,
    cancel: Option<Arc<AtomicBool>>,
    thread: Option<JoinHandle<()>>,
    port: Option<u16>,
}

struct Core {
    path: PathBuf,
    /// Where the code is shown, and where the other device is looked for. 0 picks one, for tests.
    port: u16,
    hooks: Hooks,
    shared: Mutex<Shared>,
}

/// One per service. It works with no peer address and no channels, as the app asks for pairing
/// then too.
pub struct PairingService {
    core: Arc<Core>,
}

fn explain(e: PairError) -> String {
    match e {
        PairError::WrongCode => "The code was wrong. Show a new code and try again.".into(),
        PairError::Expired => "The code ran out. Show a new one.".into(),
        PairError::Unreachable(e) => format!("The other device isn't showing a code: {e}"),
        PairError::NotAccepted => {
            "The other device didn't accept this address. Check the address saved on it, then try again. Its code is still good.".into()
        }
        PairError::Failed(e) => format!("Pairing failed: {e}"),
        PairError::Cancelled => "Pairing was cancelled.".into(),
    }
}

impl PairingService {
    /// `pairing` is None when the file at `path` can't be used, and `problem` says why. The file
    /// is then left alone until a pairing finishes.
    pub fn new(path: PathBuf, pairing: Option<Pairing>, problem: Option<String>, port: u16, hooks: Hooks) -> PairingService {
        let shared = Shared {
            me: pairing.unwrap_or_else(Pairing::fresh),
            problem,
            phase: Phase::Idle,
            error: None,
            repaired: false,
            generation: 0,
            cancel: None,
            thread: None,
            port: None,
        };
        PairingService { core: Arc::new(Core { path, port, hooks, shared: Mutex::new(shared) }) }
    }

    pub fn secure_label(&self) -> &'static str {
        match (self.core.hooks.security)() {
            LinkSecurity::Unpaired => "unpaired",
            LinkSecurity::Connecting => "connecting",
            LinkSecurity::Encrypted => "encrypted",
        }
    }

    /// One with nothing behind it, that a test of the API can drive.
    #[cfg(test)]
    pub fn for_test(path: PathBuf) -> PairingService {
        let hooks = Hooks {
            name: Box::new(|| "Test".to_string()),
            peer_ip: Box::new(|| Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))),
            pinned: Box::new(|| false),
            learn_address: Box::new(|_| {}),
            tell_peer: Box::new(|| Err("no peer".to_string())),
            restart: Box::new(|| {}),
            security: Box::new(|| LinkSecurity::Unpaired),
        };
        PairingService::new(path, Some(Pairing::fresh()), None, 0, hooks)
    }

    /// The port a code is being shown on.
    #[cfg(test)]
    fn listening_port(&self) -> Option<u16> {
        self.core.lock().port
    }

    /// For `GET /api/pairing`. Only the app on this device may read it: it holds the code.
    pub fn status_json(&self) -> String {
        let g = self.core.lock();
        let (state, code, left) = match &g.phase {
            Phase::Idle => ("idle", None, None),
            Phase::Showing { code, until } => {
                let left = until.saturating_duration_since(Instant::now());
                ("showing", Some(code.clone()), Some(left.as_secs() + u64::from(left.subsec_nanos() > 0)))
            }
            Phase::Entering => ("entering", None, None),
            Phase::Paired => ("paired", None, None),
            Phase::Failed => ("failed", None, None),
        };
        let mut status = serde_json::json!({
            "state": state,
            "seconds_left": left,
            "error": g.error,
            "problem": g.problem,
            "peer_name": g.me.peer.as_ref().map(|p| p.name.clone()),
            "fingerprint": g.me.fingerprint(),
        });
        if let Some(code) = code {
            status["code"] = serde_json::json!(code);
        }
        status.to_string()
    }

    /// Shows a new code. One already showing is cancelled first.
    pub fn show_code(&self) -> Result<(), String> {
        let core = &self.core;
        self.stop();
        let port = core.port;
        let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| {
            if e.kind() == io::ErrorKind::AddrInUse {
                format!("Port {port} is in use by another program, so a code can't be shown.")
            } else {
                format!("Couldn't listen on port {port}: {e}")
            }
        });
        let listener = match listener {
            Ok(listener) => listener,
            Err(e) => return Err(core.fail(e)),
        };
        let bound = listener.local_addr().map(|a| a.port()).ok();
        let code = new_code();
        let until = Instant::now() + Duration::from_secs(CODE_SECONDS);
        let cancel = Arc::new(AtomicBool::new(false));
        let (generation, me) = {
            let mut g = core.lock();
            g.generation += 1;
            g.error = None;
            g.phase = Phase::Showing { code: code.clone(), until };
            g.cancel = Some(cancel.clone());
            g.port = bound;
            (g.generation, g.me.clone())
        };
        let name = (core.hooks.name)();
        let only_from = (core.hooks.peer_ip)().map(|ip| ip.to_canonical());
        let worker = core.clone();
        let spawned = std::thread::Builder::new().name("mesh-kvm-pair-show".to_string()).spawn(move || {
            let result = show_until_cancelled(&code, &me, &name, listener, only_from, until, &cancel);
            match result {
                Ok((peer, from)) => worker.finish(generation, peer, Some(from)),
                Err(e) => worker.end(generation, e),
            }
        });
        match spawned {
            Ok(handle) => {
                let mut g = core.lock();
                if g.generation == generation {
                    g.thread = Some(handle);
                }
                Ok(())
            }
            Err(e) => Err(core.fail(format!("Couldn't start pairing: {e}"))),
        }
    }

    /// Enters the code shown on the other device, at the address set for it.
    pub fn enter_code(&self, code: &str) -> Result<(), String> {
        let core = &self.core;
        let code: String = code.split_whitespace().collect();
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err("The code is 6 digits.".into());
        }
        let Some(ip) = (core.hooks.peer_ip)() else {
            return Err("Set the other device's address first, then enter the code.".into());
        };
        self.stop();
        let (generation, me) = {
            let mut g = core.lock();
            g.generation += 1;
            g.error = None;
            g.phase = Phase::Entering;
            (g.generation, g.me.clone())
        };
        let name = (core.hooks.name)();
        let target = SocketAddr::new(ip, core.port);
        let worker = core.clone();
        let spawned = std::thread::Builder::new().name("mesh-kvm-pair-enter".to_string()).spawn(move || {
            match enter(&code, &me, &name, target) {
                Ok(peer) => worker.finish(generation, peer, None),
                Err(e) => worker.end(generation, e),
            }
        });
        spawned.map(|_| ()).map_err(|e| core.fail(format!("Couldn't start pairing: {e}")))
    }

    pub fn cancel(&self) {
        self.stop();
        let mut g = self.core.lock();
        if matches!(g.phase, Phase::Showing { .. } | Phase::Entering | Phase::Failed) {
            g.phase = Phase::Idle;
        }
        g.error = None;
    }

    /// Removes the peer here, after telling it if it can be reached. It is unpaired here either way.
    pub fn unpair(&self) -> Result<(), String> {
        self.remove_peer(true)
    }

    fn remove_peer(&self, tell: bool) -> Result<(), String> {
        let core = &self.core;
        if let Some(problem) = core.lock().problem.clone() {
            return Err(format!("{problem}. Pair again to replace it."));
        }
        if core.lock().me.peer.is_none() {
            return Ok(());
        }
        self.stop();
        if tell {
            // The peer's handshakes are refused anyway if this fails.
            let _ = (core.hooks.tell_peer)();
        }
        let mut next = core.lock().me.clone();
        next.peer = None;
        next.save_to(&core.path)?;
        {
            let mut g = core.lock();
            g.me = next;
            g.phase = Phase::Idle;
            g.error = None;
        }
        (core.hooks.restart)();
        Ok(())
    }

    /// Stops what is running and waits for its port to be free.
    fn stop(&self) {
        let (cancel, thread) = {
            let mut g = self.core.lock();
            g.generation += 1;
            g.port = None;
            (g.cancel.take(), g.thread.take())
        };
        if let Some(cancel) = cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        if let Some(thread) = thread {
            // A connection that is mid-exchange gives up within a few seconds.
            let _ = thread.join();
        }
    }

    /// The pairing actions of the local API, and `GET /api/pairing`. None if the request isn't one.
    pub fn api(&self, req: &HttpRequest) -> Option<String> {
        let path = req.path.split('?').next().unwrap_or("");
        if req.method == "GET" && path == "/api/pairing" {
            // Not even a refusal that says what is here.
            return (req.source() == Source::Local).then(|| self.status_json());
        }
        if req.method != "POST" || path != "/api/local/action" {
            return None;
        }
        let val: serde_json::Value = serde_json::from_str(&req.body).ok()?;
        let action = val.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let reply = |result: Result<(), String>| match result {
            Ok(()) => crate::settings::OK_RESPONSE.to_string(),
            Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
        };
        match action {
            "peer_unpaired" => {
                if req.source() != Source::Peer {
                    return Some(reply(Err("Only the paired device can say that it unpaired.".into())));
                }
                if self.core.lock().repaired {
                    return Some(reply(Err("This device has paired since then.".into())));
                }
                Some(reply(self.remove_peer(false)))
            }
            "pair_show" | "pair_enter" | "pair_cancel" | "unpair" => {
                if req.source() != Source::Local {
                    return Some(reply(Err("Pairing can only be done from the app on this device.".into())));
                }
                Some(reply(match action {
                    "pair_show" => self.show_code(),
                    "pair_enter" => self.enter_code(val.get("code").and_then(|v| v.as_str()).unwrap_or("")),
                    "pair_cancel" => {
                        self.cancel();
                        Ok(())
                    }
                    _ => self.unpair(),
                }))
            }
            _ => None,
        }
    }
}

impl Core {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a failure to start, and gives the text back.
    fn fail(&self, text: String) -> String {
        let mut g = self.lock();
        g.generation += 1;
        g.phase = Phase::Failed;
        g.error = Some(text.clone());
        g.port = None;
        text
    }

    /// An attempt ended without a pairing.
    fn end(&self, generation: u64, e: PairError) {
        let mut g = self.lock();
        if g.generation != generation || matches!(e, PairError::Cancelled) {
            return;
        }
        g.phase = Phase::Failed;
        g.error = Some(explain(e));
        g.port = None;
    }

    /// An attempt ended with a pairing. `from` is where it came from when this device was showing.
    fn finish(&self, generation: u64, peer: PeerKey, from: Option<IpAddr>) {
        let mut next = {
            let g = self.lock();
            if g.generation != generation {
                return;
            }
            g.me.clone()
        };
        next.peer = Some(peer);
        // A damaged file is replaced here, because the person chose to pair again.
        let saved = next.save_to(&self.path);
        {
            let mut g = self.lock();
            if g.generation != generation {
                return;
            }
            g.port = None;
            match saved {
                Ok(()) => {
                    g.me = next;
                    g.problem = None;
                    g.repaired = true;
                    g.phase = Phase::Paired;
                    g.error = None;
                }
                Err(e) => {
                    g.phase = Phase::Failed;
                    g.error = Some(format!("Paired, but {e}. Pair again."));
                    return;
                }
            }
        }
        if let Some(from) = from {
            if (self.hooks.peer_ip)().is_none() && !(self.hooks.pinned)() {
                (self.hooks.learn_address)(from.to_canonical());
            }
        }
        (self.hooks.restart)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::keys::Pairing;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::thread;
    use std::time::{Duration, Instant};

    fn listener() -> (TcpListener, SocketAddr) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        (l, addr)
    }

    fn soon(secs: u64) -> Instant {
        Instant::now() + Duration::from_secs(secs)
    }

    #[test]
    fn a_right_code_swaps_keys_and_names() {
        let (a, b) = (Pairing::fresh(), Pairing::fresh());
        let (l, addr) = listener();
        let a2 = a.clone();
        let t = thread::spawn(move || show("482917", &a2, "Desk", l, None, soon(10)));
        let got_b = enter("482 917", &b, "Laptop", addr).unwrap();
        let (got_a, from) = t.join().unwrap().unwrap();
        assert_eq!(got_b.public, a.identity.public);
        assert_eq!(got_b.name, "Desk");
        assert_eq!(got_a.public, b.identity.public);
        assert_eq!(got_a.name, "Laptop");
        assert_eq!(from, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(got_a.paired_at > 1_700_000_000);
    }

    #[test]
    fn a_wrong_code_fails_on_both_sides_and_saves_nothing() {
        let (a, b) = (Pairing::fresh(), Pairing::fresh());
        let (l, addr) = listener();
        let t = thread::spawn(move || show("482917", &a, "Desk", l, None, soon(10)));
        let entered = enter("482918", &b, "Laptop", addr);
        let shown = t.join().unwrap();
        assert!(matches!(entered, Err(PairError::WrongCode)), "{entered:?}");
        assert!(matches!(shown, Err(PairError::WrongCode)), "{shown:?}");
    }

    #[test]
    fn the_first_try_uses_up_the_code() {
        let (a, b) = (Pairing::fresh(), Pairing::fresh());
        let (l, addr) = listener();
        let t = thread::spawn(move || show("000123", &a, "Desk", l, None, soon(10)));
        let _ = enter("999999", &b, "Laptop", addr);
        assert!(t.join().unwrap().is_err());
        assert!(matches!(enter("000123", &b, "Laptop", addr), Err(PairError::Unreachable(_))));
    }

    #[test]
    fn an_address_other_than_the_peer_is_ignored() {
        let (a, b) = (Pairing::fresh(), Pairing::fresh());
        let (l, addr) = listener();
        let only = Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)));
        let t = thread::spawn(move || show("123456", &a, "Desk", l, only, soon(2)));
        // Turned away twice, where a spent code would have closed the port on the second.
        for _ in 0..2 {
            let why = explain(enter("123456", &b, "Laptop", addr).unwrap_err());
            assert!(why.contains("didn't accept this address"), "{why}");
            assert!(why.contains("address saved on it"), "{why}");
        }
        assert!(matches!(t.join().unwrap(), Err(PairError::Expired)));
    }

    #[test]
    fn nobody_connecting_expires() {
        let (l, _) = listener();
        let started = Instant::now();
        let r = show("123456", &Pairing::fresh(), "Desk", l, None, soon(1));
        assert!(matches!(r, Err(PairError::Expired)));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_lost_ok_fails_the_entering_side_and_pairing_again_works() {
        let (a, b) = (Pairing::fresh(), Pairing::fresh());
        let (l, addr) = listener();
        let a2 = a.clone();
        let t = thread::spawn(move || show_with("111222", &a2, "Desk", l, None, soon(10), &AtomicBool::new(false), false));
        let entered = enter("111222", &b, "Laptop", addr);
        assert!(entered.is_err());
        // The showing side thinks it paired.
        assert_eq!(t.join().unwrap().unwrap().0.public, b.identity.public);
        let (l, addr) = listener();
        let t = thread::spawn(move || show("333444", &a, "Desk", l, None, soon(10)));
        assert!(enter("333444", &b, "Laptop", addr).is_ok());
        assert!(t.join().unwrap().is_ok());
    }

    /// Plays the entering side by hand up to `stage` (0 junk first, 1 junk as message 1, 3 junk as message 3).
    fn junk_at(stage: u8) -> Result<(PeerKey, IpAddr), PairError> {
        let (l, addr) = listener();
        let t = thread::spawn(move || show("123456", &Pairing::fresh(), "Desk", l, None, soon(10)));
        let mut ex = Exchange { tcp: TcpStream::connect(addr).unwrap(), raw: Vec::new(), deadline: soon(5) };
        if stage == 0 {
            ex.send(&[1, 2, 3]).unwrap();
        } else {
            let (spake, mine) = spake_pair("123456", false);
            ex.send(&mine).unwrap();
            let theirs = ex.recv().unwrap();
            let mut noise = start(&Pairing::fresh(), false, &theirs, &mine, spake).unwrap();
            let mut buf = vec![0u8; 65535];
            if stage == 1 {
                ex.send(&[9, 9, 9]).unwrap();
            } else {
                let n = noise.write_message(&[], &mut buf).unwrap();
                ex.send(&buf[..n]).unwrap();
                ex.recv().unwrap();
                ex.send(&[7; 80]).unwrap();
            }
        }
        t.join().unwrap()
    }

    #[test]
    fn a_stranger_that_sends_junk_uses_up_the_code_without_panic() {
        let started = Instant::now();
        assert!(matches!(junk_at(0), Err(PairError::Failed(_))));
        assert!(matches!(junk_at(1), Err(PairError::Failed(_))));
        assert!(matches!(junk_at(3), Err(PairError::WrongCode)));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_silent_connection_past_the_deadline_expires() {
        let (l, addr) = listener();
        let t = thread::spawn(move || show("123456", &Pairing::fresh(), "Desk", l, None, soon(1)));
        let _quiet = TcpStream::connect(addr).unwrap();
        assert!(matches!(t.join().unwrap(), Err(PairError::Expired)));
    }

    #[test]
    fn a_name_over_64_bytes_is_refused() {
        let (a, b) = (Pairing::fresh(), Pairing::fresh());
        let (l, addr) = listener();
        let t = thread::spawn(move || show("123456", &a, "Desk", l, None, soon(1)));
        let long = "x".repeat(65);
        assert!(matches!(enter("123456", &b, &long, addr), Err(PairError::Failed(_))));
        assert!(matches!(t.join().unwrap(), Err(PairError::Expired)));
    }

    use crate::secure::datagram::LinkSecurity;
    use crate::webui::{HttpRequest, Source};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    /// What the service did to the rest of the program.
    #[derive(Default)]
    struct Probe {
        restarts: AtomicUsize,
        told: AtomicUsize,
        learned: Mutex<Vec<IpAddr>>,
    }

    fn hooks(probe: &Arc<Probe>, peer: Option<IpAddr>, pinned: bool, reach_peer: bool) -> Hooks {
        let (p1, p2, p3) = (probe.clone(), probe.clone(), probe.clone());
        Hooks {
            name: Box::new(|| "Desk".to_string()),
            peer_ip: Box::new(move || peer),
            pinned: Box::new(move || pinned),
            learn_address: Box::new(move |ip| p1.learned.lock().unwrap().push(ip)),
            tell_peer: Box::new(move || {
                p2.told.fetch_add(1, Ordering::SeqCst);
                if reach_peer { Ok(()) } else { Err("peer unreachable".to_string()) }
            }),
            restart: Box::new(move || {
                p3.restarts.fetch_add(1, Ordering::SeqCst);
            }),
            security: Box::new(|| LinkSecurity::Unpaired),
        }
    }

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mk-pairing-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const LOCALHOST: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

    /// A service on a free port, with its own file.
    fn service(name: &str, probe: &Arc<Probe>, peer: Option<IpAddr>, pinned: bool) -> (PairingService, PathBuf) {
        let path = dir(name).join("pairing.json");
        let svc = PairingService::new(path.clone(), Some(Pairing::fresh()), None, 0, hooks(probe, peer, pinned, true));
        (svc, path)
    }

    fn state_of(svc: &PairingService) -> serde_json::Value {
        serde_json::from_str(&svc.status_json()).unwrap()
    }

    fn wait_for(svc: &PairingService, state: &str) -> serde_json::Value {
        for _ in 0..300 {
            let status = state_of(svc);
            if status["state"] == state {
                return status;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("never reached {state}: {}", svc.status_json());
    }

    fn code_of(svc: &PairingService) -> String {
        state_of(svc)["code"].as_str().unwrap().to_string()
    }

    /// The entering device, aimed at the port the other service is listening on.
    fn entering_side(name: &str, probe: &Arc<Probe>, port: u16) -> (PairingService, PathBuf) {
        let path = dir(name).join("pairing.json");
        let svc = PairingService::new(path.clone(), Some(Pairing::fresh()), None, port, hooks(probe, LOCALHOST, false, true));
        (svc, path)
    }

    fn loaded(path: &PathBuf) -> Pairing {
        match Pairing::load_from(path) {
            crate::secure::keys::Loaded::Ready(p) => p,
            crate::secure::keys::Loaded::Damaged(e) => panic!("{e}"),
        }
    }

    #[test]
    fn a_finished_pairing_saves_and_asks_for_a_restart() {
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, a_path) = service("finish-a", &pa, LOCALHOST, false);
        a.show_code().unwrap();
        let (b, b_path) = entering_side("finish-b", &pb, a.listening_port().unwrap());
        b.enter_code(&code_of(&a)).unwrap();
        let a_status = wait_for(&a, "paired");
        wait_for(&b, "paired");
        let (a_saved, b_saved) = (loaded(&a_path), loaded(&b_path));
        assert_eq!(a_saved.peer.as_ref().unwrap().public, b_saved.identity.public);
        assert_eq!(b_saved.peer.as_ref().unwrap().public, a_saved.identity.public);
        assert_eq!(a_status["peer_name"], "Desk");
        assert_eq!(a_status["fingerprint"], serde_json::json!(a_saved.fingerprint().unwrap()));
        assert!(a_status.get("code").is_none_or(|c| c.is_null()), "the code is gone once it is used");
        assert_eq!((pa.restarts.load(Ordering::SeqCst), pb.restarts.load(Ordering::SeqCst)), (1, 1));
    }

    #[test]
    fn the_showing_device_learns_the_address_unless_pinned() {
        for (peer, pinned, learns) in [(None, false, true), (None, true, false), (LOCALHOST, false, false)] {
            let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
            let (a, _) = service("learn-a", &pa, peer, pinned);
            a.show_code().unwrap();
            let (b, _) = entering_side("learn-b", &pb, a.listening_port().unwrap());
            b.enter_code(&code_of(&a)).unwrap();
            wait_for(&a, "paired");
            let learned = pa.learned.lock().unwrap().clone();
            assert_eq!(learned, if learns { vec![IpAddr::V4(Ipv4Addr::LOCALHOST)] } else { vec![] });
            assert!(pb.learned.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn a_failed_save_is_reported_and_pairing_again_recovers() {
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, a_path) = service("save-a", &pa, LOCALHOST, false);
        // A folder where the file goes can't be renamed over.
        std::fs::create_dir_all(a_path.join("in-the-way")).unwrap();
        a.show_code().unwrap();
        let (b, _) = entering_side("save-b", &pb, a.listening_port().unwrap());
        b.enter_code(&code_of(&a)).unwrap();
        let status = wait_for(&a, "failed");
        assert!(status["error"].as_str().unwrap().contains("Pair again"), "{status}");
        assert_eq!(pa.restarts.load(Ordering::SeqCst), 0);
        std::fs::remove_dir_all(&a_path).unwrap();
        a.show_code().unwrap();
        let (b, _) = entering_side("save-b2", &pb, a.listening_port().unwrap());
        b.enter_code(&code_of(&a)).unwrap();
        wait_for(&a, "paired");
        assert!(loaded(&a_path).peer.is_some());
    }

    #[test]
    fn entering_needs_an_address_to_go_to() {
        let (svc, _) = service("no-address", &Arc::new(Probe::default()), None, false);
        let err = svc.enter_code("123456").unwrap_err();
        assert!(err.contains("address"), "{err}");
        assert!(svc.enter_code("12345").is_err());
        assert_eq!(state_of(&svc)["state"], "idle");
    }

    #[test]
    fn unpair_removes_the_peer_and_keeps_the_key_pair() {
        let probe = Arc::new(Probe::default());
        let path = dir("unpair").join("pairing.json");
        let mut paired = Pairing::fresh();
        paired.peer = Some(PeerKey { public: [5; 32], name: "Laptop".into(), paired_at: 1 });
        paired.save_to(&path).unwrap();
        let svc = PairingService::new(path.clone(), Some(paired.clone()), None, 0, hooks(&probe, LOCALHOST, false, true));
        assert_eq!(state_of(&svc)["peer_name"], "Laptop");
        svc.unpair().unwrap();
        let after = loaded(&path);
        assert_eq!(after.identity, paired.identity);
        assert!(after.peer.is_none());
        assert!(state_of(&svc)["peer_name"].is_null());
        assert_eq!(probe.restarts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unpair_tells_the_peer_if_it_can() {
        for reach in [true, false] {
            let probe = Arc::new(Probe::default());
            let path = dir("tell").join("pairing.json");
            let mut paired = Pairing::fresh();
            paired.peer = Some(PeerKey { public: [5; 32], name: "Laptop".into(), paired_at: 1 });
            paired.save_to(&path).unwrap();
            let svc = PairingService::new(path.clone(), Some(paired), None, 0, hooks(&probe, LOCALHOST, false, reach));
            svc.unpair().unwrap();
            assert_eq!(probe.told.load(Ordering::SeqCst), 1);
            assert!(loaded(&path).peer.is_none(), "unpaired even when reach is {reach}");
        }
        let body: serde_json::Value = serde_json::from_str(PEER_UNPAIRED_BODY).unwrap();
        assert_eq!(body["action"], "peer_unpaired");
    }

    #[test]
    fn only_the_peer_can_say_it_unpaired_and_this_device_does_not_answer_back() {
        let probe = Arc::new(Probe::default());
        let path = dir("peer-unpaired").join("pairing.json");
        let mut paired = Pairing::fresh();
        paired.peer = Some(PeerKey { public: [5; 32], name: "Laptop".into(), paired_at: 1 });
        paired.save_to(&path).unwrap();
        let svc = PairingService::new(path.clone(), Some(paired), None, 0, hooks(&probe, LOCALHOST, false, true));
        let body = PEER_UNPAIRED_BODY;
        for source in [Source::Local, Source::Browser] {
            let req = HttpRequest::for_test("POST", "/api/local/action", body, source);
            assert!(svc.api(&req).unwrap().contains("\"ok\":false"));
        }
        assert!(loaded(&path).peer.is_some());
        let req = HttpRequest::for_test("POST", "/api/local/action", body, Source::Peer);
        assert_eq!(svc.api(&req).unwrap(), crate::settings::OK_RESPONSE);
        assert!(loaded(&path).peer.is_none());
        assert_eq!(probe.told.load(Ordering::SeqCst), 0);
        assert_eq!(probe.restarts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_old_peers_goodbye_does_not_undo_a_new_pairing() {
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let path = dir("repaired-a").join("pairing.json");
        let mut paired = Pairing::fresh();
        paired.peer = Some(PeerKey { public: [5; 32], name: "Old".into(), paired_at: 1 });
        paired.save_to(&path).unwrap();
        let a = PairingService::new(path.clone(), Some(paired), None, 0, hooks(&pa, LOCALHOST, false, true));
        a.show_code().unwrap();
        let (b, b_path) = entering_side("repaired-b", &pb, a.listening_port().unwrap());
        b.enter_code(&code_of(&a)).unwrap();
        wait_for(&a, "paired");
        let new_peer = loaded(&b_path).identity.public;
        let goodbye = || a.api(&HttpRequest::for_test("POST", "/api/local/action", PEER_UNPAIRED_BODY, Source::Peer)).unwrap();

        // The channels still trust the old key until the restart.
        assert!(goodbye().contains("\"ok\":false"));
        assert_eq!(loaded(&path).peer.unwrap().public, new_peer);
        assert_eq!(state_of(&a)["state"], "paired");

        // A new code moves the state on, and the restart is still to come.
        a.show_code().unwrap();
        assert!(goodbye().contains("\"ok\":false"));
        assert_eq!(loaded(&path).peer.unwrap().public, new_peer);
        assert_eq!(state_of(&a)["state"], "showing");
        a.cancel();
        assert_eq!(pa.restarts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_device_turned_away_is_told_to_check_the_address_and_the_code_stays_good() {
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let elsewhere = Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)));
        let (a, _) = service("away-a", &pa, elsewhere, false);
        a.show_code().unwrap();
        let code = code_of(&a);
        let (b, _) = entering_side("away-b", &pb, a.listening_port().unwrap());
        b.enter_code(&code).unwrap();
        let status = wait_for(&b, "failed");
        assert!(status["error"].as_str().unwrap().contains("didn't accept this address"), "{status}");
        assert_eq!(state_of(&a)["state"], "showing");
        assert_eq!(code_of(&a), code);
        a.cancel();
    }

    #[test]
    fn a_damaged_file_is_not_replaced_by_status_or_unpair() {
        let probe = Arc::new(Probe::default());
        let path = dir("damaged").join("pairing.json");
        std::fs::write(&path, "garbage").unwrap();
        let svc = PairingService::new(path.clone(), None, Some("it is garbage".into()), 0, hooks(&probe, LOCALHOST, false, true));
        assert_eq!(state_of(&svc)["problem"], "it is garbage");
        assert!(svc.unpair().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "garbage");
        assert_eq!(probe.restarts.load(Ordering::SeqCst), 0);
        // A new pairing is what replaces it.
        let pb = Arc::new(Probe::default());
        svc.show_code().unwrap();
        let (b, _) = entering_side("damaged-b", &pb, svc.listening_port().unwrap());
        b.enter_code(&code_of(&svc)).unwrap();
        wait_for(&svc, "paired");
        assert!(loaded(&path).peer.is_some());
        assert!(state_of(&svc)["problem"].is_null());
    }

    #[test]
    fn a_second_show_replaces_the_first_code() {
        let probe = Arc::new(Probe::default());
        let (a, _) = service("second", &probe, LOCALHOST, false);
        a.show_code().unwrap();
        let first_port = a.listening_port().unwrap();
        a.show_code().unwrap();
        let second_port = a.listening_port().unwrap();
        assert_ne!(first_port, second_port);
        assert!(TcpStream::connect(("127.0.0.1", first_port)).is_err(), "the first listener is gone");
        let (b, _) = entering_side("second-b", &probe, second_port);
        b.enter_code(&code_of(&a)).unwrap();
        wait_for(&a, "paired");
    }

    #[test]
    fn cancelling_closes_the_port_and_goes_idle() {
        let (a, _) = service("cancel", &Arc::new(Probe::default()), LOCALHOST, false);
        a.show_code().unwrap();
        let port = a.listening_port().unwrap();
        a.cancel();
        let status = state_of(&a);
        assert_eq!(status["state"], "idle");
        assert!(status["code"].is_null());
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("the port stayed open");
    }

    #[test]
    fn a_port_in_use_is_reported() {
        let taken = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let path = dir("in-use").join("pairing.json");
        let svc = PairingService::new(path, Some(Pairing::fresh()), None, port, hooks(&Arc::new(Probe::default()), LOCALHOST, false, true));
        let err = svc.show_code().unwrap_err();
        assert!(err.contains(&port.to_string()), "{err}");
        let status = state_of(&svc);
        assert_eq!(status["state"], "failed");
        assert_eq!(status["error"].as_str().unwrap(), err);
    }

    #[test]
    fn a_wrong_code_is_reported_and_leaves_nothing_saved() {
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, a_path) = service("wrong-a", &pa, LOCALHOST, false);
        a.show_code().unwrap();
        let (b, _) = entering_side("wrong-b", &pb, a.listening_port().unwrap());
        let wrong = if code_of(&a) == "000000" { "000001" } else { "000000" };
        b.enter_code(wrong).unwrap();
        wait_for(&a, "failed");
        wait_for(&b, "failed");
        assert!(!a_path.exists());
        assert_eq!(pa.restarts.load(Ordering::SeqCst) + pb.restarts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pairing_is_refused_from_the_peer_and_from_a_browser() {
        let probe = Arc::new(Probe::default());
        let (svc, path) = service("refused", &probe, LOCALHOST, false);
        for source in [Source::Peer, Source::Browser] {
            for action in ["pair_show", "pair_enter", "pair_cancel", "unpair"] {
                let body = serde_json::json!({ "action": action, "code": "123456" }).to_string();
                let req = HttpRequest::for_test("POST", "/api/local/action", &body, source);
                let reply = svc.api(&req).unwrap();
                assert!(reply.contains("\"ok\":false"), "{action} from {source:?}: {reply}");
            }
            assert!(svc.api(&HttpRequest::for_test("GET", "/api/pairing", "", source)).is_none());
        }
        assert_eq!(state_of(&svc)["state"], "idle");
        assert!(svc.listening_port().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn the_local_api_shows_and_cancels() {
        let (svc, _) = service("local-api", &Arc::new(Probe::default()), LOCALHOST, false);
        let post = |body: &str| svc.api(&HttpRequest::for_test("POST", "/api/local/action", body, Source::Local));
        assert_eq!(post(r#"{"action":"pair_show"}"#).unwrap(), crate::settings::OK_RESPONSE);
        let shown = svc.api(&HttpRequest::for_test("GET", "/api/pairing", "", Source::Local)).unwrap();
        let shown: serde_json::Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(shown["state"], "showing");
        assert_eq!(shown["code"].as_str().unwrap().len(), 6);
        assert!(shown["seconds_left"].as_u64().unwrap() > 100);
        assert_eq!(post(r#"{"action":"pair_cancel"}"#).unwrap(), crate::settings::OK_RESPONSE);
        assert_eq!(state_of(&svc)["state"], "idle");
        assert!(post(r#"{"action":"pair_enter","code":"12"}"#).unwrap().contains("\"ok\":false"));
    }

    #[test]
    fn the_security_label_follows_the_channel() {
        let svc = PairingService::new(dir("label").join("p.json"), Some(Pairing::fresh()), None, 0, hooks(&Arc::new(Probe::default()), None, false, true));
        assert_eq!(svc.secure_label(), "unpaired");
    }

    #[test]
    fn codes_are_six_digits_with_leading_zeros_kept() {
        let mut seen_zero = false;
        for _ in 0..3000 {
            let c = new_code();
            assert_eq!(c.len(), 6);
            assert!(c.bytes().all(|b| b.is_ascii_digit()));
            seen_zero |= c.starts_with('0');
        }
        assert!(seen_zero);
    }
}
