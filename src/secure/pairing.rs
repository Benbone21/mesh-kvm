//! The one-time exchange that turns a 6 digit code into each device knowing the other's public key.
//! SPAKE2 turns the code into a shared key, and `Noise_XXpsk3` with that key as the PSK swaps the
//! long-term public keys and names. Nothing is saved here; the caller saves the result.

use super::keys::{Pairing, PeerKey};
use super::stream::{read_frame, write_frame};
use snow::{Builder, HandshakeState};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
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
    Failed(String),
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
pub fn show(
    code: &str,
    me: &Pairing,
    my_name: &str,
    listener: TcpListener,
    only_from: Option<IpAddr>,
    until: Instant,
) -> Result<(PeerKey, IpAddr), PairError> {
    show_with(code, me, my_name, listener, only_from, until, true)
}

/// `show`, where `send_ok` false hangs up before the final `ok`, as if it were lost.
fn show_with(
    code: &str,
    me: &Pairing,
    my_name: &str,
    listener: TcpListener,
    only_from: Option<IpAddr>,
    until: Instant,
    send_ok: bool,
) -> Result<(PeerKey, IpAddr), PairError> {
    check_own_name(my_name)?;
    listener.set_nonblocking(true).map_err(|e| failed("listen", e))?;
    loop {
        if Instant::now() >= until {
            return Err(PairError::Expired);
        }
        match listener.accept() {
            Ok((tcp, from)) => {
                if only_from.is_some_and(|ip| ip != from.ip()) {
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
    ex.send(&my_spake).map_err(|e| failed("code exchange", e))?;
    let their_spake = ex.recv().map_err(|e| failed("code exchange", e))?;
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
        assert!(enter("123456", &b, "Laptop", addr).is_err());
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
        let t = thread::spawn(move || show_with("111222", &a2, "Desk", l, None, soon(10), false));
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
