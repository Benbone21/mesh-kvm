//! The link log: what the link between the two devices did, one line per event, so that a drop can
//! be looked into afterwards from both ends. Lines go to `link.log` in the config folder, which
//! moves to `link.log.1` once it passes 2 MB, and the last ones stay in memory for the API, which
//! still has them when the file can't be written. A new service starts with the end of the file
//! in memory, so a restart loses nothing from the API.
//!
//! Nothing here may hold up an input loop. A loop looks at the link a few times a second, which
//! costs a comparison or two, and hands any event it finds to a bounded queue; a thread of its own
//! writes them. A full queue drops the event. Two more threads look at this device's clock every
//! second and at its network now and then, the network with a limit on each program it runs.

use crate::input::link::{LinkQuality, PeerLink, RoundTrips};
use crate::platform::limited::{run_limited, OnExpiry, Ran};
use crate::router::{Quiet, Router};
use std::collections::{BTreeSet, VecDeque};
use std::fmt::{Display, Write as _};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const FILE_NAME: &str = "link.log";
/// What the API serves, and how many lines it gives when not told.
pub const PATH: &str = "/api/local/linklog";
pub const DEFAULT_LINES: usize = 500;
/// This device's wall clock, for the other device to line the two logs up by.
pub const CLOCK_PATH: &str = "/api/local/clock";
/// Lines kept in memory, and the most the API gives.
pub const KEPT_LINES: usize = 2000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const QUEUE: usize = 4096;
const SAMPLE_EVERY_MS: u64 = 10_000;
/// The network is looked at this often while things are changing, and less often once they settle.
const NET_QUICK: Duration = Duration::from_secs(5);
const NET_SLOW: Duration = Duration::from_secs(30);
const QUICK_FOR: Duration = Duration::from_secs(60);
const NET_WAKE: Duration = Duration::from_millis(250);
/// The most any program run for a look at the network may take.
const COMMAND_LIMIT: Duration = Duration::from_secs(3);
const PEER_CLOCK_EVERY: Duration = Duration::from_secs(60);
/// An answer slower than this says too little about the other device's clock.
const PEER_CLOCK_MAX_RTT_MS: i64 = 2000;
const CLOCK_EVERY: Duration = Duration::from_secs(1);
/// Lost time past this between two looks at the clock means the device slept, or this didn't run.
const ASLEEP_MS: u64 = 5000;
/// An input loop this late is a `stall`.
pub const STALL_MS: u64 = 300;
/// Wi-Fi signal is logged when it moves this far, in percent, from what was last logged.
const SIGNAL_STEP: u32 = 10;
/// How long stopping waits for the lines before it to be written.
const FLUSH_WAIT: Duration = Duration::from_millis(500);
const REOPEN_EVERY: Duration = Duration::from_secs(10);
const NO_SIGNAL: u32 = u32::MAX;
const NO_CLOCK: i64 = i64::MIN;

enum Msg {
    Line(Entry),
    Flush(SyncSender<()>),
}

static QUEUE_TX: OnceLock<SyncSender<Msg>> = OnceLock::new();
static PEER: OnceLock<String> = OnceLock::new();
static RING: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static PACKETS_IN: AtomicU64 = AtomicU64::new(0);
static PACKETS_OUT: AtomicU64 = AtomicU64::new(0);
/// The latest Wi-Fi signal, for each `down`.
static SIGNAL: AtomicU32 = AtomicU32::new(NO_SIGNAL);
/// How far the other device's clock is ahead of this one's, and how long the answer took.
static PEER_CLOCK_MS: AtomicI64 = AtomicI64::new(NO_CLOCK);
static PEER_CLOCK_RTT_MS: AtomicU32 = AtomicU32::new(0);
/// A `down` or a sleep asks for a look at the network straight away.
static LOOK_NOW: AtomicBool = AtomicBool::new(false);

/// Starts the log for the service, with a `start` line. Until this runs, which it never does in
/// tests, everything recorded is dropped.
pub fn start(peer: &str) {
    let _ = PEER.set(peer.to_string());
    let (tx, rx) = sync_channel(QUEUE);
    if QUEUE_TX.set(tx).is_err() {
        return;
    }
    let path = crate::settings::config_dir().join(FILE_NAME);
    *RING.lock().unwrap_or_else(PoisonError::into_inner) = seeded(read_tail(&older_path(&path)), read_tail(&path));
    let file = LogFile::new(path, MAX_FILE_BYTES);
    let _ = std::thread::Builder::new().name("mesh-kvm-linklog".into()).spawn(move || write_lines(rx, file));
    let _ = std::thread::Builder::new().name("mesh-kvm-linklog-clock".into()).spawn(watch_clock);
    let _ = std::thread::Builder::new().name("mesh-kvm-linklog-net".into()).spawn(watch_network);
    let details = Details::new()
        .text("version", &crate::update::trust::running_version().to_string())
        .text("os", std::env::consts::OS)
        .text("name", &crate::settings::device_name())
        .text("peer", if peer.is_empty() { "-" } else { peer })
        .num("pid", std::process::id());
    record("start", details);
}

/// Set once a `restarting` is logged, so that the old service's end says it once.
static RESTARTING: AtomicBool = AtomicBool::new(false);

/// A `restarting` line, waited for, the first time this is called. The new service's `start`
/// follows it; if starting again fails, a `stop` with the reason does instead.
pub fn restarting(reason: &str) {
    if !RESTARTING.swap(true, Ordering::Relaxed) {
        record_and_wait("restarting", Details::new().text("reason", reason));
    }
}

/// Starting again failed, and this process carries on or stops with its own line.
pub fn restart_failed(error: &str) {
    RESTARTING.store(false, Ordering::Relaxed);
    record("restart_failed", Details::new().text("error", error));
}

/// A `stop` line, waited for briefly so that it's written before the process goes. Once a
/// restart is under way, only a failure is worth one.
pub fn stop(reason: &str, error: Option<&str>) {
    if RESTARTING.load(Ordering::Relaxed) && error.is_none() {
        return;
    }
    let mut details = Details::new().text("reason", reason);
    if let Some(error) = error {
        details = details.text("error", error);
    }
    record_and_wait("stop", details);
}

/// Records one event, and waits briefly for it to be written, as the process may be about to go.
pub fn record_and_wait(word: &'static str, details: Details) {
    let Some(tx) = QUEUE_TX.get() else { return };
    record(word, details);
    let (done, wait) = sync_channel(1);
    if tx.try_send(Msg::Flush(done)).is_ok() {
        let _ = wait.recv_timeout(FLUSH_WAIT);
    }
}

/// Puts one event in the queue. It never blocks: with the queue full, the event is lost.
pub fn record(word: &'static str, details: Details) {
    if let Some(tx) = QUEUE_TX.get() {
        let _ = tx.try_send(Msg::Line(Entry { at_ms: wall_ms(), word, details: details.0 }));
    }
}

/// Counts packets for the samples. Safe from a hook.
pub fn packet_in() {
    PACKETS_IN.fetch_add(1, Ordering::Relaxed);
}
pub fn packet_out() {
    PACKETS_OUT.fetch_add(1, Ordering::Relaxed);
}

/// What a failed send of the link's was, by kind, for the samples and each `down`.
const SEND_KINDS: [&str; 7] = ["would_block", "no_buffer", "unreachable", "refused", "denied", "too_big", "other"];
const NO_KIND: usize = usize::MAX;
static SEND_ERRORS: [AtomicU64; SEND_KINDS.len()] = [const { AtomicU64::new(0) }; SEND_KINDS.len()];
/// The counts as of the last sample, which reports what came since.
static SEND_ERRORS_SAMPLED: [AtomicU64; SEND_KINDS.len()] = [const { AtomicU64::new(0) }; SEND_KINDS.len()];
static SEND_ERROR_LAST: AtomicUsize = AtomicUsize::new(NO_KIND);

/// Counts a send on the link that failed. Never logs on its own, so it's safe for every packet and
/// from a hook.
pub fn send_error(error: &std::io::Error) {
    let kind = send_kind(error);
    SEND_ERRORS[kind].fetch_add(1, Ordering::Relaxed);
    SEND_ERROR_LAST.store(kind, Ordering::Relaxed);
}

fn send_kind(error: &std::io::Error) -> usize {
    use std::io::ErrorKind;
    // ENOBUFS, EMSGSIZE, ENETUNREACH and EHOSTUNREACH, as Linux and Winsock number them.
    #[cfg(windows)]
    const RAW: [i32; 4] = [10055, 10040, 10051, 10065];
    #[cfg(not(windows))]
    const RAW: [i32; 4] = [105, 90, 101, 113];
    let raw = error.raw_os_error();
    let kind = match (raw, error.kind()) {
        (Some(n), _) if n == RAW[0] => "no_buffer",
        (Some(n), _) if n == RAW[1] => "too_big",
        (Some(n), _) if n == RAW[2] || n == RAW[3] => "unreachable",
        (_, ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable) => "unreachable",
        (_, ErrorKind::WouldBlock) => "would_block",
        (_, ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset) => "refused",
        (_, ErrorKind::PermissionDenied) => "denied",
        _ => "other",
    };
    SEND_KINDS.iter().position(|k| *k == kind).unwrap_or(SEND_KINDS.len() - 1)
}

fn send_error_counts(from: &[AtomicU64; SEND_KINDS.len()]) -> [u64; SEND_KINDS.len()] {
    std::array::from_fn(|i| from[i].load(Ordering::Relaxed))
}

/// `send_errors=` with how many sends failed since `was`, and when any did, the latest kind and
/// how many of each: `send_error_last=no_buffer send_errors_by=no_buffer:3,unreachable:1`.
fn send_errors(details: Details, now: [u64; SEND_KINDS.len()], was: [u64; SEND_KINDS.len()], last: usize) -> Details {
    let since: Vec<u64> = now.iter().zip(was).map(|(n, w)| n.saturating_sub(w)).collect();
    let total: u64 = since.iter().sum();
    let details = details.num("send_errors", total);
    if total == 0 {
        return details;
    }
    let by: Vec<String> = since.iter().enumerate().filter(|(_, n)| **n > 0).map(|(i, n)| format!("{}:{n}", SEND_KINDS[i])).collect();
    details.num("send_error_last", SEND_KINDS.get(last).copied().unwrap_or("-")).num("send_errors_by", by.join(","))
}

/// The audio tallies as of the last sample, which reports what came since: sound, then mic.
static AUDIO_SAMPLED: [[AtomicU64; crate::audio::TALLY_FIELDS]; 2] = [const { [const { AtomicU64::new(0) }; crate::audio::TALLY_FIELDS] }; 2];

/// One received stream's figures since the last sample, each name after `prefix`, and the fullest
/// its buffer got.
fn audio_since(details: Details, prefix: &str, now: [u64; crate::audio::TALLY_FIELDS], was: [u64; crate::audio::TALLY_FIELDS], buffer_max_ms: u32) -> Details {
    let mut details = details;
    for ((name, n), w) in crate::audio::TALLY_NAMES.iter().zip(now).zip(was) {
        // A stream that started again counts from nothing, so a drop in a total is all new.
        details = details.num(&format!("{prefix}{name}"), if n >= w { n - w } else { n });
    }
    details.num(&format!("{prefix}buffer_max_ms"), buffer_max_ms)
}

fn packets() -> (u64, u64) {
    (PACKETS_IN.load(Ordering::Relaxed), PACKETS_OUT.load(Ordering::Relaxed))
}

/// The API's answer for `path`, if it is the link log's: the last lines as plain text, or this
/// device's clock.
pub fn answer(path: &str) -> Option<String> {
    if path == CLOCK_PATH {
        return Some(serde_json::json!({ "unix_ms": wall_ms() }).to_string());
    }
    lines_asked(path).map(|lines| tail_of(&RING.lock().unwrap_or_else(PoisonError::into_inner), lines))
}

pub fn is_log_path(path: &str) -> bool {
    lines_asked(path).is_some()
}

/// How many lines `/api/local/linklog?lines=N` asks for, or `None` for another path.
fn lines_asked(path: &str) -> Option<usize> {
    let rest = path.strip_prefix(PATH)?;
    if rest.is_empty() {
        return Some(DEFAULT_LINES);
    }
    let query = rest.strip_prefix('?')?;
    let asked = query.split('&').find_map(|pair| pair.strip_prefix("lines=")).and_then(|n| n.parse::<usize>().ok());
    Some(asked.unwrap_or(DEFAULT_LINES).min(KEPT_LINES))
}

fn tail_of(ring: &VecDeque<String>, lines: usize) -> String {
    let mut text = String::new();
    for line in ring.iter().skip(ring.len().saturating_sub(lines)) {
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// The last lines of the older file and then the newer one.
fn seeded(older: Option<String>, newer: Option<String>) -> VecDeque<String> {
    let mut ring = VecDeque::new();
    for text in [older, newer].into_iter().flatten() {
        for line in text.lines().filter(|line| !line.is_empty()) {
            keep(&mut ring, line.to_string());
        }
    }
    ring
}

/// The end of this device's log files, for `--link-log` when the service isn't answering.
pub fn file_tail(lines: usize) -> Option<String> {
    let path = crate::settings::config_dir().join(FILE_NAME);
    let older = read_tail(&older_path(&path));
    let newer = read_tail(&path);
    if older.is_none() && newer.is_none() {
        return None;
    }
    Some(tail_of(&seeded(older, newer), lines))
}

/// The most read back from each file: well over `KEPT_LINES` lines.
const TAIL_BYTES: u64 = 512 * 1024;

/// The end of a log file. A line cut through at the start is left out, and a byte that isn't
/// UTF-8 costs only itself.
fn read_tail(path: &std::path::Path) -> Option<String> {
    read_tail_within(path, TAIL_BYTES)
}

fn read_tail_within(path: &std::path::Path, most: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(most);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    file.take(most).read_to_end(&mut bytes).ok()?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if from > 0 {
        text = text.split_once('\n').map_or_else(String::new, |(_, rest)| rest.to_string());
    }
    Some(text)
}

fn older_path(path: &std::path::Path) -> PathBuf {
    path.with_file_name(format!("{FILE_NAME}.1"))
}

fn keep(ring: &mut VecDeque<String>, line: String) {
    if ring.len() >= KEPT_LINES {
        ring.pop_front();
    }
    ring.push_back(line);
}

fn wall_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

// ---- Lines ----

/// The `key=value` part of a line.
#[derive(Default)]
pub struct Details(String);

impl Details {
    pub fn new() -> Details {
        Details::default()
    }

    pub fn num(mut self, key: &str, value: impl Display) -> Details {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        let _ = write!(self.0, "{key}={value}");
        self
    }

    /// Text in quotes when it has a space or a quote in it, or nothing at all. Lines are read by
    /// splitting each pair at its first `=`.
    pub fn text(self, key: &str, value: &str) -> Details {
        let plain = !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c == '"');
        if plain {
            return self.num(key, value);
        }
        let mut quoted = String::from("\"");
        for c in value.chars() {
            match c {
                '"' | '\\' => {
                    quoted.push('\\');
                    quoted.push(c);
                }
                c if c.is_control() => quoted.push(' '),
                c => quoted.push(c),
            }
        }
        quoted.push('"');
        self.num(key, quoted)
    }

    /// A figure that may not be known, shown as `-`.
    fn opt(self, key: &str, value: Option<impl Display>) -> Details {
        match value {
            Some(value) => self.num(key, value),
            None => self.num(key, "-"),
        }
    }
}

struct Entry {
    at_ms: i64,
    word: &'static str,
    details: String,
}

impl Entry {
    fn line(&self, offset_s: i32) -> String {
        let space = if self.details.is_empty() { "" } else { " " };
        format!("{} {}{space}{}", time_text(self.at_ms, offset_s), self.word, self.details)
    }
}

/// Such as `2026-11-08T18:13:25.412-07:00`.
fn time_text(unix_ms: i64, offset_s: i32) -> String {
    let local = unix_ms + i64::from(offset_s) * 1000;
    let (year, month, day) = civil_from_days(local.div_euclid(86_400_000));
    let in_day = local.rem_euclid(86_400_000);
    let sign = if offset_s < 0 { '-' } else { '+' };
    let offset = offset_s.unsigned_abs();
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}{sign}{:02}:{:02}",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000,
        offset / 3600,
        offset / 60 % 60
    )
}

/// The date of a day counted from 1970-01-01, proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// The other way: the days from 1970-01-01 to a date.
#[cfg_attr(not(windows), allow(dead_code))]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// How far this device's clock is from UTC at `unix_ms`, in seconds.
#[cfg(unix)]
fn utc_offset_s(unix_ms: i64) -> i32 {
    let now = unix_ms.div_euclid(1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i32
}

/// Windows says local time and UTC apart, so the offset is their difference, to the quarter hour.
#[cfg(windows)]
fn utc_offset_s(_unix_ms: i64) -> i32 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    let (mut local, mut utc): (SYSTEMTIME, SYSTEMTIME) = unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    unsafe {
        GetLocalTime(&mut local);
        GetSystemTime(&mut utc);
    }
    let seconds = |t: &SYSTEMTIME| {
        days_from_civil(t.wYear.into(), t.wMonth.into(), t.wDay.into()) * 86_400
            + i64::from(t.wHour) * 3600
            + i64::from(t.wMinute) * 60
            + i64::from(t.wSecond)
    };
    let difference = seconds(&local) - seconds(&utc);
    ((difference as f64 / 900.0).round() * 900.0) as i32
}

// ---- The file ----

/// `link.log`, moved aside once it passes its limit. One that can't be opened is tried again now
/// and then, and the lines meanwhile are only in memory. One that can't be moved aside is emptied,
/// so the file never grows past its limit.
struct LogFile {
    path: PathBuf,
    limit: u64,
    file: Option<File>,
    size: u64,
    retry_at: Option<Instant>,
}

impl LogFile {
    fn new(path: PathBuf, limit: u64) -> LogFile {
        LogFile { path, limit, file: None, size: 0, retry_at: None }
    }

    fn write(&mut self, line: &str) {
        if self.file.is_none() && self.retry_at.is_none_or(|at| Instant::now() >= at) {
            self.open(false);
        }
        let bytes = format!("{line}\n");
        if self.file.is_some() && self.size > 0 && self.size + bytes.len() as u64 > self.limit {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else { return };
        if file.write_all(bytes.as_bytes()).is_ok() {
            self.size += bytes.len() as u64;
        } else {
            self.file = None;
            self.retry_at = Some(Instant::now() + REOPEN_EVERY);
        }
    }

    fn open(&mut self, empty: bool) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut options = std::fs::OpenOptions::new();
        if empty {
            options.write(true).truncate(true).create(true);
        } else {
            options.append(true).create(true);
        }
        match options.open(&self.path) {
            Ok(file) => {
                self.size = file.metadata().map_or(0, |m| m.len());
                self.file = Some(file);
                self.retry_at = None;
            }
            Err(_) => {
                self.file = None;
                self.retry_at = Some(Instant::now() + REOPEN_EVERY);
            }
        }
    }

    /// Closed first, as Windows can't move an open file.
    fn rotate(&mut self) {
        self.file = None;
        let old = older_path(&self.path);
        let _ = std::fs::remove_file(&old);
        let moved = std::fs::rename(&self.path, &old).is_ok();
        self.open(!moved);
    }
}

fn write_lines(rx: Receiver<Msg>, mut file: LogFile) {
    for msg in rx {
        match msg {
            Msg::Line(entry) => {
                let line = entry.line(utc_offset_s(entry.at_ms));
                file.write(&line);
                keep(&mut RING.lock().unwrap_or_else(PoisonError::into_inner), line);
            }
            Msg::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

// ---- The input loops ----

/// How late an input loop comes round. Each loop keeps one and ticks it every time round; a tick
/// reads a clock and makes a subtraction and a comparison.
pub struct LoopClock {
    name: &'static str,
    every_ms: u64,
    last: Option<u64>,
    worst_ms: u64,
}

impl LoopClock {
    /// For a loop meant to come round every `every_ms`.
    pub fn new(name: &'static str, every_ms: u64) -> LoopClock {
        LoopClock { name, every_ms, last: None, worst_ms: 0 }
    }

    /// Records a `stall` when this time round came more than `STALL_MS` late. It goes by a clock
    /// that stops while the device sleeps, so the time round after a sleep is no stall.
    pub fn tick(&mut self) {
        if let Some(late_ms) = self.lateness(awake_ms()) {
            record("stall", Details::new().num("late_ms", late_ms).num("loop", self.name));
        }
    }

    /// The lateness of a stall, if this was one.
    fn lateness(&mut self, now_ms: u64) -> Option<u64> {
        let late = self.last.map_or(0, |last| now_ms.saturating_sub(last).saturating_sub(self.every_ms));
        self.last = Some(now_ms);
        self.worst_ms = self.worst_ms.max(late);
        (late > STALL_MS).then_some(late)
    }

    /// The worst lateness since the last call.
    pub fn take_worst(&mut self) -> u64 {
        std::mem::take(&mut self.worst_ms)
    }
}

/// The figures that go with `down`, `gap` and `one_way`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Figures {
    pub round_trip_ms: Option<u32>,
    pub loss_in: Option<f32>,
    pub loss_out: Option<f32>,
    pub one_way: bool,
}

impl From<&LinkQuality> for Figures {
    fn from(q: &LinkQuality) -> Figures {
        Figures { round_trip_ms: q.round_trip_ms, loss_in: q.loss_in, loss_out: q.loss_out, one_way: q.one_way }
    }
}

/// One look at the link from an input loop.
#[derive(Debug, Clone, Copy, Default)]
pub struct Look {
    pub now_ms: u64,
    pub online: bool,
    pub online_since: u64,
    pub last_alive: u64,
    pub quiet: Option<Quiet>,
    pub figures: Figures,
    pub round_trips: Option<RoundTrips>,
    pub packets: (u64, u64),
    /// The worst lateness of the input loop since the last look.
    pub loop_late_ms: u64,
}

impl Look {
    /// What the router, the link and the loop say now. This takes their quiet spell, round trips
    /// and lateness.
    pub fn of(router: &mut Router, link: &mut PeerLink, clock: &mut LoopClock, now_ms: u64) -> Look {
        Look {
            now_ms,
            online: router.online(),
            online_since: router.online_since(),
            last_alive: router.last_alive(),
            quiet: router.take_quiet(),
            figures: (&link.quality(router, now_ms)).into(),
            round_trips: link.take_round_trips(),
            packets: packets(),
            loop_late_ms: clock.take_worst(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinkEvent {
    /// `silent_ms` is how long nothing came before this, unknown for the first. Its figures are
    /// still to come, so it has none.
    Up { down_ms: u64, silent_ms: Option<u64> },
    /// With the figures from just before.
    Down { silent_ms: u64, up_ms: u64, figures: Figures },
    /// A silence that ended with the link still up: over `QUIET_MS`, or over `BUSY_QUIET_MS` when
    /// it broke a stream. `in_per_s` is the rate since the last sample.
    Gap { silent_ms: u64, busy: bool, figures: Figures, in_per_s: f32 },
    /// The other device stopped hearing this one with the link up, or started again.
    OneWay { on: bool, figures: Figures },
    Sample { online: bool, round_trips: Option<RoundTrips>, figures: Figures, in_per_s: f32, out_per_s: f32, loop_late_ms: u64 },
}

/// Turns looks at the link into events. The input loops keep one each and look every tick of their
/// telemetry; nearly every look finds nothing.
pub struct LinkWatch {
    online: bool,
    online_since: u64,
    /// When the link last went up or down, or the watch started.
    changed_at: u64,
    /// The figures from the last look with the link up: they go when it does.
    figures: Figures,
    one_way: bool,
    round_trips: Option<RoundTrips>,
    loop_late_ms: u64,
    sampled_at: u64,
    sampled_packets: (u64, u64),
}

impl LinkWatch {
    pub fn new(now_ms: u64) -> LinkWatch {
        LinkWatch::starting(now_ms, packets())
    }

    fn starting(now_ms: u64, packets: (u64, u64)) -> LinkWatch {
        LinkWatch {
            online: false,
            online_since: 0,
            changed_at: now_ms,
            figures: Figures::default(),
            one_way: false,
            round_trips: None,
            loop_late_ms: 0,
            sampled_at: now_ms,
            sampled_packets: packets,
        }
    }

    pub fn look(&mut self, look: Look) -> Vec<LinkEvent> {
        let mut events = Vec::new();
        if let Some(t) = look.round_trips {
            self.round_trips = Some(match self.round_trips {
                Some(was) => RoundTrips {
                    min_ms: was.min_ms.min(t.min_ms),
                    max_ms: was.max_ms.max(t.max_ms),
                    sum_ms: was.sum_ms + t.sum_ms,
                    count: was.count + t.count,
                },
                None => t,
            });
        }
        self.loop_late_ms = self.loop_late_ms.max(look.loop_late_ms);
        let quiet_ms = look.quiet.map(|q| q.ms);
        match (self.online, look.online) {
            (false, true) => {
                let down_ms = look.online_since.saturating_sub(self.changed_at);
                events.push(LinkEvent::Up { down_ms, silent_ms: quiet_ms });
                self.changed_at = look.online_since;
                self.one_way = false;
            }
            (true, false) => {
                let silent_ms = look.now_ms.saturating_sub(look.last_alive);
                let up_ms = look.now_ms.saturating_sub(self.online_since);
                events.push(LinkEvent::Down { silent_ms, up_ms, figures: self.figures });
                self.changed_at = look.now_ms;
            }
            // The link went down and up again between two looks: the packet that ended the silence
            // found it past the timeout.
            (true, true) if look.online_since != self.online_since => {
                let silent_ms = quiet_ms.unwrap_or(0);
                let up_ms = look.online_since.saturating_sub(self.online_since + silent_ms);
                events.push(LinkEvent::Down { silent_ms, up_ms, figures: self.figures });
                events.push(LinkEvent::Up { down_ms: 0, silent_ms: quiet_ms });
                self.changed_at = look.online_since;
                self.one_way = false;
            }
            (true, true) => {
                if let Some(quiet) = look.quiet {
                    let in_per_s = self.rate(look.now_ms, look.packets.0, self.sampled_packets.0);
                    events.push(LinkEvent::Gap { silent_ms: quiet.ms, busy: quiet.busy, figures: look.figures, in_per_s });
                }
            }
            (false, false) => {}
        }
        if look.online && look.figures.one_way != self.one_way {
            self.one_way = look.figures.one_way;
            events.push(LinkEvent::OneWay { on: self.one_way, figures: look.figures });
        }
        self.online = look.online;
        self.online_since = look.online_since;
        if look.online {
            self.figures = look.figures;
        }
        if look.now_ms.saturating_sub(self.sampled_at) >= SAMPLE_EVERY_MS {
            events.push(LinkEvent::Sample {
                online: look.online,
                round_trips: self.round_trips.take(),
                figures: look.figures,
                in_per_s: self.rate(look.now_ms, look.packets.0, self.sampled_packets.0),
                out_per_s: self.rate(look.now_ms, look.packets.1, self.sampled_packets.1),
                loop_late_ms: std::mem::take(&mut self.loop_late_ms),
            });
            self.sampled_at = look.now_ms;
            self.sampled_packets = look.packets;
        }
        events
    }

    /// Packets per second since the last sample.
    fn rate(&self, now_ms: u64, now: u64, was: u64) -> f32 {
        let seconds = now_ms.saturating_sub(self.sampled_at).max(1) as f32 / 1000.0;
        now.saturating_sub(was) as f32 / seconds
    }
}

/// Records what a `LinkWatch` found, with what else goes with it: the latest Wi-Fi signal on a
/// `down`, and audio, the control connection and the other device's clock on a sample. Nothing
/// here waits: atomics, and the control connection's note, whose lock is never held over a call.
pub fn note(events: Vec<LinkEvent>) {
    for event in events {
        let (word, mut details) = describe(&event, PEER.get().map_or("-", String::as_str));
        match event {
            LinkEvent::Down { .. } => {
                let signal = SIGNAL.load(Ordering::Relaxed);
                if signal != NO_SIGNAL {
                    details = details.num("signal", signal);
                }
                let last = SEND_ERROR_LAST.load(Ordering::Relaxed);
                details = send_errors(details, send_error_counts(&SEND_ERRORS), send_error_counts(&SEND_ERRORS_SAMPLED), last);
                LOOK_NOW.store(true, Ordering::Relaxed);
            }
            LinkEvent::Sample { .. } => {
                use crate::audio::{AUDIO_RX_BUFFER_MS, AUDIO_RX_LIVE, AUDIO_RX_LOSS, AUDIO_RX_MIC_LIVE, AUDIO_RX_MIC_TALLY, AUDIO_RX_TALLY};
                let streams = [
                    (&AUDIO_RX_TALLY, AUDIO_RX_LIVE.load(Ordering::Relaxed), "audio_"),
                    (&AUDIO_RX_MIC_TALLY, AUDIO_RX_MIC_LIVE.load(Ordering::Relaxed), "mic_"),
                ];
                if streams[0].1 {
                    let loss = f64::from(AUDIO_RX_LOSS.load(Ordering::Relaxed)) / 10.0;
                    details = details.num("audio_loss", format!("{loss:.1}")).num("audio_buffer_ms", AUDIO_RX_BUFFER_MS.load(Ordering::Relaxed));
                }
                for ((tally, live, prefix), sampled) in streams.into_iter().zip(&AUDIO_SAMPLED) {
                    let now = tally.totals();
                    let was: [u64; crate::audio::TALLY_FIELDS] = std::array::from_fn(|i| sampled[i].swap(now[i], Ordering::Relaxed));
                    let buffer_max_ms = tally.take_buffer_max_ms();
                    if live {
                        details = audio_since(details, prefix, now, was, buffer_max_ms);
                    }
                }
                let now = send_error_counts(&SEND_ERRORS);
                details = send_errors(details, now, send_error_counts(&SEND_ERRORS_SAMPLED), SEND_ERROR_LAST.load(Ordering::Relaxed));
                for (sampled, n) in SEND_ERRORS_SAMPLED.iter().zip(now) {
                    sampled.store(n, Ordering::Relaxed);
                }
                details = details.num("control", crate::secure::control::control_state());
                details = peer_clock(details, PEER_CLOCK_MS.load(Ordering::Relaxed), PEER_CLOCK_RTT_MS.load(Ordering::Relaxed));
            }
            _ => {}
        }
        record(word, details);
    }
}

/// `peer_clock_ms` is how far the other device's clock is ahead of this one's, give or take half
/// of `peer_clock_rtt_ms`: add it to a time here to find the same moment in the other log.
fn peer_clock(details: Details, ahead_ms: i64, rtt_ms: u32) -> Details {
    if ahead_ms == NO_CLOCK {
        return details.num("peer_clock_ms", "-");
    }
    details.num("peer_clock_ms", format!("{ahead_ms:+}")).num("peer_clock_rtt_ms", rtt_ms)
}

fn loss(details: Details, f: &Figures) -> Details {
    details.opt("loss_in", f.loss_in.map(|l| format!("{l:.2}"))).opt("loss_out", f.loss_out.map(|l| format!("{l:.2}")))
}

fn figures(details: Details, f: &Figures) -> Details {
    loss(details.opt("rtt_ms", f.round_trip_ms), f).num("one_way", f.one_way)
}

fn describe(event: &LinkEvent, peer: &str) -> (&'static str, Details) {
    match event {
        LinkEvent::Up { down_ms, silent_ms } => {
            ("up", Details::new().num("down_ms", down_ms).opt("silent_ms", *silent_ms).text("peer", peer))
        }
        LinkEvent::Down { silent_ms, up_ms, figures: f } => {
            let details = Details::new().num("silent_ms", silent_ms);
            ("down", figures(details, f).text("peer", peer).num("up_ms", up_ms))
        }
        LinkEvent::Gap { silent_ms, busy, figures: f, in_per_s } => {
            let details = Details::new().num("silent_ms", silent_ms).num("while_busy", busy);
            ("gap", loss(details, f).num("in_pps", format!("{in_per_s:.1}")))
        }
        LinkEvent::OneWay { on, figures: f } => {
            let details = Details::new().num("state", if *on { "on" } else { "off" }).opt("rtt_ms", f.round_trip_ms);
            ("one_way", loss(details, f))
        }
        LinkEvent::Sample { online, round_trips, figures: f, in_per_s, out_per_s, loop_late_ms } => {
            let ms = |pick: fn(&RoundTrips) -> f32| round_trips.as_ref().map(|t| format!("{:.0}", pick(t)));
            let details = Details::new()
                .num("link", if *online { "up" } else { "down" })
                .opt("rtt_min_ms", ms(|t| t.min_ms))
                .opt("rtt_avg_ms", ms(RoundTrips::average_ms))
                .opt("rtt_max_ms", ms(|t| t.max_ms))
                .num("answers", round_trips.map_or(0, |t| t.count));
            let details = loss(details, f)
                .num("one_way", f.one_way)
                .num("in_pps", format!("{in_per_s:.1}"))
                .num("out_pps", format!("{out_per_s:.1}"))
                .num("loop_late_ms", loop_late_ms);
            ("sample", details)
        }
    }
}

// ---- The device: its clock ----

/// Milliseconds on a clock that stops while the device sleeps. On Linux that's the monotonic
/// clock, as `Instant` is. On Windows `Instant` goes on through a sleep, and the unbiased interrupt
/// time doesn't.
pub fn awake_ms() -> u64 {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn QueryUnbiasedInterruptTime(unbiased: *mut u64) -> i32;
        }
        let mut hundred_ns = 0u64;
        if unsafe { QueryUnbiasedInterruptTime(&mut hundred_ns) } != 0 {
            return hundred_ns / 10_000;
        }
    }
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Time lost between two looks at the clock, `slept_ms` apart, as the wall clock and `awake_ms`
/// saw it. The second stops while the device sleeps, so the wall clock gets ahead of it. A look
/// that simply comes late is the other case: this thread didn't get to run. A wall clock that's
/// set forward looks like the first.
fn asleep(wall_ms: i64, mono_ms: u64, slept_ms: u64) -> Option<(u64, &'static str)> {
    let ahead = wall_ms.saturating_sub(mono_ms as i64);
    let late = mono_ms.saturating_sub(slept_ms);
    if ahead > ASLEEP_MS as i64 && ahead as u64 >= late {
        Some((ahead as u64, "clock"))
    } else if late > ASLEEP_MS {
        Some((late, "late"))
    } else {
        None
    }
}

/// A thread of its own, so that nothing the network look waits on can keep this from seeing a sleep.
fn watch_clock() {
    loop {
        let (wall, awake) = (wall_ms(), awake_ms());
        std::thread::sleep(CLOCK_EVERY);
        let lost = asleep(wall_ms() - wall, awake_ms().saturating_sub(awake), CLOCK_EVERY.as_millis() as u64);
        if let Some((ms, how)) = lost {
            record("asleep", Details::new().num("ms", ms).num("how", how));
            LOOK_NOW.store(true, Ordering::Relaxed);
        }
    }
}

// ---- The device: its network ----

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wifi {
    ssid: String,
    bssid: String,
    /// In percent.
    signal: Option<u32>,
    channel: String,
    freq: String,
}

impl Wifi {
    fn add_to(&self, details: Details) -> Details {
        let mut details = details.text("ssid", &self.ssid).text("bssid", &self.bssid).opt("signal", self.signal);
        if !self.channel.is_empty() {
            details = details.text("chan", &self.channel);
        }
        if !self.freq.is_empty() {
            details = details.text("freq", &self.freq);
        }
        details
    }
}

/// What a look at the Wi-Fi found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WifiRead {
    Connected(Wifi),
    NotConnected,
    /// Connecting or authenticating: a roam in the middle, say. Nothing is said until it settles.
    Changing,
    /// It couldn't be read, and why. That's no reason to think it went.
    Unknown(String),
}

/// Compares each look at the network with the one before and says what changed.
#[derive(Default)]
struct NetWatch {
    started: bool,
    addrs: Option<BTreeSet<String>>,
    /// The Wi-Fi as last read, `None` until it has been.
    wifi: Option<Option<Wifi>>,
    /// Why the Wi-Fi couldn't be read, when it couldn't, as last logged.
    wifi_error: Option<String>,
    signal_logged: Option<u32>,
}

fn joined(addrs: &BTreeSet<String>) -> String {
    if addrs.is_empty() {
        "-".to_string()
    } else {
        addrs.iter().cloned().collect::<Vec<_>>().join(",")
    }
}

fn wifi_details(details: Details, wifi: &Option<Wifi>) -> Details {
    match wifi {
        Some(w) => w.add_to(details),
        None => details.num("wifi", "none"),
    }
}

impl NetWatch {
    /// `addrs` is `None` when they couldn't be read this time, which is no change.
    fn look(&mut self, addrs: Option<BTreeSet<String>>, wifi: WifiRead) -> Vec<(&'static str, Details)> {
        let mut lines = Vec::new();
        if !self.started {
            self.started = true;
            let details = Details::new().text("addrs", &addrs.as_ref().map_or("-".to_string(), joined));
            let details = match &wifi {
                WifiRead::Connected(w) => w.add_to(details),
                WifiRead::NotConnected => details.num("wifi", "none"),
                WifiRead::Changing => details.num("wifi", "changing"),
                WifiRead::Unknown(error) => details.num("wifi", "unknown").text("error", error),
            };
            lines.push(("net", details));
            self.addrs = addrs;
            match wifi {
                WifiRead::Connected(w) => self.known(Some(w)),
                WifiRead::NotConnected => self.known(None),
                WifiRead::Changing => {}
                WifiRead::Unknown(error) => self.wifi_error = Some(error),
            }
            return lines;
        }
        if let (Some(now), Some(was)) = (&addrs, &self.addrs) {
            if now != was {
                let added: BTreeSet<String> = now.difference(was).cloned().collect();
                let removed: BTreeSet<String> = was.difference(now).cloned().collect();
                let details = Details::new().text("added", &joined(&added)).text("removed", &joined(&removed)).text("addrs", &joined(now));
                lines.push(("net", details));
            }
        }
        if addrs.is_some() {
            self.addrs = addrs;
        }
        let now = match wifi {
            WifiRead::Changing => return lines,
            WifiRead::Unknown(error) => {
                if self.wifi_error.as_ref() != Some(&error) {
                    lines.push(("net", Details::new().num("wifi", "unknown").text("error", &error)));
                    self.wifi_error = Some(error);
                }
                return lines;
            }
            WifiRead::Connected(w) => Some(w),
            WifiRead::NotConnected => None,
        };
        // Readable again after it wasn't: what it is now, whether or not that's a change.
        let was_unknown = self.wifi_error.take().is_some();
        match (self.wifi.clone(), &now) {
            (Some(Some(was)), Some(now)) if was.ssid != now.ssid => {
                lines.push(("net", now.add_to(Details::new()).text("was_ssid", &was.ssid)));
            }
            (Some(Some(was)), Some(now)) if was.bssid != now.bssid => {
                lines.push(("roam", now.add_to(Details::new().text("from", &was.bssid).text("to", &now.bssid))));
            }
            (Some(Some(was)), None) => lines.push(("net", Details::new().num("wifi", "none").text("was_ssid", &was.ssid))),
            (None | Some(None), Some(_)) => lines.push(("net", wifi_details(Details::new(), &now))),
            (None, None) => lines.push(("net", Details::new().num("wifi", "none"))),
            (Some(Some(_)), Some(now)) => {
                if was_unknown {
                    lines.push(("net", now.add_to(Details::new())));
                } else if let (Some(signal), Some(logged)) = (now.signal, self.signal_logged) {
                    if signal.abs_diff(logged) >= SIGNAL_STEP {
                        lines.push(("signal", Details::new().num("signal", signal).num("was", logged).text("bssid", &now.bssid)));
                        self.signal_logged = Some(signal);
                    }
                    return lines;
                } else {
                    self.signal_logged = now.signal;
                    return lines;
                }
            }
            (Some(None), None) => {
                if was_unknown {
                    lines.push(("net", Details::new().num("wifi", "none")));
                }
            }
        }
        self.known(now);
        lines
    }

    fn known(&mut self, wifi: Option<Wifi>) {
        self.signal_logged = wifi.as_ref().and_then(|w| w.signal);
        self.wifi = Some(wifi);
    }
}

/// This device's IPv4 addresses with their prefixes, loopback left out.
#[cfg(target_os = "linux")]
fn interface_addrs() -> Option<BTreeSet<String>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut addrs = BTreeSet::new();
    let mut at = list;
    while !at.is_null() {
        let entry = unsafe { &*at };
        at = entry.ifa_next;
        if entry.ifa_addr.is_null() || i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }.to_string_lossy();
        if name == "lo" {
            continue;
        }
        let ipv4 = |sockaddr: *const libc::sockaddr| u32::from_be(unsafe { (*(sockaddr as *const libc::sockaddr_in)).sin_addr.s_addr });
        let ip = std::net::Ipv4Addr::from(ipv4(entry.ifa_addr));
        let prefix = if entry.ifa_netmask.is_null() { 32 } else { ipv4(entry.ifa_netmask).count_ones() };
        addrs.insert(format!("{name}={ip}/{prefix}"));
    }
    unsafe { libc::freeifaddrs(list) };
    Some(addrs)
}

/// `ipconfig`: each adapter's IPv4 address under its heading. Only the word IPv4 is relied on, as
/// the rest of the label is in the language of the laptop.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_ipconfig(text: &str) -> BTreeSet<String> {
    let mut adapter = String::new();
    let mut addrs = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if !trimmed.is_empty() && !trimmed.starts_with(char::is_whitespace) && trimmed.ends_with(':') {
            let heading = trimmed.trim_end_matches(':');
            adapter = heading.split_once(" adapter ").map_or(heading, |(_, name)| name).trim().to_string();
        } else if trimmed.contains("IPv4") {
            if let Some(ip) = trimmed.rsplit(':').next().and_then(first_ipv4) {
                if !ip.starts_with("127.") {
                    addrs.insert(format!("{adapter}={ip}"));
                }
            }
        }
    }
    addrs
}

#[cfg_attr(not(windows), allow(dead_code))]
fn first_ipv4(text: &str) -> Option<String> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let candidate: String = text[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    candidate.parse::<std::net::Ipv4Addr>().ok().map(|ip| ip.to_string())
}

/// `nmcli -t -f ACTIVE,SSID,BSSID,SIGNAL,FREQ,CHAN dev wifi list`: the line of the network in use.
/// Terse output escapes the colons in a BSSID.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_nmcli(text: &str) -> Option<Wifi> {
    text.lines().find_map(|line| {
        let fields = split_terse(line);
        if fields.len() < 6 || fields[0] != "yes" {
            return None;
        }
        Some(Wifi {
            ssid: fields[1].clone(),
            bssid: fields[2].to_lowercase(),
            signal: fields[3].trim().parse().ok(),
            freq: fields[4].clone(),
            channel: fields[5].clone(),
        })
    })
}

fn split_terse(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let (Some(next), Some(field)) = (chars.next(), fields.last_mut()) {
                    field.push(next);
                }
            }
            ':' => fields.push(String::new()),
            c => {
                if let Some(field) = fields.last_mut() {
                    field.push(c);
                }
            }
        }
    }
    fields
}

/// What Windows 11 24H2 and later says in place of Wi-Fi details until location is allowed.
const LOCATION_NOTE: &str = "location permission";
const NO_WIRELESS: &str = "no wireless interface";

/// `netsh wlan show interfaces`, run to the end: the first interface that's connected, or one on
/// its way there, or what kept it from saying.
#[cfg_attr(not(windows), allow(dead_code))]
fn read_netsh(text: &str, success: bool) -> WifiRead {
    if let Some(line) = text.lines().find(|line| line.to_lowercase().contains(LOCATION_NOTE)) {
        return WifiRead::Unknown(line.trim().to_string());
    }
    if text.to_lowercase().contains(NO_WIRELESS) {
        return WifiRead::NotConnected;
    }
    match parse_netsh(text) {
        WifiRead::NotConnected if !success => WifiRead::Unknown(first_line(text, "netsh failed")),
        read => read,
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_netsh(text: &str) -> WifiRead {
    let mut found: Option<(Wifi, String)> = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        if key == "name" {
            if found.as_ref().is_some_and(|(w, state)| state == "connected" && !w.ssid.is_empty()) {
                break;
            }
            found = Some((Wifi::default(), String::new()));
            continue;
        }
        let (wifi, state) = found.get_or_insert_with(|| (Wifi::default(), String::new()));
        match key.as_str() {
            "state" => *state = value.to_ascii_lowercase(),
            "ssid" => wifi.ssid = value.to_string(),
            "bssid" | "ap bssid" => wifi.bssid = value.to_lowercase(),
            "signal" => wifi.signal = value.trim_end_matches('%').trim().parse().ok(),
            "channel" => wifi.channel = value.to_string(),
            "band" => wifi.freq = value.to_string(),
            _ => {}
        }
    }
    match found {
        Some((wifi, state)) if (state == "connected" || state.is_empty()) && !wifi.ssid.is_empty() => WifiRead::Connected(wifi),
        Some((_, state)) if ["associating", "authenticating", "discovering"].contains(&state.as_str()) => WifiRead::Changing,
        _ => WifiRead::NotConnected,
    }
}

fn first_line(text: &str, otherwise: &str) -> String {
    text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or(otherwise).to_string()
}

/// Runs a program for at most `COMMAND_LIMIT`, killing it then: whether it succeeded, and what it
/// wrote, its errors after its output. `Err` when it couldn't start or ran out of time.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn command_text(program: &str, args: &[&str]) -> Result<(bool, String), String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    #[cfg(unix)]
    command.env("LC_ALL", "C");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    match run_limited(command, COMMAND_LIMIT, OnExpiry::Kill) {
        Ran::Finished(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok((out.status.success(), text))
        }
        Ran::TimedOut => Err(format!("{program} didn't answer within {} s", COMMAND_LIMIT.as_secs())),
        Ran::CouldNotStart(why) => Err(format!("{program}: {why}")),
    }
}

/// This device's addresses and Wi-Fi. A scan is never asked for, as one holds up the radio.
fn this_network() -> (Option<BTreeSet<String>>, WifiRead) {
    #[cfg(target_os = "linux")]
    {
        let fields = ["-t", "-f", "ACTIVE,SSID,BSSID,SIGNAL,FREQ,CHAN", "dev", "wifi", "list", "--rescan", "no"];
        let wifi = match command_text("nmcli", &fields) {
            Ok((true, text)) => parse_nmcli(&text).map_or(WifiRead::NotConnected, WifiRead::Connected),
            Ok((false, text)) => WifiRead::Unknown(first_line(&text, "nmcli failed")),
            Err(why) => WifiRead::Unknown(why),
        };
        (interface_addrs(), wifi)
    }
    #[cfg(windows)]
    {
        let addrs = command_text("ipconfig", &[]).ok().filter(|(ok, _)| *ok).map(|(_, text)| parse_ipconfig(&text));
        let wifi = match command_text("netsh", &["wlan", "show", "interfaces"]) {
            Ok((success, text)) => read_netsh(&text, success),
            Err(why) => WifiRead::Unknown(why),
        };
        (addrs, wifi)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        (None, WifiRead::Unknown("not looked at on this system".into()))
    }
}

/// How far the other device's clock is ahead, from its answer at `peer_ms` to a request sent at
/// `sent_ms` and answered at `back_ms`, with the round trip. `None` for an answer too slow to say.
fn clock_offset(sent_ms: i64, back_ms: i64, peer_ms: i64) -> Option<(i64, u32)> {
    let rtt = back_ms - sent_ms;
    (0..=PEER_CLOCK_MAX_RTT_MS).contains(&rtt).then(|| (peer_ms - (sent_ms + back_ms) / 2, rtt as u32))
}

/// Asks the other device the time, on a connection of its own and within a short budget. A
/// failure leaves the offset as it was, and is no news of the link.
fn look_at_peer_clock() {
    let sent = wall_ms();
    let Some(Ok((200, body))) = crate::secure::control::peer_clock_call(CLOCK_PATH) else { return };
    let back = wall_ms();
    let said = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["unix_ms"].as_i64());
    if let Some((ahead, rtt)) = said.and_then(|peer| clock_offset(sent, back, peer)) {
        PEER_CLOCK_MS.store(ahead, Ordering::Relaxed);
        PEER_CLOCK_RTT_MS.store(rtt, Ordering::Relaxed);
    }
}

fn watch_network() {
    let mut net = NetWatch::default();
    let mut quick_until = Instant::now() + QUICK_FOR;
    let mut clock_due = Instant::now();
    loop {
        let asked = LOOK_NOW.swap(false, Ordering::Relaxed);
        let (addrs, wifi) = this_network();
        let signal = match &wifi {
            WifiRead::Connected(w) => w.signal.unwrap_or(NO_SIGNAL),
            _ => NO_SIGNAL,
        };
        SIGNAL.store(signal, Ordering::Relaxed);
        let lines = net.look(addrs, wifi);
        if asked || !lines.is_empty() {
            quick_until = Instant::now() + QUICK_FOR;
        }
        for (word, details) in lines {
            record(word, details);
        }
        if Instant::now() >= clock_due {
            look_at_peer_clock();
            clock_due = Instant::now() + PEER_CLOCK_EVERY;
        }
        let every = if Instant::now() < quick_until { NET_QUICK } else { NET_SLOW };
        let next = Instant::now() + every;
        while Instant::now() < next && !LOOK_NOW.load(Ordering::Relaxed) {
            std::thread::sleep(NET_WAKE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_of(word: &'static str, details: Details, at_ms: i64, offset_s: i32) -> String {
        Entry { at_ms, word, details: details.0 }.line(offset_s)
    }

    #[test]
    fn a_line_has_local_time_with_milliseconds_and_offset_then_the_word_and_details() {
        let figures = Figures { round_trip_ms: Some(9), loss_in: Some(0.0), loss_out: Some(0.0), one_way: false };
        let (word, details) = describe(&LinkEvent::Down { silent_ms: 1352, up_ms: 60_000, figures }, "192.168.8.210:24850");
        assert_eq!(
            line_of(word, details, 1_794_186_805_412, -7 * 3600),
            "2026-11-08T18:13:25.412-07:00 down silent_ms=1352 rtt_ms=9 loss_in=0.00 loss_out=0.00 one_way=false peer=192.168.8.210:24850 up_ms=60000"
        );
        assert_eq!(line_of("start", Details::new(), 951_848_999_999, 5 * 3600 + 1800), "2000-02-29T23:59:59.999+05:30 start");
        assert_eq!(line_of("x", Details::new(), 0, 0), "1970-01-01T00:00:00.000+00:00 x");
    }

    #[test]
    fn up_has_no_figures_as_none_are_known_yet() {
        let (word, details) = describe(&LinkEvent::Up { down_ms: 2610, silent_ms: Some(3990) }, "10.0.0.5:24850");
        assert_eq!(format!("{word} {}", details.0), "up down_ms=2610 silent_ms=3990 peer=10.0.0.5:24850");
    }

    #[test]
    fn days_and_dates_agree_both_ways() {
        for days in [-1_000_000, -1, 0, 59, 60, 11_016, 20_765, 2_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn text_with_spaces_or_quotes_is_quoted_and_figures_not_known_are_dashes() {
        let details = Details::new()
            .text("ssid", "Home Net")
            .text("error", "said \"no\"\nthen")
            .text("empty", "")
            .text("peer", "10.0.0.5")
            .opt("rtt_ms", None::<u32>);
        assert_eq!(details.0, r#"ssid="Home Net" error="said \"no\" then" empty="" peer=10.0.0.5 rtt_ms=-"#);
    }

    #[test]
    fn the_api_gives_the_last_lines_asked_for_within_the_limit() {
        assert_eq!(lines_asked("/api/local/linklog"), Some(DEFAULT_LINES));
        assert_eq!(lines_asked("/api/local/linklog?lines=3"), Some(3));
        assert_eq!(lines_asked("/api/local/linklog?x=1&lines=9999"), Some(KEPT_LINES));
        assert_eq!(lines_asked("/api/local/linklog?lines=many"), Some(DEFAULT_LINES));
        assert_eq!(lines_asked("/api/local/linklogs"), None);
        assert_eq!(lines_asked("/api/local"), None);
        let mut ring = VecDeque::new();
        for n in 0..KEPT_LINES + 5 {
            keep(&mut ring, format!("line {n}"));
        }
        assert_eq!(ring.len(), KEPT_LINES, "the oldest go");
        assert_eq!(tail_of(&ring, 2), format!("line {}\nline {}\n", KEPT_LINES + 3, KEPT_LINES + 4));
        assert_eq!(tail_of(&ring, 0), "");
        assert_eq!(tail_of(&VecDeque::new(), 5), "");
    }

    #[test]
    fn the_clock_is_served_too_as_json() {
        let said: serde_json::Value = serde_json::from_str(&answer(CLOCK_PATH).unwrap()).unwrap();
        assert!((said["unix_ms"].as_i64().unwrap() - wall_ms()).abs() < 5000);
        assert!(!is_log_path(CLOCK_PATH), "it isn't plain text");
    }

    #[test]
    fn a_new_service_starts_with_the_end_of_both_files() {
        let older: String = (0..1500).map(|n| format!("old {n}\n")).collect();
        let newer: String = (0..1000).map(|n| format!("new {n}\n")).collect();
        let ring = seeded(Some(older), Some(newer.clone()));
        assert_eq!(ring.len(), KEPT_LINES);
        assert_eq!((ring.front().unwrap().as_str(), ring.back().unwrap().as_str()), ("old 500", "new 999"));
        assert_eq!(seeded(None, Some(newer)).len(), 1000);
        assert!(seeded(None, None).is_empty());
        assert_eq!(seeded(Some("a\n\nb".into()), None), VecDeque::from(["a".to_string(), "b".to_string()]), "no blank lines");
    }

    #[test]
    fn the_end_of_a_file_is_read_back_whatever_bytes_are_in_it() {
        let dir = scratch("tail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let mut bytes = b"one\nt\xffwo\n".to_vec();
        bytes.extend((0..100).flat_map(|n| format!("line {n:03}\n").into_bytes()));
        std::fs::write(&path, &bytes).unwrap();
        let all = read_tail_within(&path, 1 << 20).unwrap();
        assert!(all.starts_with("one\nt\u{fffd}wo\nline 000\n"), "a bad byte costs only itself: {:?}", &all[..20]);
        let end = read_tail_within(&path, 25).unwrap();
        assert_eq!(end, "line 098\nline 099\n", "only whole lines from the end");
        assert_eq!(read_tail_within(&dir.join("none"), 25), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_clock_that_stops_in_sleep_still_goes_forward() {
        let first = awake_ms();
        std::thread::sleep(Duration::from_millis(30));
        let gone = awake_ms() - first;
        assert!((20..2000).contains(&gone), "{gone}");
    }

    #[test]
    fn the_other_clock_is_measured_from_the_middle_of_the_round_trip() {
        assert_eq!(clock_offset(1000, 1020, 5010), Some((4000, 20)));
        assert_eq!(clock_offset(1000, 1010, 900), Some((-105, 10)));
        assert_eq!(clock_offset(1000, 4000, 2500), None, "too slow to say");
        assert_eq!(clock_offset(1000, 990, 1000), None, "this clock went back meanwhile");
        assert_eq!(peer_clock(Details::new(), 4000, 20).0, "peer_clock_ms=+4000 peer_clock_rtt_ms=20");
        assert_eq!(peer_clock(Details::new(), -105, 10).0, "peer_clock_ms=-105 peer_clock_rtt_ms=10");
        assert_eq!(peer_clock(Details::new(), NO_CLOCK, 0).0, "peer_clock_ms=-");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-linklog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_file_moves_aside_once_it_passes_its_limit() {
        let dir = scratch("rotate");
        let path = dir.join(FILE_NAME);
        let mut file = LogFile::new(path.clone(), 40);
        for n in 0..3 {
            file.write(&format!("line {n} xxxxxxxx"));
        }
        let old = std::fs::read_to_string(dir.join("link.log.1")).unwrap();
        assert_eq!(old, "line 0 xxxxxxxx\nline 1 xxxxxxxx\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line 2 xxxxxxxx\n");
        for n in 3..5 {
            file.write(&format!("line {n} xxxxxxxx"));
        }
        assert_eq!(std::fs::read_to_string(dir.join("link.log.1")).unwrap(), "line 2 xxxxxxxx\nline 3 xxxxxxxx\n", "the older copy is replaced");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line 4 xxxxxxxx\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_move_aside_is_emptied_and_never_grows_past_its_limit() {
        let dir = scratch("stuck");
        let path = dir.join(FILE_NAME);
        // A folder with something in it where the older copy goes: it can't be removed or replaced.
        std::fs::create_dir_all(dir.join("link.log.1").join("in the way")).unwrap();
        let mut file = LogFile::new(path.clone(), 40);
        for n in 0..20 {
            file.write(&format!("line {n:02} xxxxxxx"));
            assert!(std::fs::metadata(&path).unwrap().len() <= 40, "after line {n}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line 18 xxxxxxx\nline 19 xxxxxxx\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_carries_on_from_its_size_and_one_that_cannot_open_is_no_harm() {
        let dir = scratch("resume");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), "x".repeat(35) + "\n").unwrap();
        let mut file = LogFile::new(dir.join(FILE_NAME), 40);
        file.write("next");
        assert!(dir.join("link.log.1").exists(), "36 bytes there already, and 5 more pass 40");
        // A folder where the file should be: nothing is written, and nothing breaks.
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(blocked.join(FILE_NAME)).unwrap();
        let mut file = LogFile::new(blocked.join(FILE_NAME), 40);
        file.write("lost");
        assert!(file.file.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_loop_that_comes_round_late_is_a_stall_and_its_worst_goes_in_the_sample() {
        let mut clock = LoopClock::new("watchdog", 100);
        assert_eq!(clock.lateness(1000), None, "the first time round says nothing");
        assert_eq!(clock.lateness(1100), None);
        assert_eq!(clock.lateness(1350), None, "150 ms late is no stall");
        assert_eq!(clock.lateness(1800), Some(350));
        assert_eq!(clock.lateness(1900), None);
        assert_eq!(clock.take_worst(), 350);
        assert_eq!(clock.take_worst(), 0, "each sample starts again");
    }

    fn up_figures() -> Figures {
        Figures { round_trip_ms: Some(12), loss_in: Some(1.5), loss_out: Some(0.0), one_way: false }
    }

    fn look(now_ms: u64, online: bool, online_since: u64, last_alive: u64, quiet: Option<u64>) -> Look {
        let figures = if online { up_figures() } else { Figures::default() };
        let quiet = quiet.map(|ms| Quiet { ms, busy: false });
        Look { now_ms, online, online_since, last_alive, quiet, figures, round_trips: None, packets: (0, 0), loop_late_ms: 0 }
    }

    #[test]
    fn up_and_down_say_how_long_and_down_keeps_the_figures_from_before() {
        let mut watch = LinkWatch::starting(1000, (0, 0));
        assert_eq!(watch.look(look(1040, false, 0, 0, None)), vec![]);
        let up = watch.look(look(3000, true, 2990, 2990, None));
        assert_eq!(up, vec![LinkEvent::Up { down_ms: 1990, silent_ms: None }]);
        assert_eq!(watch.look(look(5000, true, 2990, 4990, None)), vec![]);
        // The link layer has already forgotten its figures by the look that finds it down.
        let down = watch.look(look(6380, false, 2990, 5000, None));
        assert_eq!(down, vec![LinkEvent::Down { silent_ms: 1380, up_ms: 3390, figures: up_figures() }]);
        let back = watch.look(look(9000, true, 8990, 8990, Some(3990)));
        assert_eq!(back, vec![LinkEvent::Up { down_ms: 2610, silent_ms: Some(3990) }]);
    }

    #[test]
    fn a_stall_that_recovers_is_a_gap_of_its_length_with_what_was_going_on() {
        let mut watch = LinkWatch::starting(0, (0, 0));
        watch.look(look(100, true, 100, 100, None));
        assert_eq!(watch.look(look(140, true, 100, 140, None)), vec![]);
        let mut stalled = look(2000, true, 100, 1990, None);
        stalled.quiet = Some(Quiet { ms: 350, busy: true });
        stalled.packets = (300, 0);
        let gap = LinkEvent::Gap { silent_ms: 350, busy: true, figures: up_figures(), in_per_s: 150.0 };
        assert_eq!(watch.look(stalled), vec![gap]);
        assert!(watch.online, "a gap leaves the link up");
        let (word, details) = describe(&gap, "-");
        assert_eq!(format!("{word} {}", details.0), "gap silent_ms=350 while_busy=true loss_in=1.50 loss_out=0.00 in_pps=150.0");
    }

    #[test]
    fn a_drop_between_two_looks_is_still_a_down_and_an_up() {
        let mut watch = LinkWatch::starting(0, (0, 0));
        watch.look(look(100, true, 100, 100, None));
        let events = watch.look(look(1700, true, 1690, 1690, Some(1500)));
        assert_eq!(
            events,
            vec![LinkEvent::Down { silent_ms: 1500, up_ms: 90, figures: up_figures() }, LinkEvent::Up { down_ms: 0, silent_ms: Some(1500) }]
        );
    }

    #[test]
    fn the_other_device_going_deaf_and_hearing_again_are_events() {
        let mut watch = LinkWatch::starting(0, (0, 0));
        watch.look(look(100, true, 100, 100, None));
        let mut deaf = look(3000, true, 100, 3000, None);
        deaf.figures = Figures { one_way: true, ..Figures::default() };
        assert_eq!(watch.look(deaf), vec![LinkEvent::OneWay { on: true, figures: deaf.figures }]);
        assert_eq!(watch.look(deaf), vec![], "said once");
        let (word, details) = describe(&LinkEvent::OneWay { on: true, figures: deaf.figures }, "-");
        assert_eq!(format!("{word} {}", details.0), "one_way state=on rtt_ms=- loss_in=- loss_out=-");
        assert_eq!(watch.look(look(3400, true, 100, 3400, None)), vec![LinkEvent::OneWay { on: false, figures: up_figures() }]);
        // Down while deaf, then up: it starts again from hearing.
        watch.look(deaf);
        watch.look(look(5000, false, 100, 3400, None));
        assert_eq!(watch.look(look(6000, true, 6000, 6000, Some(2600))), vec![LinkEvent::Up { down_ms: 1000, silent_ms: Some(2600) }]);
    }

    #[test]
    fn a_sample_every_ten_seconds_with_the_round_trips_packets_and_lateness_in_between() {
        let mut watch = LinkWatch::starting(0, (100, 50));
        let trips = |min_ms, max_ms, sum_ms, count| Some(RoundTrips { min_ms, max_ms, sum_ms, count });
        let mut first = look(5000, true, 10, 5000, None);
        first.round_trips = trips(8.0, 20.0, 28.0, 2);
        first.loop_late_ms = 40;
        watch.look(first);
        let mut second = look(10_000, true, 10, 10_000, None);
        second.round_trips = trips(5.0, 9.0, 14.0, 2);
        second.packets = (600, 350);
        second.loop_late_ms = 7;
        let events = watch.look(second);
        let Some(LinkEvent::Sample { online, round_trips: Some(t), in_per_s, out_per_s, loop_late_ms, .. }) = events.last().copied() else {
            panic!("{events:?}")
        };
        assert!(online);
        assert_eq!((t.min_ms, t.max_ms, t.count, t.average_ms()), (5.0, 20.0, 4, 10.5));
        assert_eq!((in_per_s, out_per_s, loop_late_ms), (50.0, 30.0, 40));
        let (word, details) = describe(&events[events.len() - 1], "-");
        assert_eq!(word, "sample");
        assert_eq!(
            details.0,
            "link=up rtt_min_ms=5 rtt_avg_ms=10 rtt_max_ms=20 answers=4 loss_in=1.50 loss_out=0.00 one_way=false in_pps=50.0 out_pps=30.0 loop_late_ms=40"
        );
        let mut third = look(20_000, false, 10, 10_000, None);
        third.packets = (600, 350);
        let quiet = watch.look(third);
        assert!(matches!(quiet.last(), Some(LinkEvent::Sample { online: false, round_trips: None, loop_late_ms: 0, .. })), "{quiet:?}");
    }

    #[test]
    fn audio_figures_are_what_came_since_the_last_sample() {
        let details = audio_since(Details::new(), "audio_", [500, 7, 3, 2, 1, 1], [300, 5, 3, 0, 0, 1], 80);
        assert_eq!(
            details.0,
            "audio_received=200 audio_concealed=2 audio_repaired=0 audio_late=2 audio_trimmed=1 audio_restarts=0 audio_buffer_max_ms=80"
        );
        let mic = audio_since(Details::new(), "mic_", [10, 0, 0, 0, 0, 0], [900, 0, 0, 0, 0, 0], 20);
        assert!(mic.0.starts_with("mic_received=10 "), "{}", mic.0);
    }

    #[test]
    fn send_errors_are_told_apart_by_kind() {
        let kind = |raw| SEND_KINDS[send_kind(&std::io::Error::from_raw_os_error(raw))];
        // ENOBUFS, ENETUNREACH, EHOSTUNREACH, EAGAIN, ECONNREFUSED, EACCES, EMSGSIZE, EIO.
        #[cfg(not(windows))]
        let raw = [105, 101, 113, 11, 111, 13, 90, 5];
        #[cfg(windows)]
        let raw = [10055, 10051, 10065, 10035, 10061, 10013, 10040, 10022];
        assert_eq!(raw.map(kind), ["no_buffer", "unreachable", "unreachable", "would_block", "refused", "denied", "too_big", "other"]);
    }

    #[test]
    fn send_errors_say_how_many_since_the_last_sample_and_which() {
        let none = send_errors(Details::new(), [0; 7], [0; 7], NO_KIND);
        assert_eq!(none.0, "send_errors=0");
        let some = send_errors(Details::new(), [0, 5, 2, 0, 0, 0, 0], [0, 1, 0, 0, 0, 0, 0], 2);
        assert_eq!(some.0, "send_errors=6 send_error_last=unreachable send_errors_by=no_buffer:4,unreachable:2");
    }

    #[test]
    fn sleep_shows_as_lost_time_on_either_clock() {
        assert_eq!(asleep(1003, 1001, 1000), None);
        assert_eq!(asleep(4000, 1000, 1000), None, "a clock set three seconds on");
        assert_eq!(asleep(61_000, 1000, 1000), Some((60_000, "clock")), "Linux: the monotonic clock stopped");
        assert_eq!(asleep(61_000, 61_000, 1000), Some((60_000, "late")), "both went on, and the look came late");
        assert_eq!(asleep(-50_000, 1000, 1000), None, "a clock set back is no sleep");
    }

    fn set(items: &[&str]) -> Option<BTreeSet<String>> {
        Some(items.iter().map(|s| s.to_string()).collect())
    }

    fn wifi(bssid: &str, signal: u32) -> WifiRead {
        WifiRead::Connected(Wifi { ssid: "Home Net".into(), bssid: bssid.into(), signal: Some(signal), channel: "36".into(), freq: "5180 MHz".into() })
    }

    fn texts(lines: Vec<(&'static str, Details)>) -> Vec<String> {
        lines.into_iter().map(|(word, details)| format!("{word} {}", details.0)).collect()
    }

    #[test]
    fn the_network_is_logged_once_then_only_what_changes() {
        let mut net = NetWatch::default();
        let first = texts(net.look(set(&["wlan0=192.168.8.5/24"]), wifi("aa:bb", 70)));
        assert_eq!(first, vec![r#"net addrs=wlan0=192.168.8.5/24 ssid="Home Net" bssid=aa:bb signal=70 chan=36 freq="5180 MHz""#]);
        assert!(net.look(set(&["wlan0=192.168.8.5/24"]), wifi("aa:bb", 75)).is_empty(), "a small move in signal");
        assert!(net.look(None, wifi("aa:bb", 79)).is_empty(), "addresses that couldn't be read are no change");
        assert_eq!(texts(net.look(set(&["wlan0=192.168.8.5/24"]), wifi("aa:bb", 60))), vec!["signal signal=60 was=70 bssid=aa:bb"]);
        let moved = texts(net.look(set(&["wlan0=192.168.8.7/24", "tun0=10.8.0.2/24"]), wifi("cc:dd", 64)));
        assert_eq!(
            moved,
            vec![
                "net added=tun0=10.8.0.2/24,wlan0=192.168.8.7/24 removed=wlan0=192.168.8.5/24 addrs=tun0=10.8.0.2/24,wlan0=192.168.8.7/24",
                r#"roam from=aa:bb to=cc:dd ssid="Home Net" bssid=cc:dd signal=64 chan=36 freq="5180 MHz""#,
            ]
        );
        assert!(net.look(set(&["wlan0=192.168.8.7/24", "tun0=10.8.0.2/24"]), wifi("cc:dd", 56)).is_empty(), "measured from the roam");
        assert_eq!(
            texts(net.look(set(&[]), WifiRead::NotConnected)),
            vec!["net added=- removed=tun0=10.8.0.2/24,wlan0=192.168.8.7/24 addrs=-", r#"net wifi=none was_ssid="Home Net""#]
        );
    }

    #[test]
    fn wifi_that_cannot_be_read_is_unknown_and_no_disconnect() {
        let mut net = NetWatch::default();
        net.look(set(&[]), wifi("aa:bb", 70));
        let stuck = || WifiRead::Unknown("nmcli didn't answer within 3 s".into());
        assert_eq!(texts(net.look(set(&[]), stuck())), vec![r#"net wifi=unknown error="nmcli didn't answer within 3 s""#]);
        assert!(net.look(set(&[]), stuck()).is_empty(), "said once while it stays the same");
        let other = WifiRead::Unknown("NetworkManager is not running.".into());
        assert_eq!(texts(net.look(set(&[]), other)), vec![r#"net wifi=unknown error="NetworkManager is not running.""#]);
        assert_eq!(texts(net.look(set(&[]), wifi("aa:bb", 72))), vec![r#"net ssid="Home Net" bssid=aa:bb signal=72 chan=36 freq="5180 MHz""#], "back, as it was");
        assert_eq!(texts(net.look(set(&[]), wifi("aa:bb", 73))), Vec::<String>::new());
        net.look(set(&[]), stuck());
        assert_eq!(texts(net.look(set(&[]), wifi("cc:dd", 70)))[0].split(' ').next(), Some("roam"), "measured against the last known");
        let mut first = NetWatch::default();
        assert_eq!(texts(first.look(None, stuck())), vec![r#"net addrs=- wifi=unknown error="nmcli didn't answer within 3 s""#]);
        assert!(first.look(None, stuck()).is_empty());
        assert_eq!(texts(first.look(None, WifiRead::NotConnected)), vec!["net wifi=none"]);
    }

    #[test]
    fn a_roam_through_authenticating_is_a_roam() {
        let mut net = NetWatch::default();
        net.look(set(&[]), wifi("aa:bb", 70));
        assert!(net.look(set(&[]), WifiRead::Changing).is_empty());
        assert_eq!(texts(net.look(set(&[]), wifi("cc:dd", 70)))[0].split(' ').next(), Some("roam"));
    }

    #[test]
    fn interface_addresses_leave_out_loopback() {
        #[cfg(target_os = "linux")]
        {
            let addrs = interface_addrs().unwrap();
            assert!(addrs.iter().all(|a| !a.starts_with("lo=") && a.contains('/')), "{addrs:?}");
        }
    }

    #[test]
    fn nmcli_output_gives_the_network_in_use() {
        let text = "no:Neighbour:11\\:22\\:33\\:44\\:55\\:66:40:2437 MHz:6\n\
                    yes:Home\\:Net:AA\\:BB\\:CC\\:DD\\:EE\\:FF:82:5180 MHz:36\n";
        let wifi = parse_nmcli(text).unwrap();
        assert_eq!(wifi.ssid, "Home:Net");
        assert_eq!(wifi.bssid, "aa:bb:cc:dd:ee:ff");
        assert_eq!((wifi.signal, wifi.freq.as_str(), wifi.channel.as_str()), (Some(82), "5180 MHz", "36"));
        assert_eq!(parse_nmcli("no:Neighbour:11\\:22:40:2437 MHz:6\n"), None, "nothing connected");
    }

    #[test]
    fn netsh_output_gives_the_connected_interface() {
        let text = "\r\nThere is 1 interface on the system: \r\n\r\n    Name                   : Wi-Fi\r\n    Description            : Intel(R) Wi-Fi 6E AX211 160MHz\r\n    \
                    State                  : connected\r\n    SSID                   : Home Net\r\n    AP BSSID               : 3c:84:6a:12:34:56\r\n    \
                    Band                   : 5 GHz\r\n    Channel                : 36\r\n    Receive rate (Mbps)    : 1201\r\n    Signal                 : 91% \r\n    \
                    Profile                : Home Net\r\n";
        let WifiRead::Connected(wifi) = read_netsh(text, true) else { panic!("connected") };
        assert_eq!((wifi.ssid.as_str(), wifi.bssid.as_str()), ("Home Net", "3c:84:6a:12:34:56"));
        assert_eq!((wifi.signal, wifi.channel.as_str(), wifi.freq.as_str()), (Some(91), "36", "5 GHz"));
        let older = "    Name : Wi-Fi\n    State : connected\n    SSID : Cafe\n    BSSID : 00:11:22:33:44:55\n    Signal : 40%\n";
        assert!(matches!(read_netsh(older, true), WifiRead::Connected(w) if w.bssid == "00:11:22:33:44:55"));
        assert_eq!(read_netsh("    Name : Wi-Fi\n    State : disconnected\n", true), WifiRead::NotConnected);
        assert_eq!(read_netsh("    Name : Wi-Fi\n    State : authenticating\n    SSID : Cafe\n", true), WifiRead::Changing);
        assert_eq!(read_netsh("    Name : Wi-Fi\n    State : associating\n", true), WifiRead::Changing);
    }

    #[test]
    fn netsh_saying_why_it_cannot_tell_is_unknown() {
        let location = "\r\nNetwork shell commands need location permission to access WLAN information. Turn on Location services on the Location page in Privacy & security settings.\r\n\r\nFunction WlanQueryInterface returns error 5:\r\nAccess is denied.\r\n";
        let WifiRead::Unknown(error) = read_netsh(location, false) else { panic!("unknown") };
        assert!(error.starts_with("Network shell commands need location permission"), "{error}");
        assert_eq!(read_netsh("There is no wireless interface on the system.\r\n", true), WifiRead::NotConnected);
        assert_eq!(read_netsh("The Wireless AutoConfig Service (wlansvc) is not running.\r\n", false), WifiRead::Unknown("The Wireless AutoConfig Service (wlansvc) is not running.".into()));
    }

    #[test]
    fn ipconfig_output_gives_each_adapters_ipv4_address() {
        let text = "\r\nWindows IP Configuration\r\n\r\n\r\nWireless LAN adapter Wi-Fi:\r\n\r\n   Connection-specific DNS Suffix  . : lan\r\n   \
                    IPv6 Address. . . . . . . . . . . : fd00::1234\r\n   IPv4 Address. . . . . . . . . . . : 192.168.8.210(Preferred) \r\n   \
                    Subnet Mask . . . . . . . . . . . : 255.255.255.0\r\n\r\nEthernet adapter Ethernet 2:\r\n\r\n   Media State . . . . . . . . . . . : Media disconnected\r\n\r\n\
                    Ethernet adapter vEthernet (WSL):\r\n\r\n   IPv4-Adresse  . . . . . . . . . . : 172.20.0.1\r\n";
        assert_eq!(parse_ipconfig(text), set(&["Wi-Fi=192.168.8.210", "vEthernet (WSL)=172.20.0.1"]).unwrap());
    }

    #[test]
    fn a_program_that_hangs_is_cut_off_and_says_so() {
        #[cfg(unix)]
        {
            let started = Instant::now();
            let said = command_text("sleep", &["30"]);
            assert_eq!(said, Err("sleep didn't answer within 3 s".to_string()));
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(command_text("/nonexistent/nmcli", &[]).is_err());
            assert_eq!(command_text("sh", &["-c", "echo out; echo err >&2; exit 2"]), Ok((false, "out\nerr\n".to_string())));
        }
    }
}
