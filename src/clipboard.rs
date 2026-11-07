use arboard::{Clipboard, ImageData};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Read, Write};
#[cfg(not(test))]
use crate::secure::control;
#[cfg(not(test))]
use crate::secure::stream::Purpose;
use crate::secure::stream::{SecureStream, Wire};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
/// How long one look at the clipboard gets. On Wayland it's a pipe from the app that made the copy,
/// read with no time limit of its own, and an app can simply not answer.
const READ_LIMIT: Duration = Duration::from_secs(2);
/// How often to look while reads go through the helper, as each one is a new process.
const HELPER_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// The longest gap between asking a helper that can't finish, in read limits.
const HELPER_GAP_LIMIT: u32 = 4;
const STUCK_NOTE: &str = "An app is taking too long to hand over its clipboard, so this copy hasn't been sent yet.";
const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Long enough for a lost connection request to be sent again on a poor link.
const CONNECT_WAIT: Duration = Duration::from_secs(2);
const ANSWER_WAIT: Duration = Duration::from_secs(5);
const RETRY_EVERY: Duration = Duration::from_secs(1);
/// Text that couldn't be sent for this long is left behind. It won't be what anyone is waiting for.
const RETRY_FOR: Duration = Duration::from_secs(10 * 60);

/// What the receiver answers with, in one byte.
const ANSWER_TAKEN: u8 = 1;
const ANSWER_NEWER_HERE: u8 = 2;
const ANSWER_NOT_RECEIVING: u8 = 3;
const ANSWER_FAILED: u8 = 4;
/// The slowest link worth allowing for, and the most that can be left in the network once the last
/// write returns, for working out how long an answer can take.
const SLOWEST_BYTES_PER_SECOND: u64 = 128 * 1024;
const MAX_IN_FLIGHT: u64 = 16 * 1024 * 1024;

/// The newest copy wins: an older file transfer stops when either count moves on.
static NEWEST_SEND: AtomicU64 = AtomicU64::new(0);
static NEWEST_RECEIVE: AtomicU64 = AtomicU64::new(0);
static BATCH_COUNT: AtomicU64 = AtomicU64::new(0);
/// The note, and whether it says something went wrong.
static NOTE: Mutex<(String, bool)> = Mutex::new((String::new(), false));
static OUTBOX: Mutex<Outbox> = Mutex::new(Outbox { waiting: None });
/// The clipboard sync is running, so what comes in over the control connection is taken.
static SYNC_RUNNING: AtomicBool = AtomicBool::new(false);

/// What the clipboard last did, for the app to show, and whether that was a failure.
pub fn clipboard_note() -> (String, bool) {
    NOTE.lock().map(|n| n.clone()).unwrap_or_default()
}

fn store_note(note: String, failed: bool) {
    println!("[Clipboard] {note}");
    if let Ok(mut current) = NOTE.lock() {
        *current = (note, failed);
    }
}

fn set_note(note: impl Into<String>) {
    store_note(note.into(), false);
}

fn set_failure(note: impl Into<String>) {
    store_note(note.into(), true);
}

/// Takes the note down if it still says a read is stuck, as reads are working again.
fn clear_stuck_note() {
    if let Ok(mut current) = NOTE.lock() {
        if current.0 == STUCK_NOTE {
            *current = (String::new(), false);
        }
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
    /// Comes first, and says how long ago the copy that follows was made. An age needs no clocks
    /// to agree. A device from before this can't read it, and closes the connection.
    Stamp { age_ms: u64 },
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
    /// When the newest copy known here was made, on either device.
    newest_copy: Option<Instant>,
    /// What's being written to the clipboard for the peer. The watcher may see it before the write
    /// has returned, and it must not go back out as a copy.
    incoming: Option<u64>,
}

impl Tracker {
    fn is_new(&mut self, fingerprint: u64, now: Instant) -> bool {
        let new = self.last.is_some_and(|last| last != fingerprint) && self.incoming != Some(fingerprint);
        self.last = Some(fingerprint);
        if new {
            self.newest_copy = Some(now);
        }
        new
    }

    fn expect(&mut self, fingerprint: u64) {
        self.incoming = Some(fingerprint);
    }

    /// The write worked, so this is what the clipboard holds.
    fn mark(&mut self, fingerprint: u64) {
        self.last = Some(fingerprint);
        self.incoming = None;
    }

    /// The write failed, so the clipboard holds what it did.
    fn abandon(&mut self) {
        self.incoming = None;
    }

    /// Takes a copy from the peer unless something newer has been copied since. Both devices can
    /// have text waiting through a dropout, and each must end up with the later of the two.
    fn take_from_peer(&mut self, copied_at: Instant) -> bool {
        let newer = self.newest_copy.is_none_or(|newest| copied_at >= newest);
        if newer {
            self.newest_copy = Some(copied_at);
        }
        newer
    }
}

/// Text that couldn't be sent yet. Only the newest copy ever waits here, and only while it is the
/// newest: anything copied or received since, of any kind, takes its turn away.
struct WaitingText {
    turn: u64,
    text: String,
    copied_at: Instant,
}

struct Outbox {
    waiting: Option<WaitingText>,
}

impl Outbox {
    fn hold(&mut self, turn: u64, text: String, copied_at: Instant) {
        if self.waiting.as_ref().is_none_or(|waiting| waiting.turn <= turn) {
            self.waiting = Some(WaitingText { turn, text, copied_at });
        }
    }

    fn clear(&mut self, turn: u64) {
        if self.waiting.as_ref().is_some_and(|waiting| waiting.turn <= turn) {
            self.waiting = None;
        }
    }

    /// What to try next. `gave_up` says it just ran out of time.
    fn due(&mut self, newest_turn: u64, sending: bool, now: Instant) -> Due {
        let Some(waiting) = &self.waiting else { return Due::Nothing };
        if waiting.turn != newest_turn || !sending {
            self.waiting = None;
            return Due::Nothing;
        }
        if now.duration_since(waiting.copied_at) >= RETRY_FOR {
            self.waiting = None;
            return Due::GaveUp;
        }
        Due::Send { turn: waiting.turn, text: waiting.text.clone(), copied_at: waiting.copied_at }
    }
}

#[derive(Debug, PartialEq)]
enum Due {
    Nothing,
    GaveUp,
    Send { turn: u64, text: String, copied_at: Instant },
}

/// How a sending ended.
#[derive(Debug, PartialEq)]
enum Sent {
    Taken,
    NewerThere,
    NotReceiving,
    /// It got there and couldn't be used.
    Failed,
    /// Over the limit, so it never left.
    TooLarge,
    /// It answered with something this version doesn't know, so it is newer.
    NoAnswer,
    /// It couldn't be reached, or stopped answering. The link may yet come back.
    Unreachable(String),
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

/// A snapshot on its way out of the helper.
#[cfg(target_os = "linux")]
#[derive(Serialize, Deserialize)]
enum WireSnapshot {
    /// As bytes, as a path on Linux needn't be text, and serialising one that isn't would fail.
    Files(Vec<Vec<u8>>),
    Text(String),
    Image { width: usize, height: usize, bytes: Vec<u8> },
}

#[cfg(target_os = "linux")]
impl From<Snapshot> for WireSnapshot {
    fn from(snapshot: Snapshot) -> Self {
        match snapshot {
            Snapshot::Files(paths) => {
                use std::os::unix::ffi::OsStringExt;
                Self::Files(paths.into_iter().map(|path| path.into_os_string().into_vec()).collect())
            }
            Snapshot::Text(text) => Self::Text(text),
            Snapshot::Image(image) => Self::Image { width: image.width, height: image.height, bytes: image.bytes.into_owned() },
        }
    }
}

#[cfg(target_os = "linux")]
impl From<WireSnapshot> for Snapshot {
    fn from(wire: WireSnapshot) -> Self {
        match wire {
            WireSnapshot::Files(paths) => {
                use std::os::unix::ffi::OsStringExt;
                Self::Files(paths.into_iter().map(|path| std::ffi::OsString::from_vec(path).into()).collect())
            }
            WireSnapshot::Text(text) => Self::Text(text),
            WireSnapshot::Image { width, height, bytes } => Self::Image(ImageData { width, height, bytes: Cow::Owned(bytes) }),
        }
    }
}

/// The helper's whole job: one look at the clipboard, written to stdout. It's a process so that it
/// can be killed if the read never ends, and it ends itself in case nothing is left to do that.
#[cfg(target_os = "linux")]
pub fn print_snapshot() {
    std::thread::spawn(|| {
        std::thread::sleep(READ_LIMIT * 2);
        std::process::exit(1);
    });
    let snapshot = Clipboard::new().ok().and_then(|mut cb| read_snapshot(&mut cb)).map(WireSnapshot::from);
    if let Ok(bytes) = bincode::serialize(&snapshot) {
        let _ = std::io::stdout().write_all(&bytes);
    }
}

/// One look through the helper. An `Err` means it didn't finish, and was killed.
#[cfg(target_os = "linux")]
fn read_by_helper(helper: &Path) -> Result<Option<Snapshot>, ()> {
    let mut command = std::process::Command::new(helper);
    command.arg("--read-clipboard");
    let output = crate::mute::platform::run_with_deadline(command, READ_LIMIT).ok_or(())?;
    let snapshot: Option<WireSnapshot> = bincode::deserialize(&output.stdout).unwrap_or(None);
    Ok(snapshot.map(Snapshot::from))
}

/// What came of a look at the clipboard.
#[derive(Debug, PartialEq)]
enum Look<T> {
    Got(Option<T>),
    /// It didn't finish in time.
    Stuck,
    /// A read from before is still going, and there's nothing to do but wait.
    Waiting,
}

struct Worker<T> {
    ask: std::sync::mpsc::Sender<()>,
    answers: std::sync::mpsc::Receiver<Option<T>>,
}

/// The opening of a new reader, which happens on the thread that will use it.
type OpenReader<T> = Arc<dyn Fn() -> Box<dyn FnMut() -> Option<T>> + Send + Sync>;

/// Reads off the watching thread, so that a read that never ends can't stop the watching.
///
/// A thread stuck reading a pipe can't be got back, and the next read of the same app sticks too. So
/// one stuck read is left behind, and from then until it ends, `helper` does the reading. That one
/// can be stopped at the limit, so the next copy from anywhere else is noticed. The left-behind read
/// may only be slow, as a big image from an app can be, so its answer is used when it comes, unless
/// the helper has found something newer. Where there's no helper, the read isn't on a pipe, it will
/// end, and that's all there is to do.
struct TimedReader<T> {
    open: OpenReader<T>,
    helper: Option<Box<dyn FnMut() -> Result<Option<T>, ()>>>,
    limit: Duration,
    worker: Option<Worker<T>>,
    abandoned: Option<Worker<T>>,
    /// The helper has found something since the read was left behind.
    helper_found: bool,
    /// A helper that can't finish isn't asked again for this long, and longer each time.
    helper_gap: Duration,
    next_helper: Option<Instant>,
}

impl<T: Send + 'static> TimedReader<T> {
    fn new(open: OpenReader<T>, helper: Option<Box<dyn FnMut() -> Result<Option<T>, ()>>>, limit: Duration) -> Self {
        Self { open, helper, limit, worker: None, abandoned: None, helper_found: false, helper_gap: limit, next_helper: None }
    }

    fn start_worker(&self) -> Worker<T> {
        let (ask, asked) = std::sync::mpsc::channel();
        let (answer, answers) = std::sync::mpsc::channel();
        let open = Arc::clone(&self.open);
        let _ = std::thread::Builder::new().name("mesh-kvm-clip-read".to_string()).spawn(move || {
            let mut read = open();
            while asked.recv().is_ok() {
                if answer.send(read()).is_err() {
                    break;
                }
            }
        });
        Worker { ask, answers }
    }

    fn forget_abandoned(&mut self) {
        self.abandoned = None;
        self.helper_found = false;
        self.helper_gap = self.limit;
        self.next_helper = None;
    }

    fn ask_helper(&mut self) -> Look<T> {
        let now = Instant::now();
        let Some(helper) = &mut self.helper else { return Look::Waiting };
        if self.next_helper.is_some_and(|at| now < at) {
            return Look::Waiting;
        }
        match helper() {
            Ok(found) => {
                self.helper_found |= found.is_some();
                self.helper_gap = self.limit;
                self.next_helper = None;
                Look::Got(found)
            }
            Err(()) => {
                self.next_helper = Some(now + self.helper_gap);
                self.helper_gap = (self.helper_gap * 2).min(self.limit * HELPER_GAP_LIMIT);
                Look::Stuck
            }
        }
    }

    /// True while a stuck read is holding things up.
    fn held_up(&self) -> bool {
        self.abandoned.is_some()
    }

    fn read(&mut self) -> Look<T> {
        use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
        if let Some(old) = &self.abandoned {
            match old.answers.try_recv() {
                Ok(late) => {
                    let superseded = self.helper_found;
                    self.forget_abandoned();
                    if !superseded {
                        return Look::Got(late);
                    }
                }
                Err(TryRecvError::Disconnected) => self.forget_abandoned(),
                Err(TryRecvError::Empty) => return self.ask_helper(),
            }
        }
        if self.worker.is_none() {
            self.worker = Some(self.start_worker());
        }
        let Some(worker) = &self.worker else { return Look::Got(None) };
        if worker.ask.send(()).is_err() {
            self.worker = None;
            return Look::Got(None);
        }
        match worker.answers.recv_timeout(self.limit) {
            Ok(answer) => Look::Got(answer),
            Err(RecvTimeoutError::Timeout) => {
                self.abandoned = self.worker.take();
                Look::Stuck
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.worker = None;
                Look::Got(None)
            }
        }
    }
}

/// A reader of the real clipboard, with its own handle, so that a fresh start is a fresh `Clipboard`.
fn snapshot_reader(helper_exe: PathBuf) -> TimedReader<Snapshot> {
    let open: OpenReader<Snapshot> = Arc::new(|| {
        let mut cb = None;
        Box::new(move || {
            if cb.is_none() {
                cb = Clipboard::new().ok();
            }
            read_snapshot(cb.as_mut()?)
        })
    });
    #[cfg(target_os = "linux")]
    let helper: Option<Box<dyn FnMut() -> Result<Option<Snapshot>, ()>>> = Some(Box::new(move || read_by_helper(&helper_exe)));
    #[cfg(not(target_os = "linux"))]
    let helper = {
        let _ = helper_exe;
        None
    };
    TimedReader::new(open, helper, READ_LIMIT)
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

/// The moment `age_ms` before `now`.
fn made_at(now: Instant, age_ms: u64) -> Instant {
    now.checked_sub(Duration::from_millis(age_ms)).unwrap_or(now)
}

fn age_ms(copied_at: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(copied_at).as_millis() as u64
}

/// Reads what starts a copy. One with no stamp comes from an older device, and counts as just made.
fn read_copy<R: Read>(reader: &mut R) -> Result<(u64, ClipboardPayload), String> {
    match read_frame(reader)? {
        ClipboardPayload::Stamp { age_ms } => Ok((age_ms, read_frame(reader)?)),
        payload => Ok((0, payload)),
    }
}

/// Gives the answer and closes. Closing with anything unread resets the connection, which can
/// destroy the answer on its way, so this side stops writing first and reads what's left for a moment.
fn finish<W: Wire>(mut stream: W, answer: u8) {
    let _ = stream.write_all(&[answer]);
    let _ = stream.end_writing();
    let _ = stream.set_read_wait(Some(Duration::from_millis(200)));
    let until = Instant::now() + Duration::from_secs(2);
    let mut drain = vec![0u8; CHUNK_BYTES];
    while Instant::now() < until && stream.read(&mut drain).is_ok_and(|got| got > 0) {}
}

fn receive<W: Wire>(mut stream: W, tracker: &Mutex<Tracker>, inbox: &Path) -> Result<(), String> {
    let _ = stream.set_read_wait(Some(IO_TIMEOUT));
    let _ = stream.set_write_wait(Some(IO_TIMEOUT));
    let mut first = [0u8; 1];
    match stream.read(&mut first) {
        // It ended without sending anything, so nothing failed. A connection that was cut is an error
        // from the read and not this.
        Ok(0) => return Ok(()),
        Ok(_) => {}
        Err(e) => {
            finish(stream, ANSWER_FAILED);
            return Err(e.to_string());
        }
    }
    let taken = take_copy(&mut first.as_slice().chain(&mut stream), tracker, inbox);
    finish(stream, *taken.as_ref().unwrap_or(&ANSWER_FAILED));
    taken.map(|_| ())
}

/// Puts `set` on the clipboard, with the tracker kept in step.
fn write_clipboard(tracker: &Mutex<Tracker>, fingerprint: u64, set: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let with_tracker = |act: &dyn Fn(&mut Tracker)| {
        if let Ok(mut guard) = tracker.lock() {
            act(&mut guard);
        }
    };
    with_tracker(&|t| t.expect(fingerprint));
    let written = set();
    match written {
        Ok(()) => with_tracker(&|t| t.mark(fingerprint)),
        Err(_) => with_tracker(&|t| t.abandon()),
    }
    written
}

/// Takes in one copy, and returns the answer to give.
fn take_copy<R: Read>(stream: &mut R, tracker: &Mutex<Tracker>, inbox: &Path) -> Result<u8, String> {
    let (age, payload) = read_copy(stream)?;
    let newest = tracker.lock().map(|mut t| t.take_from_peer(made_at(Instant::now(), age))).unwrap_or(true);
    if !newest {
        return Ok(ANSWER_NEWER_HERE);
    }
    // Whatever is on its way out from here is older than this now.
    NEWEST_SEND.fetch_add(1, Ordering::SeqCst);
    let turn = NEWEST_RECEIVE.fetch_add(1, Ordering::SeqCst) + 1;
    match payload {
        ClipboardPayload::Stamp { .. } => return Err("Got two stamps in a row".to_string()),
        ClipboardPayload::Text(text) => {
            write_clipboard(tracker, hash_bytes(text.as_bytes()), || with_retries(|cb| cb.set_text(text.as_str())))?;
            set_note("Received text");
        }
        ClipboardPayload::Image { width, height, bytes } => {
            write_clipboard(tracker, hash_bytes(&bytes), || {
                with_retries(|cb| cb.set_image(ImageData { width, height, bytes: Cow::Borrowed(&bytes) }))
            })?;
            set_note("Received an image");
        }
        ClipboardPayload::Files(entries) => {
            let count = file_count(&entries);
            set_note(format!("Receiving {count}\u{2026}"));
            let batch = new_batch_dir(inbox);
            let cancelled = || NEWEST_RECEIVE.load(Ordering::SeqCst) != turn;
            let top = match read_contents(stream, &entries, &batch, cancelled) {
                Ok(top) => top,
                Err(e) => {
                    let _ = fs::remove_dir_all(&batch);
                    return Err(format!("{count} didn't all arrive. Copy them again on the other device. {e}"));
                }
            };
            write_clipboard(tracker, hash_files(&top), || with_retries(|cb| cb.set().file_list(&top)))?;
            prune_inbox(inbox, KEEP_BATCHES);
            set_note(format!("Received {count}"));
        }
    }
    Ok(ANSWER_TAKEN)
}

fn tracker() -> &'static Mutex<Tracker> {
    static TRACKER: OnceLock<Mutex<Tracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(Tracker::default()))
}

/// A clipboard connection from the peer, handed on by the control listener. It runs on the
/// connection's own thread: text copied during a long transfer must not wait for it.
pub fn take_incoming(stream: SecureStream) {
    if !SYNC_RUNNING.load(Ordering::SeqCst) || !CLIPBOARD_RECEIVE.load(Ordering::SeqCst) {
        finish(stream, ANSWER_NOT_RECEIVING);
    } else if let Err(e) = receive(stream, tracker(), &inbox_dir()) {
        set_failure(format!("Couldn't receive the clipboard: {e}"));
    }
}

/// Starts the watching and sending. Receiving comes through the control listener, which
/// `take_incoming` answers for.
pub fn start_clipboard_sync(peer_ip: String, port: u16) -> Result<(), String> {
    let peer: IpAddr = peer_ip.parse().map_err(|e| format!("Bad peer address {peer_ip}: {e}"))?;
    // 1. Spawn the retrying of text that couldn't be sent. Its own thread, as waiting on a poor
    // link must not hold up noticing the next copy.
    let retry_addr = SocketAddr::new(peer, port);
    std::thread::Builder::new()
        .name("mesh-kvm-clip-retry".to_string())
        .spawn(move || loop {
            std::thread::sleep(RETRY_EVERY);
            let newest = NEWEST_SEND.load(Ordering::SeqCst);
            let sending = CLIPBOARD_SEND.load(Ordering::SeqCst);
            retry_waiting(retry_addr, &OUTBOX, newest, sending);
        })
        .map_err(|e| e.to_string())?;

    // 2. Spawn clipboard watcher and sender
    std::thread::Builder::new()
        .name("mesh-kvm-clip-tx".to_string())
        .spawn(move || {
            let target_addr = SocketAddr::new(peer, port);
            let inbox = inbox_dir();

            // The session may not be up yet when the service starts.
            // Held for good: on X11, what mesh-kvm put on the clipboard goes with the last handle.
            let _kept_open = loop {
                match Clipboard::new() {
                    Ok(cb) => break cb,
                    Err(e) => {
                        set_note(format!("Waiting for the clipboard: {e}"));
                        std::thread::sleep(CLIPBOARD_RETRY);
                    }
                }
            };

            // The running program, even if a newer one has replaced the file.
            let mut reader = snapshot_reader(PathBuf::from("/proc/self/exe"));
            let mut said_stuck = false;
            loop {
                std::thread::sleep(if reader.held_up() { HELPER_POLL_INTERVAL } else { POLL_INTERVAL });

                let look = reader.read();
                match look {
                    Look::Waiting => continue,
                    Look::Stuck => {
                        if !said_stuck && CLIPBOARD_SEND.load(Ordering::SeqCst) {
                            set_failure(STUCK_NOTE);
                        }
                        said_stuck = true;
                        continue;
                    }
                    Look::Got(_) if said_stuck => {
                        said_stuck = false;
                        clear_stuck_note();
                    }
                    Look::Got(_) => {}
                }
                let Look::Got(Some(snapshot)) = look else { continue };
                let fingerprint = snapshot.fingerprint();
                let now = Instant::now();
                let changed = tracker().lock().map(|mut t| t.is_new(fingerprint, now)).unwrap_or(false);

                // Tracked even with sending off, so that what was copied meanwhile does not go out
                // the moment sending comes back on.
                if changed && CLIPBOARD_SEND.load(Ordering::SeqCst) {
                    send_snapshot(target_addr, snapshot, &inbox, now);
                }
            }
        })
        .map_err(|e| e.to_string())?;

    SYNC_RUNNING.store(true, Ordering::SeqCst);
    Ok(())
}

/// Sends off the watching thread, so that a long transfer doesn't hide the next copy.
fn send_snapshot(target_addr: SocketAddr, snapshot: Snapshot, inbox: &Path, copied_at: Instant) {
    // Files that came from the peer stay put, or a copy would bounce between the devices.
    if let Snapshot::Files(paths) = &snapshot {
        if paths.iter().all(|p| p.starts_with(inbox)) {
            return;
        }
    }
    let turn = NEWEST_SEND.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = std::thread::Builder::new().name("mesh-kvm-clip-out".to_string()).spawn(move || {
        match snapshot {
            Snapshot::Text(text) => {
                let sent = send_text(target_addr, &text, copied_at);
                if NEWEST_SEND.load(Ordering::SeqCst) == turn {
                    settle_text(sent, turn, text, copied_at, &OUTBOX);
                }
            }
            snapshot => {
                let failure = send_whole(target_addr, snapshot, copied_at, turn);
                if let (Err(e), true) = (failure, NEWEST_SEND.load(Ordering::SeqCst) == turn) {
                    set_failure(e);
                }
            }
        }
    });
}

/// What to say and do about how sending text went. Only the newest text gets here.
fn settle_text(sent: Sent, turn: u64, text: String, copied_at: Instant, outbox: &Mutex<Outbox>) {
    let mut outbox = outbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Sent::Unreachable(reason) = sent {
        println!("[Clipboard] Couldn't send text: {reason}");
        outbox.hold(turn, text, copied_at);
        set_note("Text is waiting to go to the other device. It goes as soon as the link is back.");
        return;
    }
    outbox.clear(turn);
    match sent {
        Sent::Taken => set_note("Sent text"),
        Sent::NewerThere => set_note("The other device has something newer on its clipboard, so it kept that."),
        Sent::NotReceiving => set_note("The other device isn't taking the clipboard right now."),
        Sent::Failed => set_failure(COULD_NOT_USE),
        Sent::TooLarge => set_failure("That text is too big to send. The limit is 64 MB."),
        Sent::NoAnswer | Sent::Unreachable(_) => set_failure(TOO_OLD),
    }
}

const COULD_NOT_USE: &str = "The other device couldn't put that on its clipboard. Copy it again.";
const TOO_OLD: &str = "The other device didn't take the clipboard. Update both devices to the same version.";

/// One try at the text that's waiting, if there is any and it is still wanted.
fn retry_waiting(target_addr: SocketAddr, outbox: &Mutex<Outbox>, newest_turn: u64, sending: bool) {
    let due = outbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).due(newest_turn, sending, Instant::now());
    match due {
        Due::Nothing => {}
        Due::GaveUp => set_failure("Text waiting to go to the other device was dropped, as the link stayed down. Copy it again."),
        Due::Send { turn, text, copied_at } => {
            let sent = send_text(target_addr, &text, copied_at);
            // A copy made during the try has taken over, and saying more would only confuse.
            if NEWEST_SEND.load(Ordering::SeqCst) == turn {
                settle_text(sent, turn, text, copied_at, outbox);
            }
        }
    }
}

/// The connection to the peer. The handshake counts inside `CONNECT_WAIT`.
#[cfg(not(test))]
fn connect(target_addr: SocketAddr) -> Result<SecureStream, String> {
    let mut stream = control::open(target_addr, Purpose::Clipboard, CONNECT_WAIT)?;
    let _ = stream.set_write_wait(Some(IO_TIMEOUT));
    let _ = stream.set_read_wait(Some(ANSWER_WAIT));
    Ok(stream)
}

/// Tests that don't need the encryption run over plain TCP.
#[cfg(test)]
fn connect(target_addr: SocketAddr) -> Result<std::net::TcpStream, String> {
    let mut stream = std::net::TcpStream::connect_timeout(&target_addr, CONNECT_WAIT).map_err(|e| e.to_string())?;
    let _ = stream.set_write_wait(Some(IO_TIMEOUT));
    let _ = stream.set_read_wait(Some(ANSWER_WAIT));
    Ok(stream)
}

/// How long an answer can take once everything is written, as the last of it may still be in the
/// network on the slowest link.
fn answer_wait(bytes: u64) -> Duration {
    ANSWER_WAIT + Duration::from_secs(bytes.min(MAX_IN_FLIGHT) / SLOWEST_BYTES_PER_SECOND)
}

fn read_answer<R: Read>(stream: &mut R) -> Sent {
    let mut answer = [0u8; 1];
    match stream.read(&mut answer) {
        Ok(1) => match answer[0] {
            ANSWER_TAKEN => Sent::Taken,
            ANSWER_NEWER_HERE => Sent::NewerThere,
            ANSWER_NOT_RECEIVING => Sent::NotReceiving,
            ANSWER_FAILED => Sent::Failed,
            _ => Sent::NoAnswer,
        },
        // It was there and hung up on what it was sent, or restarted: the link may yet come back.
        Ok(_) => Sent::Unreachable("It closed part way through".to_string()),
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof) => {
            Sent::Unreachable("It closed part way through".to_string())
        }
        Err(e) => Sent::Unreachable(e.to_string()),
    }
}

/// A write that succeeded only means the text left this device, so the answer is what counts.
fn send_text(target_addr: SocketAddr, text: &str, copied_at: Instant) -> Sent {
    send_text_to(target_addr, text, copied_at)
}

fn send_text_to(target_addr: SocketAddr, text: &str, copied_at: Instant) -> Sent {
    if text.len() > MAX_FRAME_BYTES {
        return Sent::TooLarge;
    }
    let mut stream = match connect(target_addr) {
        Ok(stream) => stream,
        Err(e) => return Sent::Unreachable(e),
    };
    let age_ms = age_ms(copied_at, Instant::now());
    let written = write_frame(&mut stream, &ClipboardPayload::Stamp { age_ms })
        .and_then(|()| write_frame(&mut stream, &ClipboardPayload::Text(text.to_string())));
    let _ = stream.set_read_wait(Some(answer_wait(text.len() as u64)));
    match written {
        Ok(()) => read_answer(&mut stream),
        // It may have closed on seeing a stamp it can't read, and an answer tells the two apart.
        Err(e) => match read_answer(&mut stream) {
            Sent::Unreachable(_) => Sent::Unreachable(e),
            sent => sent,
        },
    }
}

fn unsent(what: &str, reason: &str) -> String {
    format!("Couldn't send {what}, so it stayed here. Copy it again once the link is back. {reason}")
}

/// Images and files aren't kept to retry: they can be large, and the file may be gone by then. The
/// note says so. A file transfer may take long enough for a newer copy to stop it.
fn send_whole(target_addr: SocketAddr, snapshot: Snapshot, copied_at: Instant, turn: u64) -> Result<(), String> {
    let stamp = ClipboardPayload::Stamp { age_ms: age_ms(copied_at, Instant::now()) };
    match snapshot {
        Snapshot::Text(_) => Ok(()),
        Snapshot::Image(image) => {
            let size = image.bytes.len() as u64;
            let payload = ClipboardPayload::Image {
                width: image.width,
                height: image.height,
                bytes: image.bytes.into_owned(),
            };
            let what = "the image";
            let mut stream = connect(target_addr).map_err(|e| unsent(what, &e))?;
            write_frame(&mut stream, &stamp).map_err(|e| unsent(what, &e))?;
            write_frame(&mut stream, &payload).map_err(|e| unsent(what, &e))?;
            let _ = stream.set_read_wait(Some(answer_wait(size)));
            answer_note(read_answer(&mut stream), what, "Sent an image")
        }
        Snapshot::Files(paths) => {
            let sources = collect_entries(&paths).map_err(|e| unsent("the files", &e))?;
            let entries: Vec<FileEntry> = sources.iter().map(|(_, e)| e.clone()).collect();
            let count = file_count(&entries);
            set_note(format!("Sending {count}\u{2026}"));
            let mut stream = connect(target_addr).map_err(|e| unsent(&count, &e))?;
            write_frame(&mut stream, &stamp).map_err(|e| unsent(&count, &e))?;
            write_frame(&mut stream, &ClipboardPayload::Files(entries)).map_err(|e| unsent(&count, &e))?;
            write_contents(&mut stream, &sources, || NEWEST_SEND.load(Ordering::SeqCst) != turn)
                .map_err(|e| unsent(&count, &e))?;
            let size = sources.iter().map(|(_, entry)| entry.size).sum();
            let _ = stream.set_read_wait(Some(answer_wait(size)));
            answer_note(read_answer(&mut stream), &count, &format!("Sent {count}"))
        }
    }
}

fn answer_note(sent: Sent, what: &str, done: &str) -> Result<(), String> {
    match sent {
        Sent::Taken => set_note(done),
        Sent::NewerThere => set_note("The other device has something newer on its clipboard, so it kept that."),
        Sent::NotReceiving => set_note("The other device isn't taking the clipboard right now."),
        Sent::Failed => return Err(COULD_NOT_USE.to_string()),
        Sent::NoAnswer | Sent::TooLarge => return Err(TOO_OLD.to_string()),
        Sent::Unreachable(reason) => return Err(unsent(what, &reason)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::stream::{Purpose, SecureStream};
    use std::io::Cursor;
    use std::net::TcpListener;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-clip-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn what_is_on_the_clipboard_at_startup_is_not_a_copy() {
        let mut tracker = Tracker::default();
        assert!(!tracker.is_new(1, Instant::now()));
        assert!(!tracker.is_new(1, Instant::now()));
        assert!(tracker.is_new(2, Instant::now()));
    }

    #[test]
    fn what_came_from_the_peer_is_not_sent_back() {
        let mut tracker = Tracker::default();
        tracker.mark(7);
        assert!(!tracker.is_new(7, Instant::now()));
    }

    #[test]
    fn only_the_newer_of_two_copies_is_taken_from_the_peer() {
        let start = Instant::now();
        let mut tracker = Tracker::default();
        tracker.is_new(1, start);
        tracker.is_new(2, start + Duration::from_secs(10));
        assert!(!tracker.take_from_peer(start + Duration::from_secs(5)), "copied here since");
        assert!(tracker.take_from_peer(start + Duration::from_secs(20)));
        assert!(!tracker.take_from_peer(start + Duration::from_secs(15)), "older than the last one taken");
    }

    #[test]
    fn with_nothing_copied_here_anything_from_the_peer_is_taken() {
        let mut tracker = Tracker::default();
        tracker.is_new(1, Instant::now());
        assert!(tracker.take_from_peer(Instant::now()), "what was there at startup isn't a copy");
    }

    #[test]
    fn two_devices_with_text_waiting_settle_on_the_later_copy() {
        let start = Instant::now();
        let (mut first, mut second) = (Tracker::default(), Tracker::default());
        first.is_new(0, start);
        second.is_new(0, start);
        first.is_new(1, start + Duration::from_secs(1));
        second.is_new(2, start + Duration::from_secs(2));
        // The link returns, and each sends what it has.
        assert!(!second.take_from_peer(start + Duration::from_secs(1)));
        assert!(first.take_from_peer(start + Duration::from_secs(2)));
    }

    #[test]
    fn a_copy_without_a_stamp_counts_as_just_made() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &ClipboardPayload::Text("old device".into())).unwrap();
        let (age, payload) = read_copy(&mut Cursor::new(wire)).unwrap();
        assert_eq!(age, 0);
        assert!(matches!(payload, ClipboardPayload::Text(text) if text == "old device"));
    }

    #[test]
    fn a_stamp_says_how_old_the_copy_behind_it_is() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &ClipboardPayload::Stamp { age_ms: 4000 }).unwrap();
        write_frame(&mut wire, &ClipboardPayload::Text("waited".into())).unwrap();
        let (age, payload) = read_copy(&mut Cursor::new(wire)).unwrap();
        assert_eq!(age, 4000);
        assert!(matches!(payload, ClipboardPayload::Text(text) if text == "waited"));
    }

    fn outbox_with(turn: u64, text: &str, copied_at: Instant) -> Mutex<Outbox> {
        let mut outbox = Outbox { waiting: None };
        outbox.hold(turn, text.to_string(), copied_at);
        Mutex::new(outbox)
    }

    #[test]
    fn only_the_newest_text_ever_waits() {
        let now = Instant::now();
        let mut outbox = Outbox { waiting: None };
        outbox.hold(3, "newer".into(), now);
        outbox.hold(2, "older, from a slow failure".into(), now);
        assert!(matches!(outbox.due(3, true, now), Due::Send { text, .. } if text == "newer"));
        outbox.hold(4, "newest".into(), now);
        assert!(matches!(outbox.due(4, true, now), Due::Send { text, .. } if text == "newest"));
    }

    #[test]
    fn anything_copied_since_takes_the_place_of_waiting_text() {
        let now = Instant::now();
        let mut outbox = Outbox { waiting: None };
        outbox.hold(3, "old".into(), now);
        // Turn 4 could be an image, or a copy from the other device.
        assert_eq!(outbox.due(4, true, now), Due::Nothing);
        assert_eq!(outbox.due(3, true, now), Due::Nothing, "and it doesn't come back");
    }

    #[test]
    fn turning_sending_off_drops_what_waits() {
        let now = Instant::now();
        let mut outbox = Outbox { waiting: None };
        outbox.hold(3, "secret".into(), now);
        assert_eq!(outbox.due(3, false, now), Due::Nothing);
        assert_eq!(outbox.due(3, true, now), Due::Nothing);
    }

    #[test]
    fn text_is_given_up_on_in_the_end() {
        let now = Instant::now();
        let mut outbox = Outbox { waiting: None };
        outbox.hold(3, "old".into(), now);
        assert_eq!(outbox.due(3, true, now + RETRY_FOR), Due::GaveUp);
        assert_eq!(outbox.due(3, true, now + RETRY_FOR), Due::Nothing);
    }

    /// A pretend device on `listener`: takes one copy and gives `answer`, if any.
    fn take_one(listener: TcpListener, answer: Option<u8>) -> std::thread::JoinHandle<(u64, String)> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (age, payload) = read_copy(&mut stream).unwrap();
            if let Some(answer) = answer {
                stream.write_all(&[answer]).unwrap();
            }
            let ClipboardPayload::Text(text) = payload else { panic!("expected text") };
            (age, text)
        })
    }

    #[test]
    fn text_goes_once_the_other_device_is_back_and_stops_being_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap();
        drop(listener);
        let copied_at = Instant::now() - Duration::from_secs(3);
        assert!(matches!(send_text(target, "hello", copied_at), Sent::Unreachable(_)));

        let outbox = outbox_with(7, "hello", copied_at);
        let listener = TcpListener::bind(target).unwrap();
        let other = take_one(listener, Some(ANSWER_TAKEN));
        // Stands in for the retry thread, without touching the service's own counters.
        let Due::Send { text, .. } = outbox.lock().unwrap().due(7, true, Instant::now()) else { panic!("nothing due") };
        assert_eq!(send_text(target, &text, copied_at), Sent::Taken);
        outbox.lock().unwrap().clear(7);
        let (age, text) = other.join().unwrap();
        assert_eq!(text, "hello");
        assert!(age >= 3000, "{age}");
        assert_eq!(outbox.lock().unwrap().due(7, true, Instant::now()), Due::Nothing);
    }

    /// A device that reads one frame and hangs up with the text unread. Closing like that resets the
    /// connection rather than ending it.
    fn hang_up_after_one_frame(listener: TcpListener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).unwrap();
            let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
            stream.read_exact(&mut frame).unwrap();
            // Long enough for the text to be sitting here unread when the socket goes.
            std::thread::sleep(Duration::from_millis(50));
        })
    }

    #[test]
    fn a_hang_up_with_no_answer_is_a_dead_link_to_try_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap();
        let other = hang_up_after_one_frame(listener);
        let sent = send_text_to(target, "hello", Instant::now());
        other.join().unwrap();
        assert!(matches!(sent, Sent::Unreachable(_)), "{sent:?}");
        let outbox = Mutex::new(Outbox { waiting: None });
        settle_text(sent, 9, "hello".into(), Instant::now(), &outbox);
        assert!(matches!(outbox.lock().unwrap().due(9, true, Instant::now()), Due::Send { .. }), "so it's tried again");
    }

    #[test]
    fn an_answer_this_version_does_not_know_asks_for_an_update() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap();
        let other = take_one(listener, Some(99));
        assert_eq!(send_text(target, "hello", Instant::now()), Sent::NoAnswer);
        other.join().unwrap();
    }

    /// Connected secure streams: the sender's, then the receiver's.
    fn secure_pair() -> (SecureStream, SecureStream) {
        use crate::secure::keys::{Pairing, PeerKey};
        let mut a = Pairing::fresh();
        let mut b = Pairing::fresh();
        a.peer = Some(PeerKey { public: b.identity.public, name: "b".into(), paired_at: 0 });
        b.peer = Some(PeerKey { public: a.identity.public, name: "a".into(), paired_at: 0 });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            SecureStream::accept(tcp, &b, Duration::from_secs(5)).unwrap().0
        });
        let client = SecureStream::connect(addr, &a, Purpose::Clipboard, Duration::from_secs(5)).unwrap();
        (client, server.join().unwrap())
    }

    #[test]
    fn text_goes_through_a_secure_stream() {
        let (mut sender, mut receiver) = secure_pair();
        let other = std::thread::spawn(move || {
            let (age, payload) = read_copy(&mut receiver).unwrap();
            finish(receiver, ANSWER_TAKEN);
            (age, payload)
        });
        write_frame(&mut sender, &ClipboardPayload::Stamp { age_ms: 1234 }).unwrap();
        write_frame(&mut sender, &ClipboardPayload::Text("hello".into())).unwrap();
        assert_eq!(read_answer(&mut sender), Sent::Taken);
        let (age, payload) = other.join().unwrap();
        assert_eq!(age, 1234);
        assert!(matches!(payload, ClipboardPayload::Text(t) if t == "hello"));
    }

    #[test]
    fn an_answer_survives_a_secure_receiver_that_stops_reading() {
        let (mut sender, receiver) = secure_pair();
        let other = std::thread::spawn(move || finish(receiver, ANSWER_NEWER_HERE));
        // Plenty goes unread on the far side.
        write_frame(&mut sender, &ClipboardPayload::Text("x".repeat(3 * 1024 * 1024))).ok();
        assert_eq!(read_answer(&mut sender), Sent::NewerThere);
        other.join().unwrap();
    }

    #[test]
    fn a_secure_stream_that_hangs_up_unanswered_reads_as_a_dead_link() {
        let (mut sender, receiver) = secure_pair();
        drop(receiver);
        assert!(matches!(read_answer(&mut sender), Sent::Unreachable(_)));
        let (mut sender, mut receiver) = secure_pair();
        receiver.end_writing().unwrap();
        assert!(matches!(read_answer(&mut sender), Sent::Unreachable(_)), "an end with no answer is no answer");
    }

    /// Sends a file's frames, and then `part` of its `size` bytes.
    fn send_part_of_a_file(sender: &mut SecureStream, size: u64, part: usize) {
        write_frame(sender, &ClipboardPayload::Stamp { age_ms: 0 }).unwrap();
        let entries = vec![FileEntry { path: "big.bin".to_string(), size, is_dir: false }];
        write_frame(sender, &ClipboardPayload::Files(entries)).unwrap();
        sender.write_all(&vec![5u8; part]).unwrap();
    }

    fn batches_in(inbox: &Path) -> usize {
        fs::read_dir(inbox).map(|dir| dir.flatten().count()).unwrap_or(0)
    }

    #[test]
    fn a_transfer_cut_off_is_not_taken() {
        let inbox = scratch("cut-secure");
        let tracker = Mutex::new(Tracker::default());
        // Cut: the connection goes with the file half sent and no end frame.
        let (mut sender, receiver) = secure_pair();
        send_part_of_a_file(&mut sender, 1_000_000, 300_000);
        drop(sender);
        assert!(receive(receiver, &tracker, &inbox).is_err());
        assert_eq!(batches_in(&inbox), 0, "no half batch is left to pass as a whole one");

        // Ended early on purpose: still short, so it fails, and the sender is told so.
        let (mut sender, receiver) = secure_pair();
        send_part_of_a_file(&mut sender, 1_000_000, 300_000);
        sender.end_writing().unwrap();
        assert!(receive(receiver, &tracker, &inbox).is_err());
        assert_eq!(read_answer(&mut sender), Sent::Failed);
        assert_eq!(batches_in(&inbox), 0);
        let _ = fs::remove_dir_all(inbox);
    }

    #[test]
    fn a_secure_connection_that_ends_without_a_word_is_not_a_failure() {
        let (mut sender, receiver) = secure_pair();
        sender.end_writing().unwrap();
        let tracker = Mutex::new(Tracker::default());
        assert_eq!(receive(receiver, &tracker, Path::new("unused")), Ok(()));
    }

    #[test]
    fn a_secure_connection_cut_without_a_word_is_a_failure() {
        let (sender, receiver) = secure_pair();
        drop(sender);
        let tracker = Mutex::new(Tracker::default());
        assert!(receive(receiver, &tracker, Path::new("unused")).is_err());
    }

    #[test]
    fn a_clipboard_that_is_not_running_answers_not_receiving() {
        let (mut sender, receiver) = secure_pair();
        let other = std::thread::spawn(move || take_incoming(receiver));
        assert_eq!(read_answer(&mut sender), Sent::NotReceiving);
        other.join().unwrap();
    }

    #[test]
    fn an_answer_survives_a_receiver_that_stops_reading() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap();
        let other = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).unwrap();
            finish(stream, ANSWER_NEWER_HERE);
        });
        // A big frame, so that plenty goes unread.
        let text = "x".repeat(4 * 1024 * 1024);
        assert_eq!(send_text(target, &text, Instant::now()), Sent::NewerThere);
        other.join().unwrap();
    }

    #[test]
    fn the_wait_for_an_answer_grows_with_what_may_still_be_on_its_way() {
        assert_eq!(answer_wait(0), ANSWER_WAIT);
        assert!(answer_wait(4 * 1024 * 1024) >= ANSWER_WAIT + Duration::from_secs(30));
        assert_eq!(answer_wait(u64::MAX), answer_wait(MAX_IN_FLIGHT));
    }

    #[test]
    fn text_on_its_way_in_is_not_taken_for_a_copy_made_here() {
        let mut tracker = Tracker::default();
        tracker.is_new(1, Instant::now());
        tracker.expect(2);
        assert!(!tracker.is_new(2, Instant::now()), "the watcher can look before the write is done");
        tracker.mark(2);
        assert!(!tracker.is_new(2, Instant::now()));
    }

    #[test]
    fn a_write_that_failed_leaves_the_tracker_as_it_was() {
        let mut tracker = Tracker::default();
        tracker.is_new(1, Instant::now());
        tracker.expect(2);
        tracker.abandon();
        assert!(!tracker.is_new(1, Instant::now()), "the clipboard still holds what it did");
        assert!(tracker.is_new(2, Instant::now()), "so the same text copied here later counts");
    }

    #[test]
    fn a_failure_on_the_other_device_is_said_and_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap();
        let other = take_one(listener, Some(ANSWER_FAILED));
        let sent = send_text(target, "hello", Instant::now());
        assert_eq!(sent, Sent::Failed);
        other.join().unwrap();
        let outbox = outbox_with(9, "hello", Instant::now());
        settle_text(sent, 9, "hello".into(), Instant::now(), &outbox);
        assert_eq!(outbox.lock().unwrap().due(9, true, Instant::now()), Due::Nothing);
    }

    #[test]
    fn text_too_big_to_send_fails_at_once_rather_than_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap();
        drop(listener);
        let text = "x".repeat(MAX_FRAME_BYTES + 1);
        let sent = send_text(target, &text, Instant::now());
        assert_eq!(sent, Sent::TooLarge);
        let outbox = Mutex::new(Outbox { waiting: None });
        settle_text(sent, 9, text, Instant::now(), &outbox);
        assert_eq!(outbox.lock().unwrap().due(9, true, Instant::now()), Due::Nothing);
    }

    #[test]
    fn the_receivers_answer_comes_back() {
        for (answer, sent) in [(ANSWER_NEWER_HERE, Sent::NewerThere), (ANSWER_NOT_RECEIVING, Sent::NotReceiving)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let target = listener.local_addr().unwrap();
            let other = take_one(listener, Some(answer));
            assert_eq!(send_text(target, "hello", Instant::now()), sent);
            other.join().unwrap();
        }
    }

    #[test]
    fn text_that_cannot_go_waits_and_says_so_while_files_fail_with_a_note() {
        let outbox = Mutex::new(Outbox { waiting: None });
        settle_text(Sent::Unreachable("no route".into()), 9, "hello".into(), Instant::now(), &outbox);
        assert!(matches!(outbox.lock().unwrap().due(9, true, Instant::now()), Due::Send { .. }));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap();
        drop(listener);
        let root = scratch("unsent");
        fs::write(root.join("f"), b"data").unwrap();
        let snapshot = Snapshot::Files(vec![root.join("f")]);
        let note = send_whole(target, snapshot, Instant::now(), 0).unwrap_err();
        assert!(note.starts_with("Couldn't send 1 file, so it stayed here. Copy it again once the link is back."), "{note}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_reply_of_no_answer_is_an_update_note_not_a_retry() {
        let outbox = outbox_with(9, "hello", Instant::now());
        settle_text(Sent::NoAnswer, 9, "hello".into(), Instant::now(), &outbox);
        assert_eq!(outbox.lock().unwrap().due(9, true, Instant::now()), Due::Nothing);
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

#[cfg(test)]
mod reads {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc::{channel, Sender};

    const LIMIT: Duration = Duration::from_millis(30);

    /// Reads that answer at once, unless `hang` is on, when they wait to be let go.
    struct Fake {
        opened: Arc<AtomicU32>,
        hang: Arc<AtomicBool>,
        release: Arc<Mutex<Vec<Sender<()>>>>,
        value: Arc<Mutex<Option<u32>>>,
    }

    impl Fake {
        fn new() -> Self {
            Self { opened: Arc::default(), hang: Arc::default(), release: Arc::default(), value: Arc::new(Mutex::new(Some(1))) }
        }

        fn open(&self) -> OpenReader<u32> {
            let (opened, hang, release, value) =
                (self.opened.clone(), self.hang.clone(), self.release.clone(), self.value.clone());
            Arc::new(move || {
                opened.fetch_add(1, Ordering::SeqCst);
                let (hang, release, value) = (hang.clone(), release.clone(), value.clone());
                Box::new(move || {
                    if hang.load(Ordering::SeqCst) {
                        let (let_go, wait) = channel();
                        release.lock().unwrap().push(let_go);
                        let _ = wait.recv();
                    }
                    *value.lock().unwrap()
                })
            })
        }

        fn let_go(&self) {
            self.hang.store(false, Ordering::SeqCst);
            self.release.lock().unwrap().clear();
            std::thread::sleep(Duration::from_millis(20));
        }

        fn stuck_reads(&self) -> usize {
            self.release.lock().unwrap().len()
        }
    }

    #[test]
    fn a_normal_read_stays_on_one_clipboard() {
        let fake = Fake::new();
        let mut reader = TimedReader::new(fake.open(), None, LIMIT);
        for _ in 0..5 {
            assert_eq!(reader.read(), Look::Got(Some(1)));
        }
        assert_eq!(fake.opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_read_that_never_ends_doesn_t_stop_the_watching() {
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let mut reader = TimedReader::new(fake.open(), Some(Box::new(|| Ok(Some(2)))), LIMIT);
        let started = Instant::now();
        assert_eq!(reader.read(), Look::Stuck);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(reader.held_up());
        assert_eq!(reader.read(), Look::Got(Some(2)), "the next copy is noticed, through the helper");
        fake.let_go();
    }

    #[test]
    fn stuck_reads_can_t_pile_up_however_long_the_app_stays_hung() {
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let mut reader = TimedReader::new(fake.open(), Some(Box::new(|| Err(()))), LIMIT);
        for _ in 0..30 {
            assert_ne!(reader.read(), Look::Got(None));
        }
        assert_eq!(fake.stuck_reads(), 1);
        assert_eq!(fake.opened.load(Ordering::SeqCst), 1);
        fake.let_go();
    }

    #[test]
    fn a_helper_that_can_t_finish_is_asked_less_and_less() {
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let asked = Arc::new(AtomicU32::new(0));
        let count = Arc::clone(&asked);
        let helper = Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Err(())
        });
        let mut reader = TimedReader::new(fake.open(), Some(helper), LIMIT);
        assert_eq!(reader.read(), Look::Stuck);
        assert_eq!(reader.read(), Look::Stuck, "the first go at the helper");
        assert_eq!(reader.read(), Look::Waiting, "and it gets a rest");
        let started = Instant::now();
        while started.elapsed() < LIMIT * 8 {
            reader.read();
            std::thread::sleep(Duration::from_millis(2));
        }
        // Every 30, 60, then 120 ms, not every 2 ms.
        let times = asked.load(Ordering::SeqCst);
        assert!((2..=7).contains(&times), "{times}");
        fake.let_go();
    }

    #[test]
    fn a_helper_that_answers_is_not_held_back() {
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let mut reader = TimedReader::new(fake.open(), Some(Box::new(|| Ok(None))), LIMIT);
        assert_eq!(reader.read(), Look::Stuck);
        for _ in 0..5 {
            assert_eq!(reader.read(), Look::Got(None));
        }
        fake.let_go();
    }

    #[test]
    fn a_slow_read_is_used_when_it_finishes_if_nothing_newer_came() {
        // A big image from an app: over the limit, the helper can't finish it either.
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let mut reader = TimedReader::new(fake.open(), Some(Box::new(|| Err(()))), LIMIT);
        assert_eq!(reader.read(), Look::Stuck);
        *fake.value.lock().unwrap() = Some(9);
        fake.let_go();
        assert_eq!(reader.read(), Look::Got(Some(9)));
        assert!(!reader.held_up());
    }

    #[test]
    fn a_slow_read_is_used_after_a_helper_that_found_nothing() {
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let mut reader = TimedReader::new(fake.open(), Some(Box::new(|| Ok(None))), LIMIT);
        assert_eq!(reader.read(), Look::Stuck);
        assert_eq!(reader.read(), Look::Got(None));
        *fake.value.lock().unwrap() = Some(9);
        fake.let_go();
        assert_eq!(reader.read(), Look::Got(Some(9)));
    }

    #[test]
    fn once_the_helper_has_found_something_the_stuck_read_s_answer_is_old_news() {
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let mut reader = TimedReader::new(fake.open(), Some(Box::new(|| Ok(Some(2)))), LIMIT);
        assert_eq!(reader.read(), Look::Stuck);
        assert_eq!(reader.read(), Look::Got(Some(2)));
        *fake.value.lock().unwrap() = Some(9);
        fake.let_go();
        *fake.value.lock().unwrap() = Some(3);
        assert_eq!(reader.read(), Look::Got(Some(3)), "a fresh read, not the stale answer");
        assert!(!reader.held_up());
        assert_eq!(fake.opened.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_stuck_note_goes_on_recovery_and_no_other_note_does() {
        // One test, as they share the note.
        store_note(STUCK_NOTE.to_string(), true);
        clear_stuck_note();
        assert_eq!(clipboard_note(), (String::new(), false));
        store_note("Sent 4 files.".to_string(), false);
        clear_stuck_note();
        assert_eq!(clipboard_note().0, "Sent 4 files.");
        store_note(String::new(), false);
    }

    #[test]
    fn with_no_helper_a_slow_read_is_waited_for_and_its_answer_used() {
        // Windows: reads aren't on a pipe, so they end, and nothing else has read meanwhile.
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let mut reader = TimedReader::new(fake.open(), None, LIMIT);
        assert_eq!(reader.read(), Look::Stuck);
        assert_eq!(reader.read(), Look::Waiting);
        assert_eq!(reader.read(), Look::Waiting);
        assert_eq!(fake.stuck_reads(), 1);
        fake.let_go();
        assert_eq!(reader.read(), Look::Got(Some(1)));
        assert!(!reader.held_up());
    }

    #[test]
    fn what_the_other_device_sends_doesn_t_wait_for_a_stuck_read() {
        // A read holds nothing the receiving side needs: the tracker is only taken after it.
        let fake = Fake::new();
        fake.hang.store(true, Ordering::SeqCst);
        let mut reader = TimedReader::new(fake.open(), None, LIMIT);
        assert_eq!(reader.read(), Look::Stuck);
        let tracker = Mutex::new(Tracker::default());
        let written = write_clipboard(&tracker, 7, || Ok(()));
        assert!(written.is_ok());
        fake.let_go();
    }

    #[test]
    fn a_connection_closed_without_a_word_is_not_a_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let knock = std::thread::spawn(move || drop(TcpStream::connect(address).unwrap()));
        let (stream, _) = listener.accept().unwrap();
        knock.join().unwrap();
        let tracker = Mutex::new(Tracker::default());
        assert_eq!(receive(stream, &tracker, Path::new("unused")), Ok(()));
    }

    #[test]
    fn a_connection_that_stops_partway_still_is() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let knock = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(&[1, 2]).unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        knock.join().unwrap();
        let tracker = Mutex::new(Tracker::default());
        assert!(receive(stream, &tracker, Path::new("unused")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn everything_comes_through_the_helper_as_it_went_in() {
        let round_trip = |snapshot: Snapshot| {
            let wire = WireSnapshot::from(snapshot);
            let bytes = bincode::serialize(&Some(wire)).unwrap();
            let back: Option<WireSnapshot> = bincode::deserialize(&bytes).unwrap();
            Snapshot::from(back.unwrap())
        };
        assert!(matches!(round_trip(Snapshot::Text("hello".into())), Snapshot::Text(t) if t == "hello"));
        let paths = vec![PathBuf::from("/a/b")];
        assert!(matches!(round_trip(Snapshot::Files(paths.clone())), Snapshot::Files(p) if p == paths));
        let image = ImageData { width: 1, height: 1, bytes: Cow::Owned(vec![1, 2, 3, 4]) };
        assert!(matches!(round_trip(Snapshot::Image(image)), Snapshot::Image(i) if i.width == 1 && i.bytes.len() == 4));
        use std::os::unix::ffi::OsStringExt;
        let not_text = vec![PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff, 0xfe]))];
        assert!(matches!(round_trip(Snapshot::Files(not_text.clone())), Snapshot::Files(p) if p == not_text));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod live {
    use super::*;

    /// Watches the real clipboard, as the service does, printing what it notices. For a script
    /// beside an isolated compositor, as this would otherwise watch the real one.
    #[test]
    #[ignore = "run by hand beside an isolated compositor"]
    fn watch_real_clipboard() {
        let seconds = std::env::var("MK_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
        let end = Instant::now() + Duration::from_secs(seconds);
        let mut reader = snapshot_reader(std::env::var("MK_HELPER").expect("MK_HELPER").into());
        let mut last = 0;
        let mut wrote = false;
        let started = Instant::now();
        while Instant::now() < end {
            std::thread::sleep(if reader.held_up() { HELPER_POLL_INTERVAL } else { POLL_INTERVAL });
            // As a copy from the other device would, with the hung app still holding its pipe.
            if !wrote && std::env::var("MK_WRITE").is_ok() && started.elapsed() > Duration::from_secs(8) {
                wrote = true;
                let at = Instant::now();
                let written = with_retries(|cb| cb.set_text("from the other device"));
                println!("WROTE {written:?} in {} ms", at.elapsed().as_millis());
            }
            match reader.read() {
                Look::Got(Some(snapshot)) if snapshot.fingerprint() != last => {
                    last = snapshot.fingerprint();
                    if let Snapshot::Text(text) = &snapshot {
                        println!("SEEN {text}");
                    }
                }
                Look::Stuck => println!("STUCK"),
                _ => {}
            }
        }
        println!("WATCH ENDED");
    }
}
