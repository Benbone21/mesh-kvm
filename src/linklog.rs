//! The link log: what the link between the two devices did, one line per event, so that a drop can
//! be looked into afterwards from both ends. Lines go to `link.log` in the config folder, which
//! moves to `link.log.1` once it passes 2 MB, and the last ones stay in memory for the API, which
//! still has them when the file can't be written.
//!
//! Nothing here may hold up an input loop. A loop looks at the link a few times a second, which
//! costs a comparison or two, and hands any event it finds to a bounded queue; a thread of its own
//! writes them. A full queue drops the event. A second thread watches this device's network and
//! its clock.

use crate::input::link::{LinkQuality, PeerLink, RoundTrips};
use crate::router::Router;
use std::collections::{BTreeSet, VecDeque};
use std::fmt::{Display, Write as _};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const FILE_NAME: &str = "link.log";
/// What the API serves, and how many lines it gives when not told.
pub const PATH: &str = "/api/local/linklog";
pub const DEFAULT_LINES: usize = 500;
/// Lines kept in memory, and the most the API gives.
pub const KEPT_LINES: usize = 2000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const QUEUE: usize = 4096;
const SAMPLE_EVERY_MS: u64 = 10_000;
const NET_EVERY: Duration = Duration::from_secs(5);
const CLOCK_EVERY: Duration = Duration::from_secs(1);
/// Lost time past this between two looks at the clock means the device slept, or this didn't run.
const ASLEEP_MS: u64 = 5000;
/// Wi-Fi signal is logged when it moves this far, in percent, from what was last logged.
const SIGNAL_STEP: u32 = 10;
/// How long stopping waits for the lines before it to be written.
const FLUSH_WAIT: Duration = Duration::from_millis(500);
const REOPEN_EVERY: Duration = Duration::from_secs(10);
const NO_SIGNAL: u32 = u32::MAX;

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
    let file = LogFile::new(crate::settings::config_dir().join(FILE_NAME), MAX_FILE_BYTES);
    let _ = std::thread::Builder::new().name("mesh-kvm-linklog".into()).spawn(move || write_lines(rx, file));
    let _ = std::thread::Builder::new().name("mesh-kvm-linklog-net".into()).spawn(watch_device);
    let details = Details::new()
        .text("version", &crate::update::trust::running_version().to_string())
        .text("os", std::env::consts::OS)
        .text("name", &crate::settings::device_name())
        .text("peer", if peer.is_empty() { "-" } else { peer })
        .num("pid", std::process::id());
    record("start", details);
}

/// A `stop` line, waited for briefly so that it's written before the process goes.
pub fn stop(reason: &str, error: Option<&str>) {
    let Some(tx) = QUEUE_TX.get() else { return };
    let mut details = Details::new().text("reason", reason);
    if let Some(error) = error {
        details = details.text("error", error);
    }
    record("stop", details);
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
fn packets() -> (u64, u64) {
    (PACKETS_IN.load(Ordering::Relaxed), PACKETS_OUT.load(Ordering::Relaxed))
}

/// The API's answer for `path`, if it is the link log's: the last lines, as plain text.
pub fn answer(path: &str) -> Option<String> {
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

/// The end of this device's `link.log`, for `--link-log` when the service isn't answering.
pub fn file_tail(lines: usize) -> Option<String> {
    let text = std::fs::read_to_string(crate::settings::config_dir().join(FILE_NAME)).ok()?;
    let ring: VecDeque<String> = text.lines().map(str::to_string).collect();
    Some(tail_of(&ring, lines))
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
/// and then, and the lines meanwhile are only in memory.
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
            self.open();
        }
        let bytes = line.len() as u64 + 1;
        if self.file.is_some() && self.size > 0 && self.size + bytes > self.limit {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else { return };
        if writeln!(file, "{line}").and_then(|_| file.flush()).is_ok() {
            self.size += bytes;
        } else {
            self.file = None;
            self.retry_at = Some(Instant::now() + REOPEN_EVERY);
        }
    }

    fn open(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            Ok(file) => {
                self.size = file.metadata().map_or(0, |m| m.len());
                self.file = Some(file);
                self.retry_at = None;
            }
            Err(_) => self.retry_at = Some(Instant::now() + REOPEN_EVERY),
        }
    }

    /// Closed first, as Windows can't move an open file.
    fn rotate(&mut self) {
        self.file = None;
        let old = self.path.with_file_name(format!("{FILE_NAME}.1"));
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&self.path, &old);
        self.open();
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

// ---- The link ----

/// The figures that go with `up` and `down`.
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
    pub quiet: Option<u64>,
    pub figures: Figures,
    pub round_trips: Option<RoundTrips>,
    pub packets: (u64, u64),
}

impl Look {
    /// What the router and link say now. This takes their quiet spell and round trips.
    pub fn of(router: &mut Router, link: &mut PeerLink, now_ms: u64) -> Look {
        Look {
            now_ms,
            online: router.online(),
            online_since: router.online_since(),
            last_alive: router.last_alive(),
            quiet: router.take_quiet(),
            figures: (&link.quality(router, now_ms)).into(),
            round_trips: link.take_round_trips(),
            packets: packets(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinkEvent {
    /// `silent_ms` is how long nothing came before this, unknown for the first.
    Up { down_ms: u64, silent_ms: Option<u64>, figures: Figures },
    /// With the figures from just before.
    Down { silent_ms: u64, up_ms: u64, figures: Figures },
    /// A silence over `QUIET_MS` that ended with the link still up.
    Gap { silent_ms: u64 },
    Sample { online: bool, round_trips: Option<RoundTrips>, figures: Figures, in_per_s: f32, out_per_s: f32 },
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
    round_trips: Option<RoundTrips>,
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
            round_trips: None,
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
        match (self.online, look.online) {
            (false, true) => {
                let down_ms = look.online_since.saturating_sub(self.changed_at);
                events.push(LinkEvent::Up { down_ms, silent_ms: look.quiet, figures: look.figures });
                self.changed_at = look.online_since;
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
                let silent_ms = look.quiet.unwrap_or(0);
                let up_ms = look.online_since.saturating_sub(self.online_since + silent_ms);
                events.push(LinkEvent::Down { silent_ms, up_ms, figures: self.figures });
                events.push(LinkEvent::Up { down_ms: 0, silent_ms: look.quiet, figures: look.figures });
                self.changed_at = look.online_since;
            }
            (true, true) => {
                if let Some(silent_ms) = look.quiet {
                    events.push(LinkEvent::Gap { silent_ms });
                }
            }
            (false, false) => {}
        }
        self.online = look.online;
        self.online_since = look.online_since;
        if look.online {
            self.figures = look.figures;
        }
        let window = look.now_ms.saturating_sub(self.sampled_at);
        if window >= SAMPLE_EVERY_MS {
            let seconds = window as f32 / 1000.0;
            let per_s = |now: u64, was: u64| now.saturating_sub(was) as f32 / seconds;
            events.push(LinkEvent::Sample {
                online: look.online,
                round_trips: self.round_trips.take(),
                figures: look.figures,
                in_per_s: per_s(look.packets.0, self.sampled_packets.0),
                out_per_s: per_s(look.packets.1, self.sampled_packets.1),
            });
            self.sampled_at = look.now_ms;
            self.sampled_packets = look.packets;
        }
        events
    }
}

/// Records what a `LinkWatch` found, with what else goes with it: the latest Wi-Fi signal on a
/// `down`, and audio and the control connection on a sample. Only atomics are read here.
pub fn note(events: Vec<LinkEvent>) {
    for event in events {
        let (word, mut details) = describe(&event, PEER.get().map_or("-", String::as_str));
        match event {
            LinkEvent::Down { .. } => {
                let signal = SIGNAL.load(Ordering::Relaxed);
                if signal != NO_SIGNAL {
                    details = details.num("signal", signal);
                }
                LOOK_NOW.store(true, Ordering::Relaxed);
            }
            LinkEvent::Sample { .. } => {
                use crate::audio::{AUDIO_RX_BUFFER_MS, AUDIO_RX_LIVE, AUDIO_RX_LOSS};
                if AUDIO_RX_LIVE.load(Ordering::Relaxed) {
                    let loss = f64::from(AUDIO_RX_LOSS.load(Ordering::Relaxed)) / 10.0;
                    details = details.num("audio_loss", format!("{loss:.1}")).num("audio_buffer_ms", AUDIO_RX_BUFFER_MS.load(Ordering::Relaxed));
                }
                details = details.num("control", crate::secure::control::control_state());
            }
            _ => {}
        }
        record(word, details);
    }
}

fn figures(details: Details, f: &Figures) -> Details {
    details
        .opt("rtt_ms", f.round_trip_ms)
        .opt("loss_in", f.loss_in.map(|l| format!("{l:.2}")))
        .opt("loss_out", f.loss_out.map(|l| format!("{l:.2}")))
        .num("one_way", f.one_way)
}

fn describe(event: &LinkEvent, peer: &str) -> (&'static str, Details) {
    match event {
        LinkEvent::Up { down_ms, silent_ms, figures: f } => {
            let details = Details::new().num("down_ms", down_ms).opt("silent_ms", *silent_ms);
            ("up", figures(details, f).text("peer", peer))
        }
        LinkEvent::Down { silent_ms, up_ms, figures: f } => {
            let details = Details::new().num("silent_ms", silent_ms);
            ("down", figures(details, f).text("peer", peer).num("up_ms", up_ms))
        }
        LinkEvent::Gap { silent_ms } => ("gap", Details::new().num("silent_ms", silent_ms)),
        LinkEvent::Sample { online, round_trips, figures: f, in_per_s, out_per_s } => {
            let ms = |pick: fn(&RoundTrips) -> f32| round_trips.as_ref().map(|t| format!("{:.0}", pick(t)));
            let details = Details::new()
                .num("link", if *online { "up" } else { "down" })
                .opt("rtt_min_ms", ms(|t| t.min_ms))
                .opt("rtt_avg_ms", ms(RoundTrips::average_ms))
                .opt("rtt_max_ms", ms(|t| t.max_ms))
                .num("answers", round_trips.map_or(0, |t| t.count))
                .opt("loss_in", f.loss_in.map(|l| format!("{l:.2}")))
                .opt("loss_out", f.loss_out.map(|l| format!("{l:.2}")))
                .num("one_way", f.one_way)
                .num("in_pps", format!("{in_per_s:.1}"))
                .num("out_pps", format!("{out_per_s:.1}"));
            ("sample", details)
        }
    }
}

// ---- The device: its network and its clock ----

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

/// Compares each look at the network with the one before and says what changed.
#[derive(Default)]
struct NetWatch {
    addrs: Option<BTreeSet<String>>,
    /// `None` before the first look.
    wifi: Option<Option<Wifi>>,
    signal_logged: Option<u32>,
}

fn joined(addrs: &BTreeSet<String>) -> String {
    if addrs.is_empty() {
        "-".to_string()
    } else {
        addrs.iter().cloned().collect::<Vec<_>>().join(",")
    }
}

impl NetWatch {
    /// `addrs` is `None` when they couldn't be read this time, which is no change.
    fn look(&mut self, addrs: Option<BTreeSet<String>>, wifi: Option<Wifi>) -> Vec<(&'static str, Details)> {
        let mut lines = Vec::new();
        let Some(was_wifi) = self.wifi.clone() else {
            let mut details = Details::new().text("addrs", &addrs.as_ref().map_or("-".to_string(), joined));
            details = match &wifi {
                Some(w) => w.add_to(details),
                None => details.num("wifi", "none"),
            };
            lines.push(("net", details));
            self.signal_logged = wifi.as_ref().and_then(|w| w.signal);
            self.addrs = addrs;
            self.wifi = Some(wifi);
            return lines;
        };
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
        match (&was_wifi, &wifi) {
            (Some(was), Some(now)) if was.ssid != now.ssid => {
                lines.push(("net", now.add_to(Details::new()).text("was_ssid", &was.ssid)));
                self.signal_logged = now.signal;
            }
            (Some(was), Some(now)) if was.bssid != now.bssid => {
                lines.push(("roam", now.add_to(Details::new().text("from", &was.bssid).text("to", &now.bssid))));
                self.signal_logged = now.signal;
            }
            (Some(was), None) => lines.push(("net", Details::new().num("wifi", "none").text("was_ssid", &was.ssid))),
            (None, Some(now)) => {
                lines.push(("net", now.add_to(Details::new())));
                self.signal_logged = now.signal;
            }
            (Some(_), Some(now)) => {
                if let (Some(signal), Some(logged)) = (now.signal, self.signal_logged) {
                    if signal.abs_diff(logged) >= SIGNAL_STEP {
                        lines.push(("signal", Details::new().num("signal", signal).num("was", logged).text("bssid", &now.bssid)));
                        self.signal_logged = Some(signal);
                    }
                } else if now.signal.is_some() {
                    self.signal_logged = now.signal;
                }
            }
            (None, None) => {}
        }
        self.wifi = Some(wifi);
        lines
    }
}

/// `ip -o -4 addr show`: `2: wlp3s0    inet 192.168.8.5/24 brd ...`, loopback left out.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_ip_addr(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.nth(1)?;
            if words.next()? != "inet" || name == "lo" {
                return None;
            }
            Some(format!("{name}={}", words.next()?))
        })
        .collect()
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

/// `netsh wlan show interfaces`: the first interface that's connected.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_netsh(text: &str) -> Option<Wifi> {
    let mut found: Option<Wifi> = None;
    let mut connected = true;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        match key.as_str() {
            "name" => {
                if found.as_ref().is_some_and(|w| connected && !w.ssid.is_empty()) {
                    break;
                }
                found = Some(Wifi::default());
                connected = true;
            }
            "state" => connected = value.eq_ignore_ascii_case("connected"),
            "ssid" => found.get_or_insert_with(Wifi::default).ssid = value.to_string(),
            "bssid" | "ap bssid" => found.get_or_insert_with(Wifi::default).bssid = value.to_lowercase(),
            "signal" => found.get_or_insert_with(Wifi::default).signal = value.trim_end_matches('%').trim().parse().ok(),
            "channel" => found.get_or_insert_with(Wifi::default).channel = value.to_string(),
            "band" => found.get_or_insert_with(Wifi::default).freq = value.to_string(),
            _ => {}
        }
    }
    found.filter(|w| connected && !w.ssid.is_empty())
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn command_text(program: &str, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new(program);
    command.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    command.env("LC_ALL", "C");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let out = command.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// This device's addresses and Wi-Fi. A scan is never asked for, as one holds up the radio.
fn this_network() -> (Option<BTreeSet<String>>, Option<Wifi>) {
    #[cfg(target_os = "linux")]
    {
        let addrs = command_text("ip", &["-o", "-4", "addr", "show"]).map(|t| parse_ip_addr(&t));
        let fields = ["-t", "-f", "ACTIVE,SSID,BSSID,SIGNAL,FREQ,CHAN", "dev", "wifi", "list", "--rescan", "no"];
        (addrs, command_text("nmcli", &fields).and_then(|t| parse_nmcli(&t)))
    }
    #[cfg(windows)]
    {
        let addrs = command_text("ipconfig", &[]).map(|t| parse_ipconfig(&t));
        (addrs, command_text("netsh", &["wlan", "show", "interfaces"]).and_then(|t| parse_netsh(&t)))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        (None, None)
    }
}

/// Time lost between two looks at the clock, `slept_ms` apart. On Linux the monotonic clock stops
/// while the device sleeps, so the wall clock gets ahead of it; on Windows both go on, and the
/// look simply comes late. A wall clock that's set forward looks like the first.
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

fn watch_device() {
    let mut net = NetWatch::default();
    let mut net_due = Instant::now();
    loop {
        if Instant::now() >= net_due || LOOK_NOW.swap(false, Ordering::Relaxed) {
            let (addrs, wifi) = this_network();
            SIGNAL.store(wifi.as_ref().and_then(|w| w.signal).unwrap_or(NO_SIGNAL), Ordering::Relaxed);
            for (word, details) in net.look(addrs, wifi) {
                record(word, details);
            }
            net_due = Instant::now() + NET_EVERY;
        }
        let (wall, mono) = (wall_ms(), Instant::now());
        std::thread::sleep(CLOCK_EVERY);
        let lost = asleep(wall_ms() - wall, mono.elapsed().as_millis() as u64, CLOCK_EVERY.as_millis() as u64);
        if let Some((ms, how)) = lost {
            record("asleep", Details::new().num("ms", ms).num("how", how));
            LOOK_NOW.store(true, Ordering::Relaxed);
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

    fn up_figures() -> Figures {
        Figures { round_trip_ms: Some(12), loss_in: Some(1.5), loss_out: Some(0.0), one_way: false }
    }

    fn look(now_ms: u64, online: bool, online_since: u64, last_alive: u64, quiet: Option<u64>) -> Look {
        let figures = if online { up_figures() } else { Figures::default() };
        Look { now_ms, online, online_since, last_alive, quiet, figures, round_trips: None, packets: (0, 0) }
    }

    #[test]
    fn up_and_down_say_how_long_and_down_keeps_the_figures_from_before() {
        let mut watch = LinkWatch::starting(1000, (0, 0));
        assert_eq!(watch.look(look(1040, false, 0, 0, None)), vec![]);
        let up = watch.look(look(3000, true, 2990, 2990, None));
        assert_eq!(up, vec![LinkEvent::Up { down_ms: 1990, silent_ms: None, figures: up_figures() }]);
        assert_eq!(watch.look(look(5000, true, 2990, 4990, None)), vec![]);
        // The link layer has already forgotten its figures by the look that finds it down.
        let down = watch.look(look(6380, false, 2990, 5000, None));
        assert_eq!(down, vec![LinkEvent::Down { silent_ms: 1380, up_ms: 3390, figures: up_figures() }]);
        let back = watch.look(look(9000, true, 8990, 8990, Some(3990)));
        assert_eq!(back, vec![LinkEvent::Up { down_ms: 2610, silent_ms: Some(3990), figures: up_figures() }]);
    }

    #[test]
    fn a_stall_that_recovers_is_a_gap_of_its_length() {
        let mut watch = LinkWatch::starting(0, (0, 0));
        watch.look(look(100, true, 100, 100, None));
        assert_eq!(watch.look(look(140, true, 100, 140, None)), vec![]);
        assert_eq!(watch.look(look(1100, true, 100, 1090, Some(950))), vec![LinkEvent::Gap { silent_ms: 950 }]);
        assert!(watch.online, "a gap leaves the link up");
    }

    #[test]
    fn a_drop_between_two_looks_is_still_a_down_and_an_up() {
        let mut watch = LinkWatch::starting(0, (0, 0));
        watch.look(look(100, true, 100, 100, None));
        let events = watch.look(look(1700, true, 1690, 1690, Some(1500)));
        assert_eq!(
            events,
            vec![
                LinkEvent::Down { silent_ms: 1500, up_ms: 90, figures: up_figures() },
                LinkEvent::Up { down_ms: 0, silent_ms: Some(1500), figures: up_figures() },
            ]
        );
    }

    #[test]
    fn a_sample_every_ten_seconds_with_the_round_trips_and_packets_in_between() {
        let mut watch = LinkWatch::starting(0, (100, 50));
        let trips = |min_ms, max_ms, sum_ms, count| Some(RoundTrips { min_ms, max_ms, sum_ms, count });
        let mut first = look(5000, true, 10, 5000, None);
        first.round_trips = trips(8.0, 20.0, 28.0, 2);
        watch.look(first);
        let mut second = look(10_000, true, 10, 10_000, None);
        second.round_trips = trips(5.0, 9.0, 14.0, 2);
        second.packets = (600, 350);
        let events = watch.look(second);
        let Some(LinkEvent::Sample { online, round_trips: Some(t), in_per_s, out_per_s, .. }) = events.last().copied() else {
            panic!("{events:?}")
        };
        assert!(online);
        assert_eq!((t.min_ms, t.max_ms, t.count, t.average_ms()), (5.0, 20.0, 4, 10.5));
        assert_eq!((in_per_s, out_per_s), (50.0, 30.0));
        let (word, details) = describe(&events[events.len() - 1], "-");
        assert_eq!(word, "sample");
        assert_eq!(
            details.0,
            "link=up rtt_min_ms=5 rtt_avg_ms=10 rtt_max_ms=20 answers=4 loss_in=1.50 loss_out=0.00 one_way=false in_pps=50.0 out_pps=30.0"
        );
        let mut third = look(20_000, false, 10, 10_000, None);
        third.packets = (600, 350);
        let quiet = watch.look(third);
        assert!(matches!(quiet.last(), Some(LinkEvent::Sample { online: false, round_trips: None, .. })), "{quiet:?}");
    }

    #[test]
    fn sleep_shows_as_lost_time_on_either_clock() {
        assert_eq!(asleep(1003, 1001, 1000), None);
        assert_eq!(asleep(4000, 1000, 1000), None, "a clock set three seconds on");
        assert_eq!(asleep(61_000, 1000, 1000), Some((60_000, "clock")), "Linux: the monotonic clock stopped");
        assert_eq!(asleep(61_000, 61_000, 1000), Some((60_000, "late")), "Windows: both went on, and the look came late");
        assert_eq!(asleep(-50_000, 1000, 1000), None, "a clock set back is no sleep");
    }

    fn set(items: &[&str]) -> Option<BTreeSet<String>> {
        Some(items.iter().map(|s| s.to_string()).collect())
    }

    fn wifi(bssid: &str, signal: u32) -> Option<Wifi> {
        Some(Wifi { ssid: "Home Net".into(), bssid: bssid.into(), signal: Some(signal), channel: "36".into(), freq: "5180 MHz".into() })
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
        assert_eq!(texts(net.look(set(&[]), None)), vec![
            "net added=- removed=tun0=10.8.0.2/24,wlan0=192.168.8.7/24 addrs=-",
            r#"net wifi=none was_ssid="Home Net""#,
        ]);
    }

    #[test]
    fn ip_addr_output_gives_each_address_but_loopback() {
        let text = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n\
                    2: enp3s0    inet 192.168.8.20/24 brd 192.168.8.255 scope global dynamic noprefixroute enp3s0\\       valid_lft 85000sec\n\
                    3: wlp4s0    inet 10.0.0.7/24 brd 10.0.0.255 scope global wlp4s0\\       valid_lft forever\n";
        assert_eq!(parse_ip_addr(text), set(&["enp3s0=192.168.8.20/24", "wlp4s0=10.0.0.7/24"]).unwrap());
        assert!(parse_ip_addr("").is_empty());
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
        let wifi = parse_netsh(text).unwrap();
        assert_eq!((wifi.ssid.as_str(), wifi.bssid.as_str()), ("Home Net", "3c:84:6a:12:34:56"));
        assert_eq!((wifi.signal, wifi.channel.as_str(), wifi.freq.as_str()), (Some(91), "36", "5 GHz"));
        let older = "    Name : Wi-Fi\n    State : connected\n    SSID : Cafe\n    BSSID : 00:11:22:33:44:55\n    Signal : 40%\n";
        assert_eq!(parse_netsh(older).unwrap().bssid, "00:11:22:33:44:55");
        assert_eq!(parse_netsh("    Name : Wi-Fi\n    State : disconnected\n"), None);
    }

    #[test]
    fn ipconfig_output_gives_each_adapters_ipv4_address() {
        let text = "\r\nWindows IP Configuration\r\n\r\n\r\nWireless LAN adapter Wi-Fi:\r\n\r\n   Connection-specific DNS Suffix  . : lan\r\n   \
                    IPv6 Address. . . . . . . . . . . : fd00::1234\r\n   IPv4 Address. . . . . . . . . . . : 192.168.8.210(Preferred) \r\n   \
                    Subnet Mask . . . . . . . . . . . : 255.255.255.0\r\n\r\nEthernet adapter Ethernet 2:\r\n\r\n   Media State . . . . . . . . . . . : Media disconnected\r\n\r\n\
                    Ethernet adapter vEthernet (WSL):\r\n\r\n   IPv4-Adresse  . . . . . . . . . . : 172.20.0.1\r\n";
        assert_eq!(parse_ipconfig(text), set(&["Wi-Fi=192.168.8.210", "vEthernet (WSL)=172.20.0.1"]).unwrap());
    }
}
