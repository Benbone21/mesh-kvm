//! Getting a release's files. Nothing fetched here is believed until `trust` has checked it, and
//! the addresses are built here from the tag, so an answer can't send a download anywhere else.

use serde::Deserialize;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::time::Duration;
use ureq::{http::StatusCode, Agent};

const REPOSITORY: &str = "Benbone21/mesh-kvm";
#[cfg(feature = "test-hooks")]
pub const TEST_SERVER_ENV: &str = "MESH_KVM_TEST_RELEASE_SERVER";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one try at a body lasts. A stalled connection has to end somehow, and whatever
/// arrived stays for the next try, so a slow link still gets there.
const BODY_TIMEOUT: Duration = Duration::from_secs(120);

/// Where releases come from. Only `github` is reachable from a shipped build.
#[derive(Debug, Clone)]
pub struct Server {
    api: String,
    downloads: String,
    /// Refuses anything else, including on a redirect.
    https_only: bool,
}

impl Server {
    pub fn github() -> Self {
        #[cfg(feature = "test-hooks")]
        if let Ok(address) = std::env::var(TEST_SERVER_ENV) {
            return Self::local(&address);
        }
        Self { api: "https://api.github.com".into(), downloads: "https://github.com".into(), https_only: true }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn local(address: &str) -> Self {
        Self { api: address.into(), downloads: address.into(), https_only: false }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// Try again later.
    Network(String),
    /// Nowhere to put it. The partial file is gone by the time this comes back.
    Disk(String),
    /// The server sent more than was signed.
    TooMuch,
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
}

pub struct Source {
    agent: Agent,
    server: Server,
}

impl Source {
    pub fn new(server: Server) -> Self {
        let agent = Agent::config_builder()
            .https_only(server.https_only)
            .user_agent(concat!("mesh-kvm/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(ANSWER_TIMEOUT))
            .timeout_recv_body(Some(BODY_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self { agent, server }
    }

    fn file_url(&self, tag: &str, name: &str) -> String {
        format!("{}/{REPOSITORY}/releases/download/{tag}/{name}", self.server.downloads)
    }

    /// The tag of the latest full release.
    pub fn latest_tag(&self) -> Result<String, Failure> {
        let url = format!("{}/repos/{REPOSITORY}/releases/latest", self.server.api);
        let mut reply = self
            .agent
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| Failure::Network(e.to_string()))?;
        if reply.status() != StatusCode::OK {
            return Err(Failure::Network(format!("status {}", reply.status())));
        }
        let text = reply.body_mut().read_to_string().map_err(|e| Failure::Network(e.to_string()))?;
        let latest: Latest = serde_json::from_str(&text).map_err(|e| Failure::Network(e.to_string()))?;
        Ok(latest.tag_name)
    }

    /// A small file of the release, whole. More than `cap` bytes is refused.
    pub fn small_file(&self, tag: &str, name: &str, cap: u64) -> Result<Vec<u8>, Failure> {
        let mut reply = self.agent.get(&self.file_url(tag, name)).call().map_err(|e| Failure::Network(e.to_string()))?;
        if reply.status() != StatusCode::OK {
            return Err(Failure::Network(format!("status {}", reply.status())));
        }
        let mut bytes = Vec::new();
        reply
            .body_mut()
            .as_reader()
            .take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Failure::Network(e.to_string()))?;
        if bytes.len() as u64 > cap {
            return Err(Failure::TooMuch);
        }
        Ok(bytes)
    }

    /// Fetches into `part` until it holds `size` bytes, carrying on from whatever is there. Anything
    /// that arrives stays, whether or not the connection lasts, so the next try picks it up.
    pub fn download(&self, tag: &str, name: &str, part: &Path, size: u64, progress: &dyn Fn(u64)) -> Result<(), Failure> {
        let mut have = fs::metadata(part).map_or(0, |m| m.len());
        if have > size {
            let _ = fs::remove_file(part);
            have = 0;
        }
        if have == size {
            return Ok(());
        }
        let mut request = self.agent.get(&self.file_url(tag, name));
        if have > 0 {
            request = request.header("Range", format!("bytes={have}-"));
        }
        let mut reply = request.call().map_err(|e| Failure::Network(e.to_string()))?;
        let carries_on = reply.status() == StatusCode::PARTIAL_CONTENT && starts_at(&reply, have);
        if reply.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            let _ = fs::remove_file(part);
            return Err(Failure::Network("the server has no such range".into()));
        }
        if !carries_on && reply.status() != StatusCode::OK {
            return Err(Failure::Network(format!("status {}", reply.status())));
        }
        if !carries_on {
            have = 0;
        }
        let mut file = open_part(part, carries_on).map_err(|e| disk_failure(part, e))?;
        let mut body = reply.body_mut().as_reader();
        let mut buffer = [0u8; 32 * 1024];
        loop {
            let read = match body.read(&mut buffer) {
                Ok(0) if have == size => return Ok(()),
                Ok(0) => return Err(Failure::Network("the connection ended early".into())),
                Ok(read) => read,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(Failure::Network(e.to_string())),
            };
            if have + read as u64 > size {
                let _ = fs::remove_file(part);
                return Err(Failure::TooMuch);
            }
            file.write_all(&buffer[..read]).map_err(|e| disk_failure(part, e))?;
            have += read as u64;
            progress(have);
        }
    }
}

/// True when a partial answer really does start where this download got to.
fn starts_at(reply: &ureq::http::Response<ureq::Body>, have: u64) -> bool {
    let range = reply.headers().get("content-range").and_then(|v| v.to_str().ok()).unwrap_or("");
    range.strip_prefix("bytes ").and_then(|r| r.split('-').next()).and_then(|s| s.parse::<u64>().ok()) == Some(have)
}

fn open_part(part: &Path, carry_on: bool) -> std::io::Result<File> {
    if let Some(dir) = part.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).write(true).append(carry_on).truncate(!carry_on).open(part)
}

fn disk_failure(part: &Path, error: std::io::Error) -> Failure {
    let _ = fs::remove_file(part);
    Failure::Disk(error.to_string())
}

#[cfg(test)]
pub mod fake {
    //! A release server on a local port, and what it can do wrong.

    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct Behaviour {
        pub files: HashMap<String, Vec<u8>>,
        /// The tag `releases/latest` gives, or a failed answer with none.
        pub tag: Option<String>,
        /// Each of the next downloads of a big file stops after this many bytes of body.
        pub cut_after: Vec<usize>,
        /// Ignores `Range` and sends the whole file.
        pub ignores_range: bool,
        /// Every request, as `<path> <range>`.
        pub seen: Vec<String>,
    }

    /// Runs until the test process ends. Each connection gets one request.
    pub fn serve(behaviour: Arc<Mutex<Behaviour>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let behaviour = behaviour.clone();
                std::thread::spawn(move || answer(stream, behaviour));
            }
        });
        address
    }

    fn answer(mut stream: TcpStream, behaviour: Arc<Mutex<Behaviour>>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            if let Some(value) = line.to_lowercase().strip_prefix("range: bytes=") {
                range = value.trim().trim_end_matches('-').parse::<usize>().ok();
            }
        }
        let mut b = behaviour.lock().unwrap();
        b.seen.push(format!("{path} {range:?}"));
        if path.ends_with("/releases/latest") {
            let body = b.tag.as_ref().map(|t| format!(r#"{{"tag_name":"{t}","assets":[{{"browser_download_url":"http://evil.example/x"}}]}}"#));
            return match body {
                Some(body) => reply(&mut stream, "200 OK", "", body.as_bytes(), None),
                None => reply(&mut stream, "503 Unavailable", "", b"", None),
            };
        }
        let Some(file) = b.files.get(path.rsplit('/').next().unwrap()).cloned() else {
            return reply(&mut stream, "404 Not Found", "", b"", None);
        };
        let cut = if file.len() > 1000 && !b.cut_after.is_empty() { Some(b.cut_after.remove(0)) } else { None };
        match range.filter(|_| !b.ignores_range) {
            Some(start) if start >= file.len() => reply(&mut stream, "416 Range Not Satisfiable", "", b"", None),
            Some(start) => {
                let header = format!("Content-Range: bytes {start}-{}/{}\r\n", file.len() - 1, file.len());
                reply(&mut stream, "206 Partial Content", &header, &file[start..], cut)
            }
            None => reply(&mut stream, "200 OK", "", &file, cut),
        }
    }

    fn reply(stream: &mut TcpStream, status: &str, headers: &str, body: &[u8], cut: Option<usize>) {
        let head = format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body[..cut.unwrap_or(body.len()).min(body.len())]);
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{serve, Behaviour};
    use super::*;
    use std::sync::{Arc, Mutex};

    fn big() -> Vec<u8> {
        (0..200_000u32).map(|i| (i % 251) as u8).collect()
    }

    fn part(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-fetch-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("file.part")
    }

    fn server(behaviour: Behaviour) -> (Source, Arc<Mutex<Behaviour>>) {
        let shared = Arc::new(Mutex::new(behaviour));
        (Source::new(Server::local(&serve(shared.clone()))), shared)
    }

    fn with_file(bytes: Vec<u8>) -> Behaviour {
        Behaviour { files: [("file".to_string(), bytes)].into(), ..Default::default() }
    }

    #[test]
    fn the_shipped_address_is_github_over_https() {
        let server = Server::github();
        assert_eq!((server.api.as_str(), server.downloads.as_str()), ("https://api.github.com", "https://github.com"));
        assert!(server.https_only);
    }

    #[test]
    fn a_shipped_client_refuses_plain_http() {
        let mut server = Server::local("http://127.0.0.1:9");
        server.https_only = true;
        assert!(matches!(Source::new(server).latest_tag(), Err(Failure::Network(_))));
    }

    #[test]
    fn the_latest_tag_is_read_and_nothing_else_is_believed() {
        let (source, shared) = server(Behaviour { tag: Some("v0.7.0".into()), ..Default::default() });
        assert_eq!(source.latest_tag().unwrap(), "v0.7.0");
        assert_eq!(shared.lock().unwrap().seen, vec!["/repos/Benbone21/mesh-kvm/releases/latest None"]);
    }

    #[test]
    fn a_failed_answer_is_only_a_network_failure() {
        let (source, _) = server(Behaviour::default());
        assert!(matches!(source.latest_tag(), Err(Failure::Network(_))));
    }

    #[test]
    fn files_come_from_the_tag_and_name_given() {
        let (source, shared) = server(with_file(b"small".to_vec()));
        assert_eq!(source.small_file("v0.7.0", "file", 100).unwrap(), b"small");
        assert_eq!(shared.lock().unwrap().seen, vec!["/Benbone21/mesh-kvm/releases/download/v0.7.0/file None"]);
    }

    #[test]
    fn a_small_file_that_is_too_big_is_refused() {
        let (source, _) = server(with_file(vec![0; 101]));
        assert_eq!(source.small_file("v1", "file", 100), Err(Failure::TooMuch));
    }

    #[test]
    fn a_dropped_download_keeps_what_arrived_and_carries_on_from_there() {
        let bytes = big();
        let (source, shared) = server(Behaviour { cut_after: vec![70_000], ..with_file(bytes.clone()) });
        let path = part("resume");
        let size = bytes.len() as u64;
        assert!(matches!(source.download("v1", "file", &path, size, &|_| ()), Err(Failure::Network(_))));
        assert_eq!(fs::read(&path).unwrap(), &bytes[..70_000]);
        source.download("v1", "file", &path, size, &|_| ()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(shared.lock().unwrap().seen[1].ends_with("Some(70000)"), "{:?}", shared.lock().unwrap().seen);
    }

    #[test]
    fn a_server_that_ignores_the_range_starts_the_file_again() {
        let bytes = big();
        let (source, _) = server(Behaviour { ignores_range: true, ..with_file(bytes.clone()) });
        let path = part("no-range");
        fs::write(&path, &bytes[..500]).unwrap();
        source.download("v1", "file", &path, bytes.len() as u64, &|_| ()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn a_partial_file_longer_than_what_was_signed_is_thrown_away() {
        let bytes = big();
        let (source, _) = server(with_file(bytes.clone()));
        let path = part("too-long");
        fs::write(&path, vec![0; bytes.len() + 5]).unwrap();
        source.download("v1", "file", &path, bytes.len() as u64, &|_| ()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn a_range_the_server_can_not_give_starts_over_next_time() {
        let (source, _) = server(with_file(b"short".to_vec()));
        let path = part("bad-range");
        fs::write(&path, b"longer than the file").unwrap();
        assert!(source.download("v1", "file", &path, 100, &|_| ()).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn more_than_was_signed_stops_the_download_and_deletes_it() {
        let (source, _) = server(with_file(big()));
        let path = part("too-much");
        assert_eq!(source.download("v1", "file", &path, 1000, &|_| ()), Err(Failure::TooMuch));
        assert!(!path.exists());
    }

    #[test]
    fn nowhere_to_write_is_a_disk_failure() {
        let (source, _) = server(with_file(big()));
        let path = part("disk");
        // A file where the folder should be, as disk-full can't be made to happen here.
        let blocked = path.with_file_name("blocker");
        fs::write(&blocked, b"").unwrap();
        let result = source.download("v1", "file", &blocked.join("file.part"), 200_000, &|_| ());
        assert!(matches!(result, Err(Failure::Disk(_))), "{result:?}");
    }

    #[test]
    fn progress_counts_up_to_the_whole_size() {
        let bytes = big();
        let (source, _) = server(with_file(bytes.clone()));
        let seen = Mutex::new(0);
        source.download("v1", "file", &part("progress"), bytes.len() as u64, &|n| *seen.lock().unwrap() = n).unwrap();
        assert_eq!(*seen.lock().unwrap(), bytes.len() as u64);
    }
}
