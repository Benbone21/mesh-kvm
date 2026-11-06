//! Cross-platform local API server (TCP 24880), used by the native app.
//!
//! Every node (Debian + Windows) exposes the following. Any other path, including `/`, is a 404:
//!   GET  /api/local              -> this node's live status (node-specific handler)
//!   POST /api/local/action       -> this node's local actions (audio, autostart, focus, panic...)
//!   ANY  /api/peer/<path>        -> transparently proxied to http://<peer_ip>:24880/<path>
//!   GET  /api/state              -> this node's copy of the layout, and live telemetry
//!   POST /api/layout             -> an edit to the layout
//!   POST /api/action             -> layout-wide actions (focus, release, game lock...)

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

pub const UI_PORT: u16 = 24880;
/// What `local_ip_toward` gives when there is no route to the address.
pub const UNKNOWN_ADDRESS: &str = "unknown";
const REFUSED: &str = r#"{"ok":false,"error":"Changes can only come from this device or the other device."}"#;

pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub body: String,
    /// From a program on this device, and not from a web page in a browser here, which would
    /// send an `Origin` header.
    pub from_app_here: bool,
    /// Where the connection came from.
    source: Option<IpAddr>,
    /// A browser sent this, for some web page.
    from_browser: bool,
}

impl HttpRequest {
    /// True if this request may change anything. Reading is open, as the layout sync needs it.
    /// Changes come from a program on this device, or from the other device, whose proxy passes
    /// on what its own app asks for. A web page never qualifies, wherever its browser runs.
    fn may_change_state(&self, peer_ip: &str) -> bool {
        let trusted = self.source.is_some_and(|ip| ip.is_loopback() || ip.to_string() == peer_ip);
        self.method == "GET" || (trusted && !self.from_browser)
    }

    /// True if this request may go on to the other device. Unlike the other device's own proxy
    /// asking here, it would arrive there looking like it came from this device, so a change has
    /// to come from the app on this one.
    fn may_use_proxy(&self) -> bool {
        self.method == "GET" || self.from_app_here
    }
}

/// Node-specific API handler. Returns `Some(json_body)` if the route was handled.
pub type Handler = Arc<dyn Fn(&HttpRequest) -> Option<String> + Send + Sync>;

/// Takes the API port. This comes before anything else in the service: it is what keeps a second
/// one from starting.
pub fn bind() -> Result<TcpListener, std::io::Error> {
    TcpListener::bind(("0.0.0.0", UI_PORT))
}

pub fn start(listener: TcpListener, peer_ip: String, handler: Handler) -> Result<(), String> {
    println!(
        "[Studio] mesh-kvm API listening on port {} (peer proxy -> {}:{})",
        UI_PORT, peer_ip, UI_PORT
    );
    std::thread::Builder::new()
        .name("mesh-kvm-studio".to_string())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let h = Arc::clone(&handler);
                let peer = peer_ip.clone();
                let _ = std::thread::Builder::new()
                    .name("mesh-kvm-studio-conn".to_string())
                    .spawn(move || handle_conn(stream, &peer, h));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    let mut data: Vec<u8> = Vec::with_capacity(4096);
    let mut buf = [0u8; 8192];
    let header_end;
    loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_subslice(&data, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if data.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let headers: Vec<(&str, &str)> = lines.filter_map(|l| l.split_once(':')).collect();
    let header = |name: &str| headers.iter().find(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim());
    let content_len = header("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(4 * 1024 * 1024);
    let from_browser = header("origin").is_some();
    let from_here = stream.peer_addr().is_ok_and(|a| a.ip().is_loopback());
    while data.len() < header_end + content_len {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body_end = (header_end + content_len).min(data.len());
    let body = String::from_utf8_lossy(&data[header_end..body_end]).to_string();
    let source = stream.peer_addr().ok().map(|a| a.ip());
    Some(HttpRequest { method, path, body, from_app_here: from_here && !from_browser, source, from_browser })
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn handle_conn(mut stream: TcpStream, peer_ip: &str, handler: Handler) {
    let req = match read_request(&mut stream) {
        Some(r) => r,
        None => return,
    };

    let proxied = req.path.starts_with("/api/peer");
    if !req.may_change_state(peer_ip) || (proxied && !req.may_use_proxy()) {
        respond(&mut stream, "403 Forbidden", "application/json", REFUSED);
        return;
    }

    if let Some(rest) = req.path.strip_prefix("/api/peer") {
        let target = if rest.is_empty() { "/" } else { rest };
        match http_call(peer_ip, &req.method, target, &req.body) {
            Ok((status, body)) => {
                let st = if status == 200 { "200 OK".to_string() } else { format!("{} Peer", status) };
                respond(&mut stream, &st, "application/json", &body)
            }
            Err(e) => respond(
                &mut stream,
                "502 Bad Gateway",
                "application/json",
                &serde_json::json!({ "error": e, "peer": peer_ip }).to_string(),
            ),
        }
        return;
    }

    if req.path.starts_with("/api/") {
        match handler(&req) {
            Some(body) => respond(&mut stream, "200 OK", "application/json", &body),
            None => respond(&mut stream, "404 Not Found", "application/json", r#"{"error":"not found"}"#),
        }
        return;
    }

    respond(&mut stream, "404 Not Found", "application/json", r#"{"error":"not found"}"#);
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        ctype,
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

/// Minimal blocking HTTP/1.1 client used for the peer proxy.
pub fn http_call(host: &str, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    http_call_port(host, UI_PORT, method, path, body)
}

/// `http_call` to a port other than the Studio's.
pub fn http_call_port(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: &str,
) -> Result<(u16, String), String> {
    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| format!("bad peer address: {}", e))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(350))
        .map_err(|e| format!("peer unreachable: {}", e))?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(1200)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(600)));
    let req = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        method,
        path,
        host,
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).to_string();
    let (head, payload) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed peer response".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(502);
    Ok((status, payload.to_string()))
}

/// Returns the local IPv4 address used to reach the peer (no packets are sent).
pub fn local_ip_toward(peer_ip: &str) -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect(format!("{}:9", peer_ip))?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| UNKNOWN_ADDRESS.to_string())
}

pub fn hostname() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "debian".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends one raw request to a server on `address`, and returns what the handler was told.
    fn seen_by_handler(address: &str, connect_to: &str, request: &'static str) -> bool {
        let listener = TcpListener::bind((address, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = TcpStream::connect((connect_to, port)).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut client = client;
        client.write_all(request.as_bytes()).unwrap();
        read_request(&mut stream).unwrap().from_app_here
    }

    #[test]
    fn only_a_program_on_this_device_counts_as_the_app_here() {
        let plain = "POST /api/local/action HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        let browser = "POST /api/local/action HTTP/1.1\r\nOrigin: http://evil.example\r\nContent-Length: 0\r\n\r\n";
        assert!(seen_by_handler("127.0.0.1", "127.0.0.1", plain));
        assert!(!seen_by_handler("127.0.0.1", "127.0.0.1", browser));
    }

    const PEER: &str = "192.168.8.210";

    fn request(method: &str, path: &str, source: &str, origin: bool) -> HttpRequest {
        let source: IpAddr = source.parse().unwrap();
        HttpRequest {
            method: method.into(),
            path: path.into(),
            body: String::new(),
            from_app_here: source.is_loopback() && !origin,
            source: Some(source),
            from_browser: origin,
        }
    }

    #[test]
    fn changes_come_only_from_this_device_or_the_other_one() {
        let may = |source, origin| request("POST", "/api/layout", source, origin).may_change_state(PEER);
        assert!(may("127.0.0.1", false));
        assert!(may(PEER, false));
        assert!(!may("192.168.8.77", false), "another host on the network");
        assert!(!may("127.0.0.1", true), "a web page on this device");
        assert!(!may(PEER, true), "a web page on the other device");
        // With no peer yet, only this device can change anything.
        assert!(!request("POST", "/api/layout", "192.168.8.77", false).may_change_state(""));
    }

    #[test]
    fn anything_but_a_read_counts_as_a_change_and_reading_stays_open() {
        for method in ["PUT", "DELETE", "PATCH", "OPTIONS"] {
            assert!(!request(method, "/api/layout", "192.168.8.77", false).may_change_state(PEER), "{method}");
        }
        assert!(request("GET", "/api/state", "192.168.8.77", false).may_change_state(PEER));
    }

    #[test]
    fn only_the_app_here_can_send_a_change_through_to_the_other_device() {
        assert!(request("POST", "/api/peer/api/layout", "127.0.0.1", false).may_use_proxy());
        // It would reach the other device from this one's address, and be trusted there.
        assert!(!request("POST", "/api/peer/api/layout", PEER, false).may_use_proxy());
        assert!(!request("POST", "/api/peer/api/layout", "127.0.0.1", true).may_use_proxy());
        assert!(request("GET", "/api/peer/api/local", "192.168.8.77", false).may_use_proxy());
    }

    /// One request over a real socket: its status line and everything else that came back.
    fn exchange(request: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler: Handler = Arc::new(|_| Some(r#"{"ok":true}"#.to_string()));
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(stream, PEER, handler);
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        server.join().unwrap();
        reply
    }

    #[test]
    fn a_web_page_is_turned_away_and_gets_no_permission_to_read_either() {
        let reply = exchange("POST /api/layout HTTP/1.1\r\nOrigin: http://evil.example\r\nContent-Length: 0\r\n\r\n".into());
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        let reply = exchange("POST /api/layout HTTP/1.1\r\nContent-Length: 0\r\n\r\n".into());
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        let reply = exchange("GET /api/state HTTP/1.1\r\nOrigin: http://evil.example\r\n\r\n".into());
        assert!(!reply.to_lowercase().contains("access-control"), "{reply}");
    }

    #[test]
    fn root_serves_no_page() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handler: Handler = Arc::new(|_| None);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(stream, "127.0.0.1", handler);
        });
        let (status, body) = http_call_port("127.0.0.1", addr.port(), "GET", "/", "").unwrap();
        server.join().unwrap();
        assert_eq!(status, 404);
        assert!(!body.contains("<html"));
    }
}
