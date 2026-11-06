//! Cross-platform embedded Studio UI server (TCP 24880).
//!
//! Every node (Debian + Windows) serves the exact same single-page Studio UI and exposes:
//!   GET  /                       -> Studio UI (src/ui/index.html, embedded at compile time)
//!   GET  /api/local              -> this node's live status (node-specific handler)
//!   POST /api/local/action       -> this node's local actions (audio, autostart, focus, panic...)
//!   ANY  /api/peer/<path>        -> transparently proxied to http://<peer_ip>:24880/<path>
//! The Debian node additionally serves the spatial layout master API (/api/state, /api/layout, /api/action).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

pub const UI_PORT: u16 = 24880;
const INDEX_HTML: &str = include_str!("ui/index.html");

pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub body: String,
}

/// Node-specific API handler. Returns `Some(json_body)` if the route was handled.
pub type Handler = Arc<dyn Fn(&HttpRequest) -> Option<String> + Send + Sync>;

pub fn start(peer_ip: String, handler: Handler) -> Result<(), String> {
    let bind_addr = format!("0.0.0.0:{}", UI_PORT);
    let listener =
        TcpListener::bind(&bind_addr).map_err(|e| format!("Failed to bind {}: {}", bind_addr, e))?;
    println!(
        "[Studio] mesh-kvm Studio UI running at http://localhost:{} (peer proxy -> {}:{})",
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
    let content_len = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .next()
        .unwrap_or(0)
        .min(4 * 1024 * 1024);
    while data.len() < header_end + content_len {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body_end = (header_end + content_len).min(data.len());
    let body = String::from_utf8_lossy(&data[header_end..body_end]).to_string();
    Some(HttpRequest { method, path, body })
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn handle_conn(mut stream: TcpStream, peer_ip: &str, handler: Handler) {
    let req = match read_request(&mut stream) {
        Some(r) => r,
        None => return,
    };

    if req.method == "OPTIONS" {
        respond(&mut stream, "204 No Content", "text/plain", "");
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

    respond(&mut stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML);
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
        .unwrap_or_else(|_| "unknown".to_string())
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

/// Opens the Studio UI in a chromeless app window on this device.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // Only a bare Windows launch still uses it.
pub fn open_app_window() {
    let url = format!("http://127.0.0.1:{}", UI_PORT);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        if let Some(edge) = find_edge() {
            if std::process::Command::new(edge)
                .args([&format!("--app={}", url), "--window-size=1480,920"])
                .spawn()
                .is_ok()
            {
                return;
            }
        }
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .creation_flags(0x0800_0000)
            .spawn();
    }

    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/bread".to_string());
        let profile = std::path::PathBuf::from(&home).join(".config/mesh-kvm/studio-profile");
        let _ = std::fs::create_dir_all(profile.join("chrome"));
        let user_js = format!(
            r#"user_pref("toolkit.legacyUserProfileCustomizations.stylesheets", true);
user_pref("browser.tabs.inTitlebar", 0);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage", "{url}");
user_pref("browser.startup.page", 1);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("startup.homepage_welcome_url", "");
user_pref("startup.homepage_override_url", "");
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("app.update.auto", false);
user_pref("browser.toolbars.bookmarks.visibility", "never");
"#
        );
        let _ = std::fs::write(profile.join("user.js"), user_js);
        let _ = std::fs::write(
            profile.join("chrome/userChrome.css"),
            "#TabsToolbar, #nav-bar, #PersonalToolbar, #sidebar-box, #titlebar { visibility: collapse !important; }\n",
        );
        let ff = if std::path::Path::new("/usr/bin/firefox-esr").exists() {
            "firefox-esr"
        } else {
            "firefox"
        };
        let _ = std::process::Command::new(ff)
            .args([
                "--name",
                "mesh-kvm-studio",
                "--class",
                "mesh-kvm-studio",
                "--profile",
                &profile.to_string_lossy(),
                "--new-window",
                &url,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(target_os = "windows")]
pub fn find_edge() -> Option<String> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            candidates.push(format!("{}\\Microsoft\\Edge\\Application\\msedge.exe", base));
        }
    }
    candidates
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}
