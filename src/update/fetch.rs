//! Getting a release's files. Nothing fetched here is believed until `trust` has checked it, and
//! the addresses are built here from the tag, so an answer can't send a download anywhere else.

use serde::Deserialize;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use ureq::http::{Response, StatusCode};
use ureq::{Agent, Body};

const REPOSITORY: &str = "Benbone21/mesh-kvm";
#[cfg(feature = "test-hooks")]
pub const TEST_SERVER_ENV: &str = "MESH_KVM_TEST_RELEASE_SERVER";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one try at a body lasts. A stalled connection has to end somehow, and whatever
/// arrived stays for the next try, so a slow link still gets there.
const BODY_TIMEOUT: Duration = Duration::from_secs(120);
/// GitHub sends a download to its storage in one hop.
const MOST_REDIRECTS: u32 = 5;
/// Beside a partial download: what the server called that file, to be sure a resume is the same one.
const VALIDATOR_SUFFIX: &str = ".etag";

/// Where releases come from. Only `github` is reachable from a shipped build.
#[derive(Debug, Clone)]
pub struct Server {
    api: String,
    downloads: String,
    /// The one address that may be plain HTTP, for a local test server. Everything else, redirects
    /// included, has to be HTTPS. A shipped build has none.
    plain_address: Option<String>,
}

impl Server {
    pub fn github() -> Self {
        #[cfg(feature = "test-hooks")]
        if let Ok(address) = std::env::var(TEST_SERVER_ENV) {
            return Self::local(&address);
        }
        Self { api: "https://api.github.com".into(), downloads: "https://github.com".into(), plain_address: None }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn local(address: &str) -> Self {
        Self { api: address.into(), downloads: address.into(), plain_address: Some(address.into()) }
    }

    fn allows(&self, url: &str) -> bool {
        let plain = self.plain_address.as_ref().is_some_and(|address| url.strip_prefix(address.as_str()).is_some_and(|rest| rest.starts_with('/')));
        url.starts_with("https://") || plain
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
            .max_redirects(0)
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

    /// One GET, following redirects here and not in the agent, so that every hop is looked at.
    fn get(&self, url: &str, headers: &[(&str, String)]) -> Result<Response<Body>, Failure> {
        let mut url = url.to_string();
        for _ in 0..=MOST_REDIRECTS {
            if !self.server.allows(&url) {
                return Err(Failure::Network(format!("{url} isn't HTTPS")));
            }
            let mut request = self.agent.get(&url);
            for (name, value) in headers {
                request = request.header(*name, value);
            }
            let reply = request.call().map_err(|e| Failure::Network(e.to_string()))?;
            if !reply.status().is_redirection() {
                return Ok(reply);
            }
            let location = reply.headers().get("location").and_then(|v| v.to_str().ok());
            url = location.ok_or_else(|| Failure::Network("a redirect to nowhere".into()))?.to_string();
        }
        Err(Failure::Network("too many redirects".into()))
    }

    /// The tag of the latest full release.
    pub fn latest_tag(&self) -> Result<String, Failure> {
        let url = format!("{}/repos/{REPOSITORY}/releases/latest", self.server.api);
        let mut reply = self.get(&url, &[("Accept", "application/vnd.github+json".to_string())])?;
        if reply.status() != StatusCode::OK {
            return Err(Failure::Network(format!("status {}", reply.status())));
        }
        let text = reply.body_mut().read_to_string().map_err(|e| Failure::Network(e.to_string()))?;
        let latest: Latest = serde_json::from_str(&text).map_err(|e| Failure::Network(e.to_string()))?;
        Ok(latest.tag_name)
    }

    /// A small file of the release, whole. More than `cap` bytes is refused.
    pub fn small_file(&self, tag: &str, name: &str, cap: u64) -> Result<Vec<u8>, Failure> {
        let mut reply = self.get(&self.file_url(tag, name), &[])?;
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
    /// that arrives stays, whether or not the connection lasts, so the next try picks it up. A file
    /// that has changed on the server since then starts again, by way of `If-Range`.
    pub fn download(&self, tag: &str, name: &str, part: &Path, size: u64, progress: &dyn Fn(u64)) -> Result<(), Failure> {
        let mut have = fs::metadata(part).map_or(0, |m| m.len());
        if have > size {
            forget_part(part);
            have = 0;
        }
        if have == size {
            forget_validator(part);
            return Ok(());
        }
        let mut headers = Vec::new();
        if have > 0 {
            headers.push(("Range", format!("bytes={have}-")));
            if let Ok(validator) = fs::read_to_string(validator_path(part)) {
                headers.push(("If-Range", validator));
            }
        }
        let mut reply = self.get(&self.file_url(tag, name), &headers)?;
        let carries_on = reply.status() == StatusCode::PARTIAL_CONTENT && starts_at(&reply, have);
        if reply.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            forget_part(part);
            return Err(Failure::Network("the server has no such range".into()));
        }
        if !carries_on && reply.status() != StatusCode::OK {
            return Err(Failure::Network(format!("status {}", reply.status())));
        }
        if !carries_on {
            have = 0;
        }
        let mut file = open_part(part, carries_on).map_err(|e| disk_failure(part, e))?;
        if !carries_on {
            keep_validator(part, &reply);
        }
        let mut body = reply.body_mut().as_reader();
        let mut buffer = [0u8; 32 * 1024];
        loop {
            let read = match body.read(&mut buffer) {
                Ok(0) if have == size => {
                    forget_validator(part);
                    return Ok(());
                }
                Ok(0) => return Err(Failure::Network("the connection ended early".into())),
                Ok(read) => read,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(Failure::Network(e.to_string())),
            };
            if have + read as u64 > size {
                forget_part(part);
                return Err(Failure::TooMuch);
            }
            file.write_all(&buffer[..read]).map_err(|e| disk_failure(part, e))?;
            have += read as u64;
            progress(have);
        }
    }
}

fn validator_path(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(VALIDATOR_SUFFIX);
    PathBuf::from(name)
}

/// Keeps what the server calls this file. A weak tag is no use, as `If-Range` needs a strong one.
fn keep_validator(part: &Path, reply: &Response<Body>) {
    match reply.headers().get("etag").and_then(|v| v.to_str().ok()).filter(|tag| !tag.starts_with("W/")) {
        Some(tag) => drop(fs::write(validator_path(part), tag)),
        None => forget_validator(part),
    }
}

fn forget_validator(part: &Path) {
    let _ = fs::remove_file(validator_path(part));
}

fn forget_part(part: &Path) {
    let _ = fs::remove_file(part);
    forget_validator(part);
}

/// True when a partial answer really does start where this download got to.
fn starts_at(reply: &Response<Body>, have: u64) -> bool {
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
    forget_part(part);
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
        /// The next this many downloads of a file come back damaged.
        pub damage_next: u32,
        /// Sent as `ETag` with a file, and compared with `If-Range`.
        pub etag: Option<String>,
        /// Each request's `If-Range`, in turn.
        pub if_range: Vec<Option<String>>,
        /// Answers one path with a redirect to this address.
        pub redirect: Option<(String, String)>,
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
        let mut if_range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            if let Some(value) = line.to_lowercase().strip_prefix("range: bytes=") {
                range = value.trim().trim_end_matches('-').parse::<usize>().ok();
            }
            if line.to_lowercase().starts_with("if-range:") {
                if_range = Some(line["if-range:".len()..].trim().to_string());
            }
        }
        let mut b = behaviour.lock().unwrap();
        b.seen.push(format!("{path} {range:?}"));
        b.if_range.push(if_range.clone());
        if let Some((_, to)) = b.redirect.clone().filter(|(from, _)| path.ends_with(from.as_str())) {
            return reply(&mut stream, "302 Found", &format!("Location: {to}\r\n"), b"", None);
        }
        if path.ends_with("/releases/latest") {
            let body = b.tag.as_ref().map(|t| format!(r#"{{"tag_name":"{t}","assets":[{{"browser_download_url":"http://evil.example/x"}}]}}"#));
            return match body {
                Some(body) => reply(&mut stream, "200 OK", "", body.as_bytes(), None),
                None => reply(&mut stream, "503 Unavailable", "", b"", None),
            };
        }
        let name = path.rsplit('/').next().unwrap();
        let Some(mut file) = b.files.get(name).cloned() else {
            return reply(&mut stream, "404 Not Found", "", b"", None);
        };
        if b.damage_next > 0 && !name.starts_with("release.json") {
            b.damage_next -= 1;
            file[0] ^= 1;
        }
        let cut = if file.len() > 1000 && !b.cut_after.is_empty() { Some(b.cut_after.remove(0)) } else { None };
        let etag = b.etag.as_ref().map_or_else(String::new, |tag| format!("ETag: {tag}\r\n"));
        let unchanged = if_range.is_none() || if_range == b.etag;
        match range.filter(|_| !b.ignores_range && unchanged) {
            Some(start) if start >= file.len() => reply(&mut stream, "416 Range Not Satisfiable", "", b"", None),
            Some(start) => {
                let header = format!("{etag}Content-Range: bytes {start}-{}/{}\r\n", file.len() - 1, file.len());
                reply(&mut stream, "206 Partial Content", &header, &file[start..], cut)
            }
            None => reply(&mut stream, "200 OK", &etag, &file, cut),
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

    /// A real server the client would happily talk to, if it weren't told not to.
    fn shipped_rules(address: &str) -> Source {
        let mut server = Server::local(address);
        server.plain_address = None;
        Source::new(server)
    }

    #[test]
    fn the_shipped_address_is_github_over_https() {
        let server = Server::github();
        assert_eq!((server.api.as_str(), server.downloads.as_str()), ("https://api.github.com", "https://github.com"));
        assert_eq!(server.plain_address, None);
    }

    #[test]
    fn a_shipped_client_refuses_plain_http() {
        let shared = Arc::new(Mutex::new(Behaviour { tag: Some("v0.7.0".into()), ..Default::default() }));
        let source = shipped_rules(&serve(shared.clone()));
        assert!(matches!(source.latest_tag(), Err(Failure::Network(_))));
        assert!(shared.lock().unwrap().seen.is_empty(), "it never got as far as asking");
    }

    #[test]
    fn a_redirect_to_plain_http_is_refused() {
        let elsewhere = Arc::new(Mutex::new(with_file(b"from elsewhere".to_vec())));
        let to = format!("{}/x/file", serve(elsewhere.clone()));
        let (source, _) = server(Behaviour { redirect: Some(("/file".into(), to)), ..with_file(b"file".to_vec()) });
        assert!(matches!(source.small_file("v1", "file", 100), Err(Failure::Network(_))));
        assert!(elsewhere.lock().unwrap().seen.is_empty(), "it never went there");
    }

    #[test]
    fn a_redirect_within_what_is_allowed_is_followed_with_its_headers() {
        let bytes = big();
        let shared = Arc::new(Mutex::new(with_file(bytes.clone())));
        let address = serve(shared.clone());
        shared.lock().unwrap().redirect = Some(("/releases/download/v1/file".into(), format!("{address}/storage/file")));
        let source = Source::new(Server::local(&address));
        let path = part("redirect");
        fs::write(&path, &bytes[..500]).unwrap();
        source.download("v1", "file", &path, bytes.len() as u64, &|_| ()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(shared.lock().unwrap().seen[1].ends_with("Some(500)"), "the range went along");
    }

    #[test]
    fn a_resume_asks_for_the_same_file_and_starts_again_if_it_changed() {
        let bytes = big();
        let (source, shared) = server(Behaviour { cut_after: vec![70_000], etag: Some("\"one\"".into()), ..with_file(bytes.clone()) });
        let path = part("etag");
        let size = bytes.len() as u64;
        assert!(source.download("v1", "file", &path, size, &|_| ()).is_err());
        {
            let mut b = shared.lock().unwrap();
            b.etag = Some("\"two\"".into());
            b.files.insert("file".into(), vec![9; bytes.len()]);
        }
        source.download("v1", "file", &path, size, &|_| ()).unwrap();
        assert_eq!(shared.lock().unwrap().if_range[1].as_deref(), Some("\"one\""));
        assert_eq!(fs::read(&path).unwrap(), vec![9; bytes.len()], "no mix of the two");
        assert!(!validator_path(&path).exists(), "nothing left beside it");
    }

    #[test]
    fn a_weak_tag_is_not_used_to_resume() {
        let bytes = big();
        let (source, shared) = server(Behaviour { cut_after: vec![70_000], etag: Some("W/\"weak\"".into()), ..with_file(bytes.clone()) });
        let path = part("weak");
        let _ = source.download("v1", "file", &path, bytes.len() as u64, &|_| ());
        let _ = source.download("v1", "file", &path, bytes.len() as u64, &|_| ());
        assert_eq!(shared.lock().unwrap().if_range[1], None);
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
