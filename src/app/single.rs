//! One window and one tray per login. The first of each holds a loopback port; a second one finds
//! the port taken, and (for the window) asks the first to come forward. Quitting mesh-kvm asks both
//! to close through the same ports.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Held by the running app window.
pub const WINDOW_PORT: u16 = 24881;
/// Held by the running tray.
pub const TRAY_PORT: u16 = 24882;

const SHOW: &[u8] = b"show\n";
const QUIT: &[u8] = b"quit\n";
const IO_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a new copy waits for the old one it's replacing to let go of the port.
const HANDOVER: Duration = Duration::from_secs(5);

pub enum Instance {
    /// Nobody else holds the port. Keep this alive for as long as the process should own it.
    First(TcpListener),
    AlreadyRunning,
    /// The port couldn't be taken for some other reason (the message says why).
    Failed(String),
}

/// Takes `port` on 127.0.0.1.
pub fn claim(port: u16) -> Instance {
    classify(TcpListener::bind(("127.0.0.1", port)))
}

/// Takes `port`, and when this copy is replacing an old one, gives that one time to end. Otherwise
/// it would ask the old window to come forward, just as that closes.
pub fn claim_after_handover(port: u16) -> Instance {
    let replacing = std::env::var_os(super::launch::RELAUNCHED_ENV).is_some();
    std::env::remove_var(super::launch::RELAUNCHED_ENV);
    wait_for_port(|| claim(port), if replacing { HANDOVER } else { Duration::ZERO })
}

fn wait_for_port(claim: impl Fn() -> Instance, patience: Duration) -> Instance {
    let started = Instant::now();
    loop {
        match claim() {
            Instance::AlreadyRunning if started.elapsed() < patience => std::thread::sleep(Duration::from_millis(50)),
            other => return other,
        }
    }
}

/// A taken port means another instance holds it. Any other error is a real failure.
fn classify(bound: std::io::Result<TcpListener>) -> Instance {
    match bound {
        Ok(listener) => Instance::First(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Instance::AlreadyRunning,
        Err(e) => Instance::Failed(format!("mesh-kvm couldn't reserve its local port: {e}")),
    }
}

/// What one process can ask the one holding a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    /// Bring the window forward.
    Show,
    /// Close, as mesh-kvm is quitting on this device.
    Quit,
}

impl Word {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Show => SHOW,
            Self::Quit => QUIT,
        }
    }
}

/// True while some process holds `port`, such as a running tray.
pub fn is_held(port: u16) -> bool {
    TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST, port).into(), IO_TIMEOUT).is_ok()
}

/// Asks the process holding `port` to bring its window forward.
pub fn signal_show(port: u16) {
    signal(port, Word::Show);
}

/// Says `word` to the process holding `port`, if there is one.
pub fn signal(port: u16, word: Word) {
    if let Ok(mut stream) = TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST, port).into(), IO_TIMEOUT) {
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let _ = stream.write_all(word.bytes());
    }
}

/// Runs `on_word` each time another process calls `signal` for this listener.
pub fn listen(listener: TcpListener, on_word: impl Fn(Word) + Send + 'static) {
    let spawned = std::thread::Builder::new().name("mesh-kvm-single".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let mut said = [0u8; 5];
            if stream.read_exact(&mut said).is_err() {
                continue;
            }
            if let Some(word) = [Word::Show, Word::Quit].into_iter().find(|w| w.bytes() == said) {
                on_word(word);
            }
        }
    });
    drop(spawned);
}

static WINDOW_LISTENER: OnceLock<Mutex<Option<TcpListener>>> = OnceLock::new();

/// Keeps the window's listener until the window exists and can be told to focus.
pub fn keep_for_window(listener: TcpListener) {
    let slot = WINDOW_LISTENER.get_or_init(Default::default);
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(listener);
}

/// Call once the window's egui context exists: from then on, a second `--app` raises this window,
/// and quitting mesh-kvm closes it.
pub fn serve_focus(ctx: eframe::egui::Context) {
    let listener = WINDOW_LISTENER
        .get()
        .and_then(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).take());
    if let Some(listener) = listener {
        listen(listener, move |word| {
            if word == Word::Quit {
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                ctx.request_repaint();
                return;
            }
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
            // Wayland refuses a plain focus request. This one goes through xdg-activation.
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::RequestUserAttention(
                eframe::egui::UserAttentionType::Informational,
            ));
            ctx.request_repaint();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_that_replaces_another_waits_for_the_port_and_any_other_does_not() {
        let looks = std::cell::Cell::new(0);
        let frees_up = || {
            looks.set(looks.get() + 1);
            if looks.get() < 3 {
                Instance::AlreadyRunning
            } else {
                Instance::Failed("free".into())
            }
        };
        assert!(matches!(wait_for_port(&frees_up, HANDOVER), Instance::Failed(_)));
        assert_eq!(looks.get(), 3);
        looks.set(0);
        assert!(matches!(wait_for_port(&frees_up, Duration::ZERO), Instance::AlreadyRunning));
        assert_eq!(looks.get(), 1);
    }
    use std::sync::mpsc;

    /// A port nobody holds right now.
    fn free_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn only_a_taken_port_means_already_running() {
        let taken = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        assert!(matches!(classify(Err(taken)), Instance::AlreadyRunning));
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(matches!(classify(Err(denied)), Instance::Failed(_)));
    }

    #[test]
    fn second_claim_reports_already_running() {
        let port = free_port();
        let first = claim(port);
        assert!(matches!(first, Instance::First(_)));
        assert!(matches!(claim(port), Instance::AlreadyRunning));
    }

    #[test]
    fn claim_works_after_first_is_dropped() {
        let port = free_port();
        drop(claim(port));
        assert!(matches!(claim(port), Instance::First(_)));
    }

    #[test]
    fn signal_show_reaches_the_listener() {
        let port = free_port();
        let Instance::First(listener) = claim(port) else { panic!("port should be free") };
        let (tx, rx) = mpsc::channel();
        listen(listener, move |word| {
            let _ = tx.send(word);
        });
        signal_show(port);
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)), Ok(Word::Show));
    }

    #[test]
    fn quitting_reaches_the_listener_as_its_own_word() {
        let port = free_port();
        let Instance::First(listener) = claim(port) else { panic!("port should be free") };
        let (tx, rx) = mpsc::channel();
        listen(listener, move |word| {
            let _ = tx.send(word);
        });
        signal(port, Word::Quit);
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)), Ok(Word::Quit));
    }

    #[test]
    fn a_held_port_is_seen_and_a_free_one_is_not() {
        let port = free_port();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        assert!(is_held(port));
        drop(listener);
        assert!(!is_held(port));
    }

    #[test]
    fn saying_something_to_nobody_does_no_harm() {
        signal(free_port(), Word::Quit);
    }

    #[test]
    fn stray_connections_do_not_raise_the_window() {
        let port = free_port();
        let Instance::First(listener) = claim(port) else { panic!("port should be free") };
        let (tx, rx) = mpsc::channel();
        listen(listener, move |word| {
            let _ = tx.send(word);
        });
        TcpStream::connect(("127.0.0.1", port)).unwrap().write_all(b"GET /\r\n").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(700)).is_err());
    }
}
