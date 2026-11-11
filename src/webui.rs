//! Cross-platform local API server (TCP 24880, loopback only), used by the native app.
//!
//! Every node (Debian + Windows) exposes the following. Any other path, including `/`, is a 404:
//!   GET  /api/local              -> this node's live status (node-specific handler)
//!   GET  /api/local/linklog      -> the last lines of this node's link log, as plain text (?lines=N)
//!   GET  /api/local/link         -> this node's connection details, as JSON
//!   POST /api/local/action       -> this node's local actions (audio, autostart, focus, panic...)
//!   ANY  /api/peer/<path>        -> sent to the other device over the encrypted control connection
//!   GET  /api/state              -> this node's copy of the layout, and live telemetry
//!   POST /api/layout             -> an edit to the layout
//!   POST /api/action             -> layout-wide actions (focus, release, game lock...)
//!
//! The other device asks for the same paths over `secure::control`, with the exception of
//! `/api/peer`. This port is never open to the network.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

pub const UI_PORT: u16 = 24880;
/// What `local_ip_toward` gives when there is no route to the address.
pub const UNKNOWN_ADDRESS: &str = "unknown";
const REFUSED: &str = r#"{"ok":false,"error":"Changes can only come from this device or the other device."}"#;
const NOT_FOUND: &str = r#"{"error":"not found"}"#;

/// Where a request came from. `Peer` is made only by `HttpRequest::from_peer`, for requests that
/// arrived over the encrypted control connection: nothing a request says can make one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A program on this device.
    Local,
    /// A web page in a browser, which sends an `Origin` header, or anything else that isn't a
    /// program on this device.
    Browser,
    /// The paired device, authenticated by its key.
    Peer,
}

pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub body: String,
    /// From a program on this device, and not from a web page in a browser here.
    pub from_app_here: bool,
    source: Source,
    /// The `Host` header, if one was sent.
    host: Option<String>,
}

impl HttpRequest {
    /// A request that came in on the control connection, from the paired device.
    pub fn from_peer(method: &str, path: &str, body: &str) -> Self {
        Self::new(method, path, body, Source::Peer, None)
    }

    fn new(method: &str, path: &str, body: &str, source: Source, host: Option<String>) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            body: body.into(),
            from_app_here: source == Source::Local,
            source,
            host,
        }
    }

    pub fn source(&self) -> Source {
        self.source
    }

    /// True if this request may change anything. Reading is open, as the layout sync needs it.
    /// Changes come from a program on this device, or from the other device. A web page never
    /// qualifies, wherever its browser runs.
    fn may_change_state(&self) -> bool {
        self.method == "GET" || self.source != Source::Browser
    }

    /// True if this request may go on to the other device. A change has to come from the app on
    /// this one, and the other device's own requests never go back to it.
    fn may_use_proxy(&self) -> bool {
        self.source != Source::Peer && (self.method == "GET" || self.from_app_here)
    }

    /// A page on another name reaching this port by way of DNS rebinding sends its own name.
    fn host_allowed(&self) -> bool {
        let Some(host) = &self.host else { return true };
        let name = match host.rsplit_once(':') {
            Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => host.as_str(),
        };
        name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost")
    }
}

#[cfg(test)]
impl HttpRequest {
    /// A request as it would arrive from `source`.
    pub fn for_test(method: &str, path: &str, body: &str, source: Source) -> Self {
        Self::new(method, path, body, source, None)
    }
}

/// Node-specific API handler. Returns `Some(json_body)` if the route was handled.
pub type Handler = Arc<dyn Fn(&HttpRequest) -> Option<String> + Send + Sync>;

/// Takes the API port. This comes before anything else in the service: it is what keeps a second
/// one from starting.
pub fn bind() -> Result<TcpListener, std::io::Error> {
    bind_on(UI_PORT)
}

/// Loopback only: the other device comes in by the control connection.
fn bind_on(port: u16) -> Result<TcpListener, std::io::Error> {
    TcpListener::bind(("127.0.0.1", port))
}

pub fn start(listener: TcpListener, peer_ip: String, handler: Handler) -> Result<(), String> {
    println!(
        "[Studio] mesh-kvm API listening on 127.0.0.1:{} (peer: {})",
        UI_PORT,
        if peer_ip.is_empty() { "none yet" } else { &peer_ip }
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
    let host = header("host").map(str::to_string);
    while data.len() < header_end + content_len {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body_end = (header_end + content_len).min(data.len());
    let body = String::from_utf8_lossy(&data[header_end..body_end]).to_string();
    let source = if from_here && !from_browser { Source::Local } else { Source::Browser };
    Some(HttpRequest::new(&method, &path, &body, source, host))
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// What the handler makes of a request that isn't for the other device, as a status and a body.
/// The control connection answers through this too, so both sides draw the same lines.
pub fn answer(req: &HttpRequest, handler: &Handler) -> (u16, String) {
    if !req.may_change_state() || (req.path.starts_with("/api/peer") && !req.may_use_proxy()) {
        return (403, REFUSED.to_string());
    }
    if req.method == "GET" {
        if let Some(text) = crate::linklog::answer(&req.path) {
            return (200, text);
        }
    }
    if req.path.starts_with("/api/") && !req.path.starts_with("/api/peer") {
        if let Some(body) = handler(req) {
            return (200, body);
        }
    }
    (404, NOT_FOUND.to_string())
}

/// Everything is JSON but the link log.
fn content_type(path: &str) -> &'static str {
    if crate::linklog::is_log_path(path) {
        "text/plain; charset=utf-8"
    } else {
        "application/json"
    }
}

fn handle_conn(mut stream: TcpStream, peer_ip: &str, handler: Handler) {
    let req = match read_request(&mut stream) {
        Some(r) => r,
        None => return,
    };

    if !req.host_allowed() {
        respond(&mut stream, "403 Forbidden", "application/json", r#"{"error":"wrong host"}"#);
        return;
    }

    if let Some(rest) = req.path.strip_prefix("/api/peer") {
        if !req.may_change_state() || !req.may_use_proxy() {
            respond(&mut stream, "403 Forbidden", "application/json", REFUSED);
            return;
        }
        let target = if rest.is_empty() { "/" } else { rest };
        use crate::secure::control::{peer_call, peer_call_patiently};
        // The link log can be long, and is most wanted on a link that's slow.
        let patient = req.body.contains("install_update") || crate::linklog::is_log_path(target);
        let call = if patient { peer_call_patiently } else { peer_call };
        match call(&req.method, target, &req.body) {
            Ok((status, body)) => {
                let st = if status == 200 { "200 OK".to_string() } else { format!("{} Peer", status) };
                let ctype = if status == 200 { content_type(target) } else { "application/json" };
                respond(&mut stream, &st, ctype, &body)
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

    let (status, body) = answer(&req, &handler);
    let line = match status {
        200 => "200 OK",
        403 => "403 Forbidden",
        _ => "404 Not Found",
    };
    let ctype = if status == 200 { content_type(&req.path) } else { "application/json" };
    respond(&mut stream, line, ctype, &body);
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

/// Minimal blocking HTTP/1.1 client for this device's own port.
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
    http_call_port_waiting(host, port, method, path, body, Duration::from_millis(1200))
}

/// `http_call_port` with its own wait for the answer, for calls that can take seconds.
pub fn http_call_port_waiting(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    wait: Duration,
) -> Result<(u16, String), String> {
    http_call_within(host, port, method, path, body, Duration::from_millis(350), wait)
}

fn http_call_within(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    connect: Duration,
    read: Duration,
) -> Result<(u16, String), String> {
    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| format!("bad peer address: {}", e))?;
    let mut s = TcpStream::connect_timeout(&addr, connect)
        .map_err(|e| format!("peer unreachable: {}", e))?;
    let _ = s.set_read_timeout(Some(read));
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

    /// Sends one raw request to a server on loopback, and returns what the handler would be told.
    fn seen_by_handler(request: &'static str) -> HttpRequest {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        client.write_all(request.as_bytes()).unwrap();
        read_request(&mut stream).unwrap()
    }

    #[test]
    fn only_a_program_on_this_device_counts_as_the_app_here() {
        let plain = "POST /api/local/action HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        let browser = "POST /api/local/action HTTP/1.1\r\nOrigin: http://evil.example\r\nContent-Length: 0\r\n\r\n";
        assert!(seen_by_handler(plain).from_app_here);
        assert!(!seen_by_handler(browser).from_app_here);
    }

    #[test]
    fn a_header_cannot_make_a_request_a_peer_one() {
        let forged = "POST /api/layout HTTP/1.1\r\nX-Source: peer\r\nSource: Peer\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(seen_by_handler(forged).source, Source::Local);
        let browser = "POST /api/layout HTTP/1.1\r\nOrigin: http://evil.example\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(seen_by_handler(browser).source, Source::Browser);
    }

    fn request(method: &str, path: &str, source: Source) -> HttpRequest {
        HttpRequest::for_test(method, path, "", source)
    }

    #[test]
    fn a_peer_request_may_change_state_and_a_browser_may_not() {
        let may = |source| request("POST", "/api/layout", source).may_change_state();
        assert!(may(Source::Local));
        assert!(may(Source::Peer));
        assert!(!may(Source::Browser), "a web page, wherever its browser runs");
        assert!(request("GET", "/api/state", Source::Browser).may_change_state(), "reading stays open");
    }

    #[test]
    fn anything_but_a_read_counts_as_a_change_and_reading_stays_open() {
        for method in ["PUT", "DELETE", "PATCH", "OPTIONS"] {
            assert!(!request(method, "/api/layout", Source::Browser).may_change_state(), "{method}");
        }
    }

    #[test]
    fn a_peer_request_cannot_use_the_proxy() {
        assert!(request("POST", "/api/peer/api/layout", Source::Local).may_use_proxy());
        assert!(!request("POST", "/api/peer/api/layout", Source::Peer).may_use_proxy());
        assert!(!request("GET", "/api/peer/api/local", Source::Peer).may_use_proxy(), "not even a read");
        assert!(!request("POST", "/api/peer/api/layout", Source::Browser).may_use_proxy());
        assert!(request("GET", "/api/peer/api/local", Source::Browser).may_use_proxy());
    }

    #[test]
    fn only_a_local_program_counts_as_the_app_here() {
        assert!(request("POST", "/api/local/action", Source::Local).from_app_here);
        assert!(!request("POST", "/api/local/action", Source::Peer).from_app_here);
        assert!(!request("POST", "/api/local/action", Source::Browser).from_app_here);
    }

    #[test]
    fn the_peer_api_refuses_the_proxy_and_unknown_paths() {
        let handler: Handler = Arc::new(|req| (req.path == "/api/state").then(|| r#"{"ok":true}"#.to_string()));
        let ask = |method, path| answer(&HttpRequest::from_peer(method, path, ""), &handler).0;
        assert_eq!(ask("GET", "/api/state"), 200);
        assert_eq!(ask("GET", "/api/nothing"), 404);
        assert_eq!(ask("GET", "/"), 404);
        assert_eq!(ask("GET", "/api/peer/api/state"), 403);
        assert_eq!(ask("POST", "/api/peer"), 403);
    }

    #[test]
    fn the_link_log_is_a_read_open_to_both_devices_and_through_the_proxy() {
        let path = "/api/local/linklog?lines=5";
        for source in [Source::Local, Source::Peer, Source::Browser] {
            assert!(request("GET", path, source).may_change_state(), "{source:?}");
        }
        assert!(request("GET", "/api/peer/api/local/linklog", Source::Local).may_use_proxy());
        assert!(request("GET", "/api/peer/api/local/linklog", Source::Browser).may_use_proxy(), "as any read");
        assert!(!request("GET", "/api/peer/api/local/linklog", Source::Peer).may_use_proxy(), "never back to the asker");
        let handler: Handler = Arc::new(|_| None);
        assert_eq!(answer(&HttpRequest::from_peer("GET", path, ""), &handler).0, 200, "the handler needn't know it");
        assert_eq!(answer(&HttpRequest::from_peer("POST", path, ""), &handler).0, 404, "only read");
        assert_eq!(answer(&request("DELETE", path, Source::Browser), &handler).0, 403);
    }

    #[test]
    fn the_connection_details_are_a_read_open_to_both_devices_and_through_the_proxy() {
        let path = crate::linkdetails::PATH;
        for source in [Source::Local, Source::Peer, Source::Browser] {
            assert!(request("GET", path, source).may_change_state(), "{source:?}");
        }
        assert!(request("GET", "/api/peer/api/local/link", Source::Local).may_use_proxy());
        assert!(!request("GET", "/api/peer/api/local/link", Source::Peer).may_use_proxy(), "never back to the asker");
        let handler: Handler = Arc::new(|_| None);
        let (status, body) = answer(&HttpRequest::from_peer("GET", path, ""), &handler);
        assert_eq!(status, 200, "the handler needn't know it");
        assert!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["events"].is_array());
        assert_eq!(answer(&HttpRequest::from_peer("POST", path, ""), &handler).0, 404, "only read");
        assert_eq!(answer(&request("DELETE", path, Source::Browser), &handler).0, 403);
        let reply = exchange(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"));
        assert!(reply.starts_with("HTTP/1.1 200") && reply.contains("Content-Type: application/json"), "{reply}");
    }

    #[test]
    fn the_link_log_comes_as_plain_text() {
        let reply = exchange("GET /api/local/linklog?lines=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".into());
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.contains("Content-Type: text/plain; charset=utf-8"), "{reply}");
        let reply = exchange("GET /api/state HTTP/1.1\r\n\r\n".into());
        assert!(reply.contains("Content-Type: application/json"), "{reply}");
    }

    #[test]
    fn the_port_is_bound_to_loopback_only() {
        let listener = bind_on(0).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }

    /// One request over a real socket: its status line and everything else that came back.
    fn exchange(request: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler: Handler = Arc::new(|_| Some(r#"{"ok":true}"#.to_string()));
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(stream, "192.168.8.210", handler);
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
    fn a_wrong_host_is_refused() {
        let get = |host: &str| exchange(format!("GET /api/state HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        assert!(get("evil.example").starts_with("HTTP/1.1 403"));
        assert!(get("evil.example:24880").starts_with("HTTP/1.1 403"));
        assert!(get("127.0.0.1.evil.example").starts_with("HTTP/1.1 403"));
        assert!(get("127.0.0.1:24880").starts_with("HTTP/1.1 200"));
        assert!(get("127.0.0.1").starts_with("HTTP/1.1 200"));
        assert!(get("localhost:24880").starts_with("HTTP/1.1 200"));
        assert!(get("LocalHost").starts_with("HTTP/1.1 200"));
        let none = exchange("GET /api/state HTTP/1.1\r\n\r\n".into());
        assert!(none.starts_with("HTTP/1.1 200"), "http_call always sends one, but a missing one is no rebinding");
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
