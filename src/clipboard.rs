use arboard::{Clipboard, ImageData};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// This device's clipboard goes to the peer.
pub static CLIPBOARD_SEND: AtomicBool = AtomicBool::new(true);
/// The peer's clipboard is taken in here.
pub static CLIPBOARD_RECEIVE: AtomicBool = AtomicBool::new(true);

/// Copied files travel the moment they are copied, whether or not they get pasted, so a copy
/// larger than this stays put.
const MAX_COPY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_COPY_ENTRIES: usize = 100_000;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// The batch on the clipboard must outlive the paste, and the one before it may still be copying.
const KEEP_BATCHES: usize = 2;
const CHUNK_BYTES: usize = 256 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(350);
const CLIPBOARD_RETRY: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// The newest copy wins: an older file transfer stops when either count moves on.
static NEWEST_SEND: AtomicU64 = AtomicU64::new(0);
static NEWEST_RECEIVE: AtomicU64 = AtomicU64::new(0);
static BATCH_COUNT: AtomicU64 = AtomicU64::new(0);
static NOTE: Mutex<String> = Mutex::new(String::new());

/// What the clipboard last did, for the app to show.
pub fn clipboard_note() -> String {
    NOTE.lock().map(|n| n.clone()).unwrap_or_default()
}

fn set_note(note: impl Into<String>) {
    let note = note.into();
    println!("[Clipboard] {note}");
    if let Ok(mut current) = NOTE.lock() {
        *current = note;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative to the copy, with `/` between folders on every device.
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClipboardPayload {
    Text(String),
    Image {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
    /// The contents of each file follow the frame, in order.
    Files(Vec<FileEntry>),
}

fn hash_bytes(data: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

/// Paths and their sizes and times, so copying a file again after editing it counts as a change.
fn hash_files(paths: &[PathBuf]) -> u64 {
    let mut hasher = DefaultHasher::new();
    let sorted: BTreeSet<&PathBuf> = paths.iter().collect();
    for path in sorted {
        path.hash(&mut hasher);
        if let Ok(meta) = fs::metadata(path) {
            meta.len().hash(&mut hasher);
            meta.modified().ok().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Remembers the clipboard's last contents. What is there at startup is not a copy, so the first
/// look only takes note of it.
#[derive(Default)]
struct Tracker {
    last: Option<u64>,
}

impl Tracker {
    fn is_new(&mut self, fingerprint: u64) -> bool {
        let new = self.last.is_some_and(|last| last != fingerprint);
        self.last = Some(fingerprint);
        new
    }

    fn mark(&mut self, fingerprint: u64) {
        self.last = Some(fingerprint);
    }
}

enum Snapshot {
    Files(Vec<PathBuf>),
    Text(String),
    Image(ImageData<'static>),
}

impl Snapshot {
    fn fingerprint(&self) -> u64 {
        match self {
            Snapshot::Files(paths) => hash_files(paths),
            Snapshot::Text(text) => hash_bytes(text.as_bytes()),
            Snapshot::Image(image) => hash_bytes(&image.bytes),
        }
    }
}

/// One look at the clipboard, taking a single kind so that a copy offering several is one change.
/// File managers also offer copied files as text, so files come first.
fn read_snapshot(cb: &mut Clipboard) -> Option<Snapshot> {
    if let Ok(paths) = cb.get().file_list() {
        if !paths.is_empty() {
            return Some(Snapshot::Files(paths));
        }
    }
    if let Ok(text) = cb.get_text() {
        if !text.is_empty() {
            return Some(Snapshot::Text(text));
        }
    }
    cb.get_image().ok().filter(|i| !i.bytes.is_empty()).map(Snapshot::Image)
}

fn file_count(entries: &[FileEntry]) -> String {
    match entries.iter().filter(|e| !e.is_dir).count() {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    }
}

/// Lists a copy for sending. Links are left out: they could lead anywhere on this device.
fn collect_entries(paths: &[PathBuf]) -> Result<Vec<(Option<PathBuf>, FileEntry)>, String> {
    fn walk(
        path: &Path,
        name: String,
        out: &mut Vec<(Option<PathBuf>, FileEntry)>,
        total: &mut u64,
    ) -> Result<(), String> {
        let meta = fs::symlink_metadata(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
        if out.len() >= MAX_COPY_ENTRIES {
            return Err(format!("Too many files to send. The limit is {MAX_COPY_ENTRIES}."));
        }
        if meta.is_dir() {
            out.push((None, FileEntry { path: name.clone(), size: 0, is_dir: true }));
            let children = fs::read_dir(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
            for child in children {
                let child = child.map_err(|e| format!("Couldn't read {name}: {e}"))?;
                let child_name = format!("{name}/{}", child.file_name().to_string_lossy());
                walk(&child.path(), child_name, out, total)?;
            }
        } else if meta.is_file() {
            *total += meta.len();
            if *total > MAX_COPY_BYTES {
                return Err("Too large to send. The limit is 2 GB per copy.".to_string());
            }
            let entry = FileEntry { path: name, size: meta.len(), is_dir: false };
            out.push((Some(path.to_path_buf()), entry));
        }
        Ok(())
    }

    let mut out = Vec::new();
    let mut total = 0;
    for path in paths {
        let Some(name) = path.file_name() else { continue };
        walk(path, name.to_string_lossy().into_owned(), &mut out, &mut total)?;
    }
    Ok(out)
}

fn write_frame<W: Write>(writer: &mut W, payload: &ClipboardPayload) -> Result<(), String> {
    let encoded = bincode::serialize(payload).map_err(|e| e.to_string())?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err("Too large to send.".to_string());
    }
    writer.write_all(&(encoded.len() as u32).to_le_bytes()).map_err(|e| e.to_string())?;
    writer.write_all(&encoded).map_err(|e| e.to_string())
}

fn read_frame<R: Read>(reader: &mut R) -> Result<ClipboardPayload, String> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(|e| e.to_string())?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(format!("Refused a frame of {len} bytes"));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
    bincode::deserialize(&buf).map_err(|e| e.to_string())
}

/// Writes each file's contents. A file that shrank mid-copy fails the transfer, since the receiver
/// counts bytes and would otherwise take the next file's bytes as this one's.
fn write_contents<W: Write>(
    writer: &mut W,
    sources: &[(Option<PathBuf>, FileEntry)],
    cancelled: impl Fn() -> bool,
) -> Result<(), String> {
    let mut chunk = vec![0u8; CHUNK_BYTES];
    for (source, entry) in sources {
        let Some(source) = source else { continue };
        let mut file = File::open(source).map_err(|e| format!("Couldn't read {}: {e}", entry.path))?;
        let mut left = entry.size;
        while left > 0 {
            if cancelled() {
                return Err("Replaced by a newer copy".to_string());
            }
            let want = left.min(CHUNK_BYTES as u64) as usize;
            let got = file.read(&mut chunk[..want]).map_err(|e| e.to_string())?;
            if got == 0 {
                return Err(format!("{} changed while sending", entry.path));
            }
            writer.write_all(&chunk[..got]).map_err(|e| e.to_string())?;
            left -= got as u64;
        }
    }
    Ok(())
}

/// One folder or file name from the peer, made safe to create here. Characters Windows refuses
/// become `_`; anything that would climb out of the batch folder is refused outright.
fn clean_component(name: &str) -> Option<String> {
    if matches!(name, "" | "." | "..") {
        return None;
    }
    let replaced: String = name
        .chars()
        .map(|c| if c.is_control() || r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    let trimmed = replaced.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return None;
    }
    let stem = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.ends_with(|c: char| c.is_ascii_digit()));
    Some(if reserved { format!("_{trimmed}") } else { trimmed.to_string() })
}

fn clean_relative(path: &str) -> Option<PathBuf> {
    let mut clean = PathBuf::new();
    for name in path.split('/') {
        clean.push(clean_component(name)?);
    }
    Some(clean)
}

/// Reads a copy's contents into `batch`, returning what belongs on the clipboard: the top-level
/// files and folders, in the order they came.
fn read_contents<R: Read>(
    reader: &mut R,
    entries: &[FileEntry],
    batch: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<PathBuf>, String> {
    let total = entries.iter().try_fold(0u64, |sum, e| sum.checked_add(e.size));
    if entries.len() > MAX_COPY_ENTRIES || total.is_none_or(|t| t > MAX_COPY_BYTES) {
        return Err("Refused a copy over the limit".to_string());
    }
    fs::create_dir_all(batch).map_err(|e| e.to_string())?;
    let mut top = Vec::new();
    let mut chunk = vec![0u8; CHUNK_BYTES];
    for entry in entries {
        let relative = clean_relative(&entry.path)
            .ok_or_else(|| format!("Refused an unsafe path: {}", entry.path))?;
        let target = batch.join(&relative);
        if let Some(first) = relative.components().next() {
            let first = batch.join(first);
            if !top.contains(&first) {
                top.push(first);
            }
        }
        if entry.is_dir {
            fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| format!("Couldn't save {}: {e}", entry.path))?;
        let mut left = entry.size;
        while left > 0 {
            if cancelled() {
                return Err("Replaced by a newer copy".to_string());
            }
            let want = left.min(CHUNK_BYTES as u64) as usize;
            let got = reader.read(&mut chunk[..want]).map_err(|e| e.to_string())?;
            if got == 0 {
                return Err("The connection ended mid-copy".to_string());
            }
            file.write_all(&chunk[..got]).map_err(|e| format!("Couldn't save {}: {e}", entry.path))?;
            left -= got as u64;
        }
    }
    Ok(top)
}

fn inbox_dir() -> PathBuf {
    crate::settings::config_dir().join("received")
}

fn new_batch_dir(inbox: &Path) -> PathBuf {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let count = BATCH_COUNT.fetch_add(1, Ordering::SeqCst);
    inbox.join(format!("{millis:020}-{count:06}"))
}

/// Removes all but the newest batches. Names sort by age.
fn prune_inbox(inbox: &Path, keep: usize) {
    let Ok(dir) = fs::read_dir(inbox) else { return };
    let mut batches: Vec<PathBuf> = dir.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    batches.sort();
    let extra = batches.len().saturating_sub(keep);
    for batch in &batches[..extra] {
        let _ = fs::remove_dir_all(batch);
    }
}

/// The clipboard can be briefly held by another program, on Windows especially.
fn with_retries(mut attempt: impl FnMut(&mut Clipboard) -> Result<(), arboard::Error>) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..5 {
        match Clipboard::new().and_then(|mut cb| attempt(&mut cb)) {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(last)
}

fn receive(mut stream: TcpStream, tracker: &Mutex<Tracker>, inbox: &Path) -> Result<(), String> {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let turn = NEWEST_RECEIVE.fetch_add(1, Ordering::SeqCst) + 1;
    let mark = |fingerprint: u64| {
        if let Ok(mut guard) = tracker.lock() {
            guard.mark(fingerprint);
        }
    };
    match read_frame(&mut stream)? {
        ClipboardPayload::Text(text) => {
            mark(hash_bytes(text.as_bytes()));
            with_retries(|cb| cb.set_text(text.as_str()))?;
            set_note("Received text");
        }
        ClipboardPayload::Image { width, height, bytes } => {
            mark(hash_bytes(&bytes));
            with_retries(|cb| {
                cb.set_image(ImageData { width, height, bytes: Cow::Borrowed(&bytes) })
            })?;
            set_note("Received an image");
        }
        ClipboardPayload::Files(entries) => {
            let count = file_count(&entries);
            set_note(format!("Receiving {count}\u{2026}"));
            let batch = new_batch_dir(inbox);
            let cancelled = || NEWEST_RECEIVE.load(Ordering::SeqCst) != turn;
            let top = match read_contents(&mut stream, &entries, &batch, cancelled) {
                Ok(top) => top,
                Err(e) => {
                    let _ = fs::remove_dir_all(&batch);
                    return Err(e);
                }
            };
            mark(hash_files(&top));
            with_retries(|cb| cb.set().file_list(&top))?;
            prune_inbox(inbox, KEEP_BATCHES);
            set_note(format!("Received {count}"));
        }
    }
    Ok(())
}

pub fn start_clipboard_sync(peer_ip: String, port: u16) -> Result<(), String> {
    let peer: IpAddr = peer_ip.parse().map_err(|e| format!("Bad peer address {peer_ip}: {e}"))?;
    let tracker = Arc::new(Mutex::new(Tracker::default()));
    let tracker_rx = Arc::clone(&tracker);

    // 1. Spawn TCP receiver on Port 24852
    std::thread::Builder::new()
        .name("mesh-kvm-clip-rx".to_string())
        .spawn(move || {
            let bind_addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
            let listener = match TcpListener::bind(bind_addr) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[Clipboard] Failed to bind TCP {}: {}", bind_addr, e);
                    return;
                }
            };
            println!("[Clipboard] Unified clipboard sync listening on TCP {}", bind_addr);

            let inbox = inbox_dir();
            for stream in listener.incoming().flatten() {
                if !CLIPBOARD_RECEIVE.load(Ordering::SeqCst) {
                    continue;
                }
                // Files land on disk, so only the peer gets to write here.
                if stream.peer_addr().map(|a| a.ip()).ok() != Some(peer) {
                    continue;
                }
                // Each on its own thread: text copied during a long transfer must not wait for it.
                let tracker = Arc::clone(&tracker_rx);
                let inbox = inbox.clone();
                let _ = std::thread::Builder::new().name("mesh-kvm-clip-in".to_string()).spawn(
                    move || {
                        if let Err(e) = receive(stream, &tracker, &inbox) {
                            set_note(format!("Couldn't receive the clipboard: {e}"));
                        }
                    },
                );
            }
        })
        .map_err(|e| e.to_string())?;

    // 2. Spawn clipboard watcher and TCP sender
    std::thread::Builder::new()
        .name("mesh-kvm-clip-tx".to_string())
        .spawn(move || {
            let target_addr = SocketAddr::new(peer, port);
            let inbox = inbox_dir();

            // The session may not be up yet when the service starts.
            let mut cb = loop {
                match Clipboard::new() {
                    Ok(c) => break c,
                    Err(e) => {
                        set_note(format!("Waiting for the clipboard: {e}"));
                        std::thread::sleep(CLIPBOARD_RETRY);
                    }
                }
            };

            loop {
                std::thread::sleep(POLL_INTERVAL);

                let Some(snapshot) = read_snapshot(&mut cb) else { continue };
                let fingerprint = snapshot.fingerprint();
                let changed = tracker.lock().map(|mut t| t.is_new(fingerprint)).unwrap_or(false);

                // Tracked even with sending off, so that what was copied meanwhile does not go out
                // the moment sending comes back on.
                if changed && CLIPBOARD_SEND.load(Ordering::SeqCst) {
                    send_snapshot(target_addr, snapshot, &inbox);
                }
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Sends off the watching thread, so that a long transfer doesn't hide the next copy.
fn send_snapshot(target_addr: SocketAddr, snapshot: Snapshot, inbox: &Path) {
    // Files that came from the peer stay put, or a copy would bounce between the devices.
    if let Snapshot::Files(paths) = &snapshot {
        if paths.iter().all(|p| p.starts_with(inbox)) {
            return;
        }
    }
    let turn = NEWEST_SEND.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = std::thread::Builder::new().name("mesh-kvm-clip-out".to_string()).spawn(move || {
        if let Err(e) = send(target_addr, snapshot, turn) {
            if NEWEST_SEND.load(Ordering::SeqCst) == turn {
                set_note(format!("Couldn't send the clipboard: {e}"));
            }
        }
    });
}

fn connect(target_addr: SocketAddr) -> Result<TcpStream, String> {
    let stream = TcpStream::connect_timeout(&target_addr, Duration::from_millis(500))
        .map_err(|e| e.to_string())?;
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    Ok(stream)
}

fn send(target_addr: SocketAddr, snapshot: Snapshot, turn: u64) -> Result<(), String> {
    match snapshot {
        Snapshot::Text(text) => {
            write_frame(&mut connect(target_addr)?, &ClipboardPayload::Text(text))?;
            set_note("Sent text");
        }
        Snapshot::Image(image) => {
            let payload = ClipboardPayload::Image {
                width: image.width,
                height: image.height,
                bytes: image.bytes.into_owned(),
            };
            write_frame(&mut connect(target_addr)?, &payload)?;
            set_note("Sent an image");
        }
        Snapshot::Files(paths) => {
            let sources = collect_entries(&paths)?;
            let entries: Vec<FileEntry> = sources.iter().map(|(_, e)| e.clone()).collect();
            let count = file_count(&entries);
            set_note(format!("Sending {count}\u{2026}"));
            let mut stream = connect(target_addr)?;
            write_frame(&mut stream, &ClipboardPayload::Files(entries))?;
            write_contents(&mut stream, &sources, || NEWEST_SEND.load(Ordering::SeqCst) != turn)?;
            set_note(format!("Sent {count}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-clip-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn what_is_on_the_clipboard_at_startup_is_not_a_copy() {
        let mut tracker = Tracker::default();
        assert!(!tracker.is_new(1));
        assert!(!tracker.is_new(1));
        assert!(tracker.is_new(2));
    }

    #[test]
    fn what_came_from_the_peer_is_not_sent_back() {
        let mut tracker = Tracker::default();
        tracker.mark(7);
        assert!(!tracker.is_new(7));
    }

    #[test]
    fn files_and_folders_arrive_whole() {
        let root = scratch("whole");
        let source = root.join("source");
        fs::create_dir_all(source.join("album/empty")).unwrap();
        fs::write(source.join("album/song.bin"), vec![7u8; CHUNK_BYTES * 2 + 5]).unwrap();
        fs::write(source.join("note.txt"), b"hello").unwrap();
        fs::write(source.join("nothing.txt"), b"").unwrap();

        let paths = vec![source.join("album"), source.join("note.txt"), source.join("nothing.txt")];
        let sources = collect_entries(&paths).unwrap();
        let entries: Vec<FileEntry> = sources.iter().map(|(_, e)| e.clone()).collect();

        let mut wire = Vec::new();
        write_frame(&mut wire, &ClipboardPayload::Files(entries)).unwrap();
        write_contents(&mut wire, &sources, || false).unwrap();

        let mut reader = Cursor::new(wire);
        let ClipboardPayload::Files(entries) = read_frame(&mut reader).unwrap() else {
            panic!("expected files");
        };
        let batch = root.join("batch");
        let top = read_contents(&mut reader, &entries, &batch, || false).unwrap();

        assert_eq!(top, vec![batch.join("album"), batch.join("note.txt"), batch.join("nothing.txt")]);
        assert_eq!(fs::read(batch.join("album/song.bin")).unwrap().len(), CHUNK_BYTES * 2 + 5);
        assert_eq!(fs::read(batch.join("note.txt")).unwrap(), b"hello");
        assert!(fs::read(batch.join("nothing.txt")).unwrap().is_empty());
        assert!(batch.join("album/empty").is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_left_out_of_a_copy() {
        let root = scratch("links");
        fs::create_dir_all(root.join("folder")).unwrap();
        fs::write(root.join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("secret"), root.join("folder/link")).unwrap();
        let sources = collect_entries(&[root.join("folder")]).unwrap();
        assert_eq!(sources.len(), 1);
        assert!(sources[0].1.is_dir);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn paths_cannot_climb_out_of_the_batch() {
        for bad in ["../evil", "a/../../evil", "/etc/passwd", "a//b", "..", ".", "", "a/.", "...", "   "] {
            assert_eq!(clean_relative(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_hostile_entry_writes_nothing_outside() {
        let root = scratch("hostile");
        let batch = root.join("batch");
        let entries = vec![FileEntry { path: "../outside".to_string(), size: 1, is_dir: false }];
        let result = read_contents(&mut Cursor::new(vec![1u8]), &entries, &batch, || false);
        assert!(result.is_err());
        assert!(!root.join("outside").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn names_windows_refuses_are_made_safe() {
        assert_eq!(clean_component("a:b?.txt").as_deref(), Some("a_b_.txt"));
        assert_eq!(clean_component(r"C:\x").as_deref(), Some("C__x"));
        assert_eq!(clean_component("trailing. ").as_deref(), Some("trailing"));
        assert_eq!(clean_component("con.txt").as_deref(), Some("_con.txt"));
        assert_eq!(clean_component("COM1").as_deref(), Some("_COM1"));
        assert_eq!(clean_component("console.txt").as_deref(), Some("console.txt"));
        assert_eq!(clean_component("ordinary name.tar.gz").as_deref(), Some("ordinary name.tar.gz"));
    }

    #[test]
    fn a_copy_over_the_limit_is_refused_before_writing() {
        let root = scratch("limit");
        let batch = root.join("batch");
        let entries = vec![
            FileEntry { path: "a".to_string(), size: u64::MAX, is_dir: false },
            FileEntry { path: "b".to_string(), size: u64::MAX, is_dir: false },
        ];
        assert!(read_contents(&mut Cursor::new(Vec::new()), &entries, &batch, || false).is_err());
        assert!(!batch.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_cut_off_transfer_fails() {
        let root = scratch("cut");
        let entries = vec![FileEntry { path: "a".to_string(), size: 10, is_dir: false }];
        let result = read_contents(&mut Cursor::new(vec![1u8; 4]), &entries, &root.join("b"), || false);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_newer_copy_stops_a_transfer() {
        let root = scratch("newer");
        fs::write(root.join("f"), b"data").unwrap();
        let sources = collect_entries(&[root.join("f")]).unwrap();
        assert!(write_contents(&mut Vec::new(), &sources, || true).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_file_that_shrank_fails_rather_than_shifting_the_next() {
        let root = scratch("shrank");
        fs::write(root.join("f"), b"data").unwrap();
        let mut sources = collect_entries(&[root.join("f")]).unwrap();
        sources[0].1.size = 100;
        assert!(write_contents(&mut Vec::new(), &sources, || false).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn only_the_newest_batches_stay() {
        let root = scratch("prune");
        for name in ["001", "002", "003", "004"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        prune_inbox(&root, 2);
        assert!(!root.join("001").exists() && !root.join("002").exists());
        assert!(root.join("003").exists() && root.join("004").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn editing_a_file_changes_its_fingerprint() {
        let root = scratch("edit");
        let file = root.join("f");
        fs::write(&file, b"one").unwrap();
        let before = hash_files(std::slice::from_ref(&file));
        fs::write(&file, b"longer").unwrap();
        assert_ne!(before, hash_files(std::slice::from_ref(&file)));
        let _ = fs::remove_dir_all(root);
    }

    /// Uses the session's real clipboard, replacing what is on it.
    #[test]
    #[ignore]
    fn real_clipboard_holds_files_and_text_after_the_setter_is_gone() {
        let root = scratch("real");
        let file = root.join("real file.txt");
        fs::write(&file, b"x").unwrap();

        with_retries(|cb| cb.set().file_list(std::slice::from_ref(&file))).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let mut cb = Clipboard::new().unwrap();
        match read_snapshot(&mut cb) {
            Some(Snapshot::Files(paths)) => assert_eq!(paths, vec![file]),
            _ => panic!("expected the file back"),
        }

        with_retries(|cb| cb.set_text("mesh real text")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        match read_snapshot(&mut cb) {
            Some(Snapshot::Text(text)) => assert_eq!(text, "mesh real text"),
            _ => panic!("expected the text back"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_order_does_not_change_the_fingerprint() {
        let (a, b) = (PathBuf::from("/a"), PathBuf::from("/b"));
        assert_eq!(hash_files(&[a.clone(), b.clone()]), hash_files(&[b, a]));
    }
}
