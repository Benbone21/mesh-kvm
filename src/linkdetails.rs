//! The connection details: what the link log says, kept in a small structured form and served as
//! JSON at `/api/local/link` for the app's Connection details panel. It holds the latest sample,
//! the last ten minutes of samples in a ring of fixed size, today's drops, the last 20 link events,
//! and how the Wi-Fi, its scans, the packet marks and the control connection stand.
//!
//! Everything comes from the lines the link log writes, as they are written, so that the panel and
//! the log can't disagree; only the Wi-Fi as it is now and the other device's process come from
//! elsewhere. The store is fed on the link log's own thread, and the input loops never take its
//! lock, so building the JSON can't hold one up.
//!
//! A drop's cause is decided once the link is back and the evidence is in: whatever this device
//! logged within 3 s of the drop, and whether the other device's process changed across it.

use crate::linklog::Details;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};

pub const PATH: &str = "/api/local/link";
/// Ten minutes of samples, one every 10 s.
const RING: usize = 60;
const TEN_MIN_MS: i64 = 600_000;
const KEPT_EVENTS: usize = 20;
/// What this device logged this near a drop counts towards its cause.
const NEAR_MS: i64 = 3000;
/// The longest a drop that's back waits for the other device's process to be looked at.
const DECIDE_WAIT_MS: i64 = 5000;
const CLUES_KEPT: usize = 64;
const DAY_MS: i64 = 86_400_000;

static STORE: Mutex<Store> = Mutex::new(Store::new());

fn store() -> std::sync::MutexGuard<'static, Store> {
    STORE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One line of the link log, as it is written. `offset_s` is this device's distance from UTC then.
pub fn take(at_ms: i64, offset_s: i32, word: &str, details: &Details) {
    store().take(at_ms, offset_s, word, details);
}

/// The Wi-Fi as the network thread last read it.
pub fn wifi_now(wifi: WifiNow) {
    store().wifi = Some(wifi);
}

/// The other device answered at `at_ms`, naming its service, or not, as an older one doesn't.
pub fn peer_seen(at_ms: i64, peer: Option<Identity>) {
    store().peer_seen(at_ms, peer);
}

/// How often to ask the other device which process it runs, while that's wanted: often while a
/// drop that's back waits on it, and now and then while it has never answered.
pub fn peer_wanted() -> Option<std::time::Duration> {
    store().peer_wanted()
}

/// The API's answer for `path`, if it is this one.
pub fn answer(path: &str) -> Option<String> {
    if path != PATH && !path.starts_with(&format!("{PATH}?")) {
        return None;
    }
    let now = crate::linklog::wall_ms();
    Some(store().json(now, crate::linklog::utc_offset_s(now)).to_string())
}

/// Which service the other device is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub pid: u64,
    pub version: String,
}

/// The Wi-Fi now.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WifiNow {
    Connected { ssid: String, band: Option<String>, channel: Option<String>, freq_mhz: Option<u32>, signal: Option<u32> },
    None,
    Unknown { error: String },
}

impl WifiNow {
    /// From what nmcli or netsh said: nmcli gives the frequency, `5180 MHz`, and netsh the band,
    /// `5 GHz`, in the same place.
    pub fn connected(ssid: &str, channel: &str, freq: &str, signal: Option<u32>) -> WifiNow {
        let mhz: Option<u32> = freq.split_whitespace().next().and_then(|n| n.parse().ok()).filter(|_| freq.contains("MHz"));
        let band = match mhz {
            Some(mhz) if mhz < 3000 => Some("2.4 GHz".to_string()),
            Some(mhz) if mhz < 5925 => Some("5 GHz".to_string()),
            Some(_) => Some("6 GHz".to_string()),
            None if freq.contains("GHz") => Some(freq.trim().to_string()),
            None => None,
        };
        let channel = Some(channel.trim().to_string()).filter(|c| !c.is_empty());
        WifiNow::Connected { ssid: ssid.to_string(), band, channel, freq_mhz: mhz, signal }
    }
}

/// Why a drop happened, as far as can be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// This device's Wi-Fi changed or went near the drop.
    Wifi,
    /// This device slept near the drop.
    Asleep,
    /// This service or the other one started again.
    Restart,
    /// The control connection timed out too: the other device didn't answer at all.
    Unreachable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct Rtt {
    min_ms: f32,
    avg_ms: f32,
    max_ms: f32,
    jitter_ms: Option<f32>,
    answers: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct Sound {
    loss_percent: Option<f32>,
    buffer_ms: Option<u32>,
    buffer_peak_ms: u32,
    received: u64,
    /// Lost on the way, repaired or not.
    lost: u64,
    repaired: u64,
    late: u64,
}

/// One `sample` line: the last 10 s.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Sample {
    at_ms: i64,
    online: bool,
    rtt: Option<Rtt>,
    loss_in: Option<f32>,
    loss_out: Option<f32>,
    in_per_s: Option<f32>,
    out_per_s: Option<f32>,
    failed_sends: u64,
    failed_last: Option<String>,
    worst_freeze_ms: u64,
    sound: Option<Sound>,
}

fn figure<T: std::str::FromStr>(details: &Details, key: &str) -> Option<T> {
    details.get(key).and_then(|v| v.parse().ok())
}

impl Sample {
    fn of(at_ms: i64, d: &Details) -> Sample {
        let rtt = match (figure(d, "rtt_min_ms"), figure(d, "rtt_avg_ms"), figure(d, "rtt_max_ms")) {
            (Some(min_ms), Some(avg_ms), Some(max_ms)) => {
                Some(Rtt { min_ms, avg_ms, max_ms, jitter_ms: figure(d, "rtt_jitter_ms"), answers: figure(d, "answers").unwrap_or(0) })
            }
            _ => None,
        };
        let sound = figure::<u64>(d, "audio_received").map(|received| {
            let (concealed, repaired) = (figure::<u64>(d, "audio_concealed").unwrap_or(0), figure(d, "audio_repaired").unwrap_or(0));
            Sound {
                loss_percent: figure(d, "audio_loss"),
                buffer_ms: figure(d, "audio_buffer_ms"),
                buffer_peak_ms: figure(d, "audio_buffer_max_ms").unwrap_or(0),
                received,
                lost: concealed + repaired,
                repaired,
                late: figure(d, "audio_late").unwrap_or(0),
            }
        });
        Sample {
            at_ms,
            online: d.get("link") == Some("up"),
            rtt,
            loss_in: figure(d, "loss_in"),
            loss_out: figure(d, "loss_out"),
            in_per_s: figure(d, "in_pps"),
            out_per_s: figure(d, "out_pps"),
            failed_sends: figure(d, "send_errors").unwrap_or(0),
            failed_last: d.get("send_error_last").map(str::to_string),
            worst_freeze_ms: figure(d, "loop_late_ms").unwrap_or(0),
            sound,
        }
    }
}

/// A drop of the link, from when nothing more came to when it came back.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Drop {
    #[serde(skip)]
    id: u64,
    /// When the other device was last heard.
    at_ms: i64,
    /// When the link was said to be down.
    detected_ms: i64,
    back_ms: Option<i64>,
    /// From last heard to back.
    length_ms: Option<u64>,
    cause: Option<Cause>,
    /// For a restart: `this` device or the `other`.
    restarted: Option<&'static str>,
    #[serde(skip)]
    peer_before: Option<Identity>,
    #[serde(skip)]
    peer_after: Option<Identity>,
    /// The other device has answered since the link came back.
    #[serde(skip)]
    peer_checked: bool,
}

/// Something this device logged that can explain a drop, and the span it covers.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Clue {
    Wifi,
    Asleep,
    Restart,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Event {
    #[serde(skip)]
    id: u64,
    at_ms: i64,
    kind: &'static str,
    fields: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<Cause>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Scans {
    /// Only Windows pauses its scans.
    NotHere,
    /// Nothing has been tried yet.
    NotYet,
    Paused,
    /// Streaming mode is on, and the scans couldn't be paused.
    NotPaused,
    Off { reason: String },
    Unavailable { error: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Mark {
    socket: String,
    applied: String,
    dscp: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Control {
    state: String,
    error: Option<String>,
}

struct Store {
    version: String,
    pid: Option<u64>,
    started_ms: Option<i64>,
    online: Option<bool>,
    /// When the link last went up or down.
    changed_ms: Option<i64>,
    ring: [Option<Sample>; RING],
    /// Where the next sample goes in the ring.
    next: usize,
    latest: Option<Sample>,
    /// The day today's drops were counted on, in this device's time, and how many.
    day: i64,
    drops_today: u32,
    events: VecDeque<Event>,
    clues: VecDeque<(i64, i64, Clue)>,
    pending: Option<Drop>,
    last_drop: Option<Drop>,
    wifi: Option<WifiNow>,
    scans: Scans,
    marks: BTreeMap<String, Mark>,
    control: Control,
    peer: Option<Identity>,
    peer_answered: bool,
    next_id: u64,
}

/// A line's pairs as JSON, figures as numbers.
fn fields_of(details: &Details) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value, text) in details.pairs() {
        let figure = || {
            if let Ok(whole) = value.parse::<i64>() {
                return Some(Value::from(whole));
            }
            let number = value.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number);
            number.or_else(|| match value {
                "-" => Some(Value::Null),
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            })
        };
        let as_figure = if text { None } else { figure() };
        map.insert(key.to_string(), as_figure.unwrap_or_else(|| Value::String(value.to_string())));
    }
    map
}

fn local_day(at_ms: i64, offset_s: i32) -> i64 {
    (at_ms + i64::from(offset_s) * 1000).div_euclid(DAY_MS)
}

/// A `control_down` error that says the other end never answered.
fn timed_out(error: &str) -> bool {
    let error = error.to_lowercase();
    ["timed out", "time out", "timeout", "wait ran out", "10060", "did not properly respond"].iter().any(|w| error.contains(w))
}

impl Store {
    const fn new() -> Store {
        Store {
            version: String::new(),
            pid: None,
            started_ms: None,
            online: None,
            changed_ms: None,
            ring: [const { None }; RING],
            next: 0,
            latest: None,
            day: i64::MIN,
            drops_today: 0,
            events: VecDeque::new(),
            clues: VecDeque::new(),
            pending: None,
            last_drop: None,
            wifi: None,
            scans: if cfg!(windows) { Scans::NotYet } else { Scans::NotHere },
            marks: BTreeMap::new(),
            control: Control { state: String::new(), error: None },
            peer: None,
            peer_answered: false,
            next_id: 0,
        }
    }

    fn take(&mut self, at_ms: i64, offset_s: i32, word: &str, d: &Details) {
        match word {
            "sample" => {
                let sample = Sample::of(at_ms, d);
                self.ring[self.next] = Some(sample.clone());
                self.next = (self.next + 1) % RING;
                self.latest = Some(sample);
                if let Some(state) = d.get("control") {
                    if state != self.control.state {
                        self.control = Control { state: state.to_string(), error: None };
                    }
                }
            }
            "down" => self.down(at_ms, offset_s, d),
            "up" => self.up(at_ms, d),
            "gap" | "stall" | "one_way" => {
                self.event(at_ms, word_kind(word), d);
            }
            "net" => {
                let wifi = d.get("ssid").is_some() || d.get("was_ssid").is_some() || d.get("wifi").is_some();
                if wifi && d.get("wifi") != Some("unknown") {
                    self.clue(at_ms, at_ms, Clue::Wifi);
                }
                self.event(at_ms, if wifi { "wifi" } else { "network" }, d);
            }
            "roam" => {
                self.clue(at_ms, at_ms, Clue::Wifi);
                self.event(at_ms, "roam", d);
            }
            "asleep" => {
                let lost: i64 = figure(d, "ms").unwrap_or(0);
                self.clue(at_ms - lost, at_ms, Clue::Asleep);
                self.event(at_ms, "asleep", d);
            }
            "suspend" | "resume" => {
                self.clue(at_ms, at_ms, Clue::Asleep);
                self.event(at_ms, word_kind(word), d);
            }
            "start" => {
                self.version = d.get("version").unwrap_or_default().to_string();
                self.pid = figure(d, "pid");
                self.started_ms = Some(at_ms);
                self.clue(at_ms, at_ms, Clue::Restart);
                self.event(at_ms, "start", d);
            }
            "restarting" | "restart_failed" | "stop" => {
                self.clue(at_ms, at_ms, Clue::Restart);
                self.event(at_ms, word_kind(word), d);
            }
            "control_up" => self.control = Control { state: "up".into(), error: None },
            "control_down" => {
                let error = d.get("error").map(str::to_string);
                if error.as_deref().is_some_and(timed_out) {
                    self.clue(at_ms, at_ms, Clue::Timeout);
                }
                self.control = Control { state: "down".into(), error };
            }
            "wifi_mode" => {
                let get = |key| d.get(key).unwrap_or_default().to_string();
                self.scans = match d.get("streaming") {
                    Some("on") if d.get("bgscan") == Some("off") => Scans::Paused,
                    Some("on") => Scans::NotPaused,
                    Some("off") => Scans::Off { reason: get("reason") },
                    _ => Scans::Unavailable { error: get("error") },
                };
            }
            "qos" => {
                let socket = d.get("socket").unwrap_or("-").to_string();
                let mark = Mark {
                    socket: socket.clone(),
                    applied: d.get("applied").unwrap_or_default().to_string(),
                    dscp: d.get("dscp").map(str::to_string),
                    error: d.get("error").map(str::to_string),
                };
                self.marks.insert(socket, mark);
            }
            _ => {}
        }
        self.decide(at_ms);
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn event(&mut self, at_ms: i64, kind: &'static str, d: &Details) -> u64 {
        let id = self.id();
        if self.events.len() >= KEPT_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(Event { id, at_ms, kind, fields: fields_of(d), cause: None });
        id
    }

    fn clue(&mut self, from_ms: i64, to_ms: i64, clue: Clue) {
        if self.clues.len() >= CLUES_KEPT {
            self.clues.pop_front();
        }
        self.clues.push_back((from_ms, to_ms, clue));
    }

    fn down(&mut self, at_ms: i64, offset_s: i32, d: &Details) {
        // A drop still waiting when the next comes goes with what is known.
        if self.pending.is_some() {
            self.settle();
        }
        let silent_ms: i64 = figure(d, "silent_ms").unwrap_or(0);
        let id = self.event(at_ms, "drop", d);
        let day = local_day(at_ms, offset_s);
        if day != self.day {
            self.day = day;
            self.drops_today = 0;
        }
        self.drops_today += 1;
        self.online = Some(false);
        self.changed_ms = Some(at_ms);
        self.pending = Some(Drop {
            id,
            at_ms: at_ms - silent_ms,
            detected_ms: at_ms,
            back_ms: None,
            length_ms: None,
            cause: None,
            restarted: None,
            peer_before: self.peer.clone(),
            peer_after: None,
            peer_checked: false,
        });
    }

    fn up(&mut self, at_ms: i64, d: &Details) {
        self.online = Some(true);
        self.changed_ms = Some(at_ms);
        let Some(mut drop) = self.pending.take() else {
            self.event(at_ms, "linked", d);
            return;
        };
        let down_ms: i64 = figure(d, "down_ms").unwrap_or(0);
        let length = (drop.detected_ms - drop.at_ms + down_ms).max(0) as u64;
        drop.back_ms = Some(at_ms);
        drop.length_ms = Some(length);
        if let Some(event) = self.events.iter_mut().find(|e| e.id == drop.id) {
            event.fields.insert("length_ms".into(), Value::from(length));
        }
        self.pending = Some(drop);
        let id = self.event(at_ms, "return", d);
        if let Some(event) = self.events.iter_mut().find(|e| e.id == id) {
            event.fields.insert("length_ms".into(), Value::from(length));
        }
    }

    fn peer_seen(&mut self, at_ms: i64, peer: Option<Identity>) {
        self.peer_answered = true;
        if let Some(drop) = self.pending.as_mut() {
            if drop.back_ms.is_some_and(|back| at_ms >= back) && !drop.peer_checked {
                drop.peer_checked = true;
                drop.peer_after = peer.clone();
            }
        }
        if peer.is_some() {
            self.peer = peer;
        }
        self.decide(at_ms);
    }

    fn peer_wanted(&self) -> Option<std::time::Duration> {
        use std::time::Duration;
        if self.pending.as_ref().is_some_and(|p| p.back_ms.is_some() && !p.peer_checked && p.peer_before.is_some()) {
            return Some(Duration::from_secs(1));
        }
        (self.online == Some(true) && !self.peer_answered).then_some(Duration::from_secs(10))
    }

    /// Settles the drop waiting once the link is back, the 3 s after the drop have passed, and the
    /// other device's process has been looked at, or 5 s after it came back without that.
    fn decide(&mut self, now_ms: i64) {
        let Some(drop) = &self.pending else { return };
        let Some(back) = drop.back_ms else { return };
        let window_closed = now_ms >= drop.detected_ms + NEAR_MS && now_ms >= back;
        let peer_known = drop.peer_checked || drop.peer_before.is_none();
        if window_closed && (peer_known || now_ms >= back + DECIDE_WAIT_MS) {
            self.settle();
        }
    }

    fn settle(&mut self) {
        let Some(mut drop) = self.pending.take() else { return };
        let (cause, restarted) = self.cause_of(&drop);
        drop.cause = Some(cause);
        drop.restarted = restarted;
        if let Some(event) = self.events.iter_mut().find(|e| e.id == drop.id) {
            event.cause = Some(cause);
            if let Some(who) = restarted {
                event.fields.insert("restarted".into(), Value::from(who));
            }
        }
        self.last_drop = Some(drop);
    }

    /// Sleep explains a Wi-Fi change that comes with it, and a process that changed is certain, so
    /// these go first.
    fn cause_of(&self, drop: &Drop) -> (Cause, Option<&'static str>) {
        let near = (drop.at_ms - NEAR_MS, drop.detected_ms + NEAR_MS);
        let found = |clue: Clue, (from, to): (i64, i64)| self.clues.iter().any(|(a, b, c)| *c == clue && *a <= to && *b >= from);
        if found(Clue::Asleep, near) {
            return (Cause::Asleep, None);
        }
        if let (Some(before), Some(after)) = (&drop.peer_before, &drop.peer_after) {
            if before != after {
                return (Cause::Restart, Some("other"));
            }
        }
        if found(Clue::Restart, near) {
            return (Cause::Restart, Some("this"));
        }
        if found(Clue::Wifi, near) {
            return (Cause::Wifi, None);
        }
        if found(Clue::Timeout, (near.0, drop.back_ms.unwrap_or(drop.detected_ms))) {
            return (Cause::Unreachable, None);
        }
        (Cause::Unknown, None)
    }

    /// The last ten minutes of samples, oldest first.
    fn ten_minutes(&self, now_ms: i64) -> Vec<&Sample> {
        let mut samples: Vec<&Sample> = self.ring.iter().flatten().filter(|s| s.at_ms > now_ms - TEN_MIN_MS).collect();
        samples.sort_by_key(|s| s.at_ms);
        samples
    }

    fn window(&self, now_ms: i64) -> Value {
        let samples = self.ten_minutes(now_ms);
        let trips: Vec<Rtt> = samples.iter().filter_map(|s| s.rtt).collect();
        let answers: u32 = trips.iter().map(|t| t.answers).sum();
        let rtt = (!trips.is_empty()).then(|| {
            let weight = |t: &Rtt| t.answers.max(1) as f32;
            let total: f32 = trips.iter().map(weight).sum();
            let avg_ms = trips.iter().map(|t| t.avg_ms * weight(t)).sum::<f32>() / total;
            let jittered: Vec<&Rtt> = trips.iter().filter(|t| t.jitter_ms.is_some()).collect();
            let jitter_total: f32 = jittered.iter().map(|t| weight(t)).sum();
            let jitter_ms = (!jittered.is_empty())
                .then(|| jittered.iter().map(|t| t.jitter_ms.unwrap_or(0.0) * weight(t)).sum::<f32>() / jitter_total);
            Rtt {
                min_ms: trips.iter().map(|t| t.min_ms).fold(f32::INFINITY, f32::min),
                avg_ms,
                max_ms: trips.iter().map(|t| t.max_ms).fold(0.0, f32::max),
                jitter_ms,
                answers,
            }
        });
        let sounds: Vec<Sound> = samples.iter().filter_map(|s| s.sound).collect();
        let sound = (!sounds.is_empty()).then(|| {
            json!({
                "buffer_peak_ms": sounds.iter().map(|s| s.buffer_peak_ms).max().unwrap_or(0),
                "received": sounds.iter().map(|s| s.received).sum::<u64>(),
                "lost": sounds.iter().map(|s| s.lost).sum::<u64>(),
                "repaired": sounds.iter().map(|s| s.repaired).sum::<u64>(),
                "late": sounds.iter().map(|s| s.late).sum::<u64>(),
            })
        });
        json!({
            "from_ms": samples.first().map(|s| s.at_ms),
            "samples": samples.len(),
            "rtt": rtt,
            "failed_sends": samples.iter().map(|s| s.failed_sends).sum::<u64>(),
            "worst_freeze_ms": samples.iter().map(|s| s.worst_freeze_ms).max(),
            "sound": sound,
        })
    }

    fn json(&mut self, now_ms: i64, offset_s: i32) -> Value {
        self.decide(now_ms);
        let drops_today = if local_day(now_ms, offset_s) == self.day { self.drops_today } else { 0 };
        let last_drop = self.pending.as_ref().or(self.last_drop.as_ref());
        json!({
            "version": self.version,
            "pid": self.pid,
            "now_ms": now_ms,
            "started_ms": self.started_ms,
            "online": self.online,
            "since_ms": self.changed_ms,
            "latest": self.latest,
            "ten_min": self.window(now_ms),
            "stability": { "drops_today": drops_today, "last_drop": last_drop },
            "wifi": self.wifi,
            "scans": self.scans,
            "marks": self.marks.values().collect::<Vec<_>>(),
            "control": self.control,
            "events": self.events,
        })
    }
}

/// The history's name for a line's word.
fn word_kind(word: &str) -> &'static str {
    match word {
        "gap" => "gap",
        "stall" => "stall",
        "one_way" => "one_way",
        "suspend" => "suspend",
        "resume" => "resume",
        "restarting" => "restarting",
        "restart_failed" => "restart_failed",
        "stop" => "stop",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_762_862_400_000; // 2025-11-11 12:00:00 UTC

    fn d(pairs: &[(&str, &str)]) -> Details {
        pairs.iter().fold(Details::new(), |d, (k, v)| d.num(k, v))
    }

    fn sample(rtt: (&str, &str, &str, &str, &str)) -> Details {
        d(&[
            ("link", "up"),
            ("rtt_min_ms", rtt.0),
            ("rtt_avg_ms", rtt.1),
            ("rtt_max_ms", rtt.2),
            ("rtt_jitter_ms", rtt.3),
            ("answers", rtt.4),
            ("loss_in", "0.20"),
            ("loss_out", "1.50"),
            ("one_way", "false"),
            ("in_pps", "61.0"),
            ("out_pps", "59.5"),
            ("loop_late_ms", "4"),
            ("send_errors", "0"),
            ("control", "up"),
        ])
    }

    fn fed(lines: &[(i64, &str, Details)]) -> Store {
        let mut store = Store::new();
        for (at, word, details) in lines {
            store.take(T + at, 0, word, details);
        }
        store
    }

    fn drop_at(store: &mut Store, now: i64) -> Value {
        store.json(T + now, 0)["stability"]["last_drop"].clone()
    }

    #[test]
    fn the_ring_keeps_ten_minutes_of_samples_and_no_more() {
        let mut store = Store::new();
        for i in 0..(RING as i64 + 15) {
            store.take(T + i * 10_000, 0, "sample", &sample(("3", "5", "9", "1.0", "40")));
        }
        assert_eq!(store.ring.len(), RING);
        assert_eq!(store.ring.iter().flatten().count(), RING);
        let now = T + (RING as i64 + 14) * 10_000;
        assert_eq!(store.ten_minutes(now).len(), RING);
        assert_eq!(store.ten_minutes(now)[0].at_ms, now - (RING as i64 - 1) * 10_000, "oldest first");
    }

    #[test]
    fn ten_minutes_of_round_trips_take_the_lowest_and_worst_and_weigh_the_typical_by_answers() {
        let mut store = fed(&[
            (0, "sample", sample(("3", "5", "9", "1.0", "30"))),
            (10_000, "sample", sample(("2", "8", "120", "4.0", "10"))),
            (20_000, "sample", d(&[("link", "down"), ("rtt_min_ms", "-"), ("answers", "0")])),
        ]);
        let json = store.json(T + 25_000, 0);
        let rtt = &json["ten_min"]["rtt"];
        assert_eq!((rtt["min_ms"].as_f64(), rtt["max_ms"].as_f64(), rtt["answers"].as_u64()), (Some(2.0), Some(120.0), Some(40)));
        assert!((rtt["avg_ms"].as_f64().unwrap() - 5.75).abs() < 0.01, "{rtt}");
        assert!((rtt["jitter_ms"].as_f64().unwrap() - 1.75).abs() < 0.01, "{rtt}");
        assert_eq!(json["ten_min"]["samples"], 3);
        assert_eq!(json["latest"]["rtt"], Value::Null, "the last 10 s had no answers");
        assert_eq!(json["latest"]["online"], false);
        // Ten minutes on, the older ones have gone.
        assert_eq!(store.json(T + 615_000, 0)["ten_min"]["samples"], 1);
    }

    #[test]
    fn a_sample_keeps_packets_sound_freezes_and_failed_sends() {
        let mut details = sample(("3", "5", "9", "1.0", "40"));
        for (k, v) in [
            ("send_errors", "3"),
            ("send_error_last", "no_buffer"),
            ("audio_loss", "0.4"),
            ("audio_buffer_ms", "42"),
            ("audio_received", "1000"),
            ("audio_concealed", "2"),
            ("audio_repaired", "3"),
            ("audio_late", "1"),
            ("audio_buffer_max_ms", "80"),
        ] {
            details = details.num(k, v);
        }
        let mut store = fed(&[(0, "sample", details)]);
        let json = store.json(T + 1000, 0);
        let latest = &json["latest"];
        assert_eq!((latest["in_per_s"].as_f64(), latest["out_per_s"].as_f64()), (Some(61.0), Some(59.5)));
        assert_eq!((latest["loss_in"].as_f64().map(|v| (v * 100.0).round()), latest["loss_out"].as_f64()), (Some(20.0), Some(1.5)));
        assert_eq!((latest["failed_sends"].as_u64(), latest["failed_last"].as_str()), (Some(3), Some("no_buffer")));
        assert_eq!(latest["worst_freeze_ms"], 4);
        let sound = &latest["sound"];
        assert_eq!((sound["buffer_ms"].as_u64(), sound["buffer_peak_ms"].as_u64()), (Some(42), Some(80)));
        assert_eq!((sound["lost"].as_u64(), sound["repaired"].as_u64(), sound["late"].as_u64()), (Some(5), Some(3), Some(1)));
        assert_eq!(json["ten_min"]["sound"]["lost"], 5);
        assert_eq!(json["ten_min"]["failed_sends"], 3);
        assert_eq!(json["control"]["state"], "up");
    }

    #[test]
    fn the_json_has_every_part_the_app_reads() {
        let mut store = fed(&[(0, "start", d(&[("version", "0.9.4"), ("pid", "4242")]))]);
        let json = store.json(T + 5, 0);
        for key in ["version", "pid", "now_ms", "started_ms", "online", "since_ms", "latest", "ten_min", "stability", "wifi", "scans", "marks", "control", "events"] {
            assert!(json.get(key).is_some(), "{key} in {json}");
        }
        assert_eq!((json["version"].as_str(), json["pid"].as_u64(), json["now_ms"].as_i64()), (Some("0.9.4"), Some(4242), Some(T + 5)));
        assert_eq!(json["events"][0]["kind"], "start");
        assert_eq!(json["events"][0]["fields"]["version"], "0.9.4");
        assert_eq!(json["events"][0]["fields"]["pid"], 4242, "figures are numbers");
        assert_eq!(json["stability"]["drops_today"], 0);
        assert_eq!(json["stability"]["last_drop"], Value::Null);
    }

    #[test]
    fn only_the_last_twenty_events_are_kept() {
        let mut store = Store::new();
        for i in 0..30 {
            store.take(T + i, 0, "stall", &d(&[("late_ms", &(300 + i).to_string()), ("loop", "input")]));
        }
        let events = store.json(T + 100, 0)["events"].as_array().unwrap().clone();
        assert_eq!(events.len(), KEPT_EVENTS);
        assert_eq!(events[0]["fields"]["late_ms"], 310);
        assert_eq!(events[19]["kind"], "stall");
    }

    fn down(silent: &str) -> Details {
        d(&[("silent_ms", silent), ("rtt_ms", "6"), ("loss_in", "0.00"), ("loss_out", "0.00"), ("one_way", "false"), ("up_ms", "60000")])
    }

    fn up(down_ms: &str) -> Details {
        d(&[("down_ms", down_ms), ("silent_ms", "-")])
    }

    #[test]
    fn a_drop_is_counted_and_its_length_runs_from_last_heard_to_back() {
        let mut store = fed(&[(10_000, "down", down("1200")), (10_500, "up", up("500"))]);
        let json = store.json(T + 20_000, 0);
        let drop = &json["stability"]["last_drop"];
        assert_eq!((drop["at_ms"].as_i64(), drop["detected_ms"].as_i64()), (Some(T + 8800), Some(T + 10_000)));
        assert_eq!((drop["back_ms"].as_i64(), drop["length_ms"].as_u64()), (Some(T + 10_500), Some(1700)));
        assert_eq!(json["stability"]["drops_today"], 1);
        assert_eq!(json["online"], true);
        let kinds: Vec<&str> = json["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["drop", "return"]);
        assert_eq!(json["events"][0]["fields"]["length_ms"], 1700);
        assert_eq!(json["events"][1]["fields"]["length_ms"], 1700, "the return says how long too");
        assert_eq!(json["events"][0]["cause"], "unknown");
    }

    #[test]
    fn drops_today_start_again_each_day_on_this_devices_clock() {
        // 23:59 and 00:01 at UTC-7.
        let offset = -7 * 3600;
        let evening = T + 18 * 3_600_000 + 59 * 60_000;
        let mut store = Store::new();
        store.take(evening, offset, "down", &down("100"));
        store.take(evening + 1000, offset, "up", &up("0"));
        assert_eq!(store.json(evening + 30_000, offset)["stability"]["drops_today"], 1);
        assert_eq!(store.json(evening + 120_000, offset)["stability"]["drops_today"], 0, "a new day");
        store.take(evening + 130_000, offset, "down", &down("100"));
        assert_eq!(store.json(evening + 140_000, offset)["stability"]["drops_today"], 1);
    }

    #[test]
    fn the_first_link_up_is_no_return_from_a_drop() {
        let mut store = fed(&[(0, "up", up("4000"))]);
        let json = store.json(T + 10_000, 0);
        assert_eq!(json["events"][0]["kind"], "linked");
        assert_eq!(json["stability"]["last_drop"], Value::Null);
    }

    #[test]
    fn a_drop_is_decided_once_the_three_seconds_after_it_have_passed() {
        let mut store = fed(&[(10_000, "down", down("1000")), (10_400, "up", up("400"))]);
        assert_eq!(drop_at(&mut store, 11_000)["cause"], Value::Null, "a Wi-Fi change could still come");
        assert_eq!(drop_at(&mut store, 13_000)["cause"], "unknown");
    }

    #[test]
    fn a_drop_still_down_has_no_cause_or_length() {
        let mut store = fed(&[(10_000, "down", down("1000"))]);
        let drop = drop_at(&mut store, 60_000);
        assert_eq!((drop["cause"].clone(), drop["length_ms"].clone()), (Value::Null, Value::Null));
    }

    #[test]
    fn a_wifi_change_or_roam_near_the_drop_is_the_cause() {
        let net = d(&[("wifi", "none")]).text("was_ssid", "Home");
        let mut store = fed(&[(10_000, "down", down("1000")), (11_500, "net", net), (12_000, "up", up("2000"))]);
        assert_eq!(drop_at(&mut store, 20_000)["cause"], "wifi");
        let roam = d(&[]).text("from", "aa").text("to", "bb").text("ssid", "Home");
        let mut store = fed(&[(5_000, "roam", roam), (10_000, "down", down("2500")), (10_100, "up", up("100"))]);
        assert_eq!(drop_at(&mut store, 20_000)["cause"], "wifi", "within 3 s before it was last heard");
    }

    #[test]
    fn a_wifi_change_further_away_is_not_the_cause() {
        let roam = d(&[]).text("from", "aa").text("to", "bb");
        let mut store = fed(&[(1_000, "roam", roam), (10_000, "down", down("1000")), (10_100, "up", up("100"))]);
        assert_eq!(drop_at(&mut store, 20_000)["cause"], "unknown");
        let unreadable = d(&[("wifi", "unknown")]).text("error", "nmcli failed");
        let mut store = fed(&[(10_000, "down", down("1000")), (10_500, "net", unreadable), (10_600, "up", up("600"))]);
        assert_eq!(drop_at(&mut store, 20_000)["cause"], "unknown", "Wi-Fi that couldn't be read didn't change");
    }

    #[test]
    fn sleep_near_the_drop_is_the_cause_even_with_a_wifi_change() {
        let mut store = fed(&[
            (10_000, "down", down("1000")),
            (10_200, "asleep", d(&[("ms", "60000"), ("how", "clock")])),
            (10_400, "net", d(&[]).text("ssid", "Home")),
            (11_000, "up", up("1000")),
        ]);
        assert_eq!(drop_at(&mut store, 20_000)["cause"], "asleep");
        // Windows says when it goes to sleep and when it wakes.
        let mut store = fed(&[(9_200, "suspend", d(&[])), (10_000, "down", down("1000")), (10_500, "up", up("500"))]);
        assert_eq!(drop_at(&mut store, 20_000)["cause"], "asleep");
        // A long sleep that ended near the drop covers it.
        let mut store = fed(&[(10_000, "down", down("1000")), (14_500, "asleep", d(&[("ms", "3600000")])), (15_000, "up", up("5000"))]);
        assert_eq!(drop_at(&mut store, 20_000)["cause"], "asleep");
    }

    fn identity(pid: u64, version: &str) -> Identity {
        Identity { pid, version: version.into() }
    }

    #[test]
    fn the_other_device_restarting_is_seen_from_its_process_changing() {
        let mut store = fed(&[(0, "sample", sample(("3", "5", "9", "1.0", "40")))]);
        store.peer_seen(T + 1000, Some(identity(100, "0.9.4")));
        store.take(T + 10_000, 0, "down", &down("1000"));
        store.take(T + 14_000, 0, "up", &up("4000"));
        assert_eq!(drop_at(&mut store, 15_000)["cause"], Value::Null, "waiting to hear which process it is");
        store.peer_seen(T + 14_300, Some(identity(200, "0.9.4")));
        let drop = drop_at(&mut store, 14_300);
        assert_eq!((drop["cause"].as_str(), drop["restarted"].as_str()), (Some("restart"), Some("other")));
        assert_eq!(store.json(T + 15_000, 0)["events"][0]["fields"]["restarted"], "other");
    }

    #[test]
    fn the_same_process_after_a_drop_is_no_restart_and_no_answer_is_waited_out() {
        let mut store = Store::new();
        store.peer_seen(T, Some(identity(100, "0.9.4")));
        store.take(T + 10_000, 0, "down", &down("1000"));
        store.take(T + 10_500, 0, "up", &up("500"));
        store.peer_seen(T + 10_700, Some(identity(100, "0.9.4")));
        assert_eq!(drop_at(&mut store, 13_000)["cause"], "unknown");
        let mut store = Store::new();
        store.peer_seen(T, Some(identity(100, "0.9.3")));
        store.take(T + 10_000, 0, "down", &down("1000"));
        store.take(T + 10_500, 0, "up", &up("500"));
        assert!(store.pending.is_some());
        assert_eq!(drop_at(&mut store, 15_400)["cause"], Value::Null);
        assert_eq!(drop_at(&mut store, 15_500)["cause"], "unknown", "5 s after it came back");
    }

    #[test]
    fn the_other_devices_process_is_asked_for_while_a_drop_waits_on_it_or_it_never_said() {
        let mut store = fed(&[(0, "up", up("100"))]);
        assert_eq!(store.peer_wanted(), Some(std::time::Duration::from_secs(10)), "linked, and never asked");
        store.peer_seen(T + 100, None);
        assert_eq!(store.peer_wanted(), None, "an older service that doesn't say is left alone");
        store.peer_seen(T + 200, Some(identity(7, "0.9.4")));
        store.take(T + 10_000, 0, "down", &down("1000"));
        assert_eq!(store.peer_wanted(), None, "nothing to ask while it's down");
        store.take(T + 11_000, 0, "up", &up("1000"));
        assert_eq!(store.peer_wanted(), Some(std::time::Duration::from_secs(1)));
        store.peer_seen(T + 11_200, Some(identity(7, "0.9.4")));
        assert_eq!(store.peer_wanted(), None);
    }

    #[test]
    fn this_service_restarting_near_the_drop_is_the_cause() {
        let mut store = fed(&[
            (10_000, "down", down("1000")),
            (10_100, "restarting", d(&[("reason", "update")])),
            (11_000, "restart_failed", d(&[]).text("error", "no")),
            (11_500, "up", up("1500")),
        ]);
        let drop = drop_at(&mut store, 20_000);
        assert_eq!((drop["cause"].as_str(), drop["restarted"].as_str()), (Some("restart"), Some("this")));
    }

    #[test]
    fn a_control_connection_that_timed_out_during_the_drop_means_the_other_device_was_unreachable() {
        let timeout = d(&[]).text("to", "192.168.8.241:24850").text("error", "connection timed out");
        let mut store = fed(&[(10_000, "down", down("1000")), (25_000, "control_down", timeout), (30_000, "up", up("20000"))]);
        assert_eq!(drop_at(&mut store, 40_000)["cause"], "unreachable");
        assert_eq!(store.json(T + 40_000, 0)["control"], json!({"state": "down", "error": "connection timed out"}));
        let refused = d(&[]).text("error", "connection refused");
        let mut store = fed(&[(10_000, "down", down("1000")), (12_000, "control_down", refused), (13_000, "up", up("3000"))]);
        assert_eq!(drop_at(&mut store, 20_000)["cause"], "unknown", "a refusal is an answer");
    }

    #[test]
    fn a_new_drop_settles_the_one_before() {
        let mut store = fed(&[(10_000, "down", down("1000")), (10_200, "up", up("200")), (11_000, "down", down("500"))]);
        assert_eq!(store.json(T + 11_100, 0)["events"][0]["cause"], "unknown");
        assert_eq!(store.json(T + 11_100, 0)["stability"]["drops_today"], 2);
    }

    #[test]
    fn wifi_scans_and_marks_say_how_they_stand() {
        let mut store = Store::new();
        store.take(T, 0, "wifi_mode", &d(&[("streaming", "on"), ("ifaces", "1"), ("bgscan", "off")]));
        assert_eq!(store.json(T, 0)["scans"], json!({"state": "paused"}));
        store.take(T, 0, "wifi_mode", &d(&[("streaming", "off"), ("reason", "link_down")]));
        assert_eq!(store.json(T, 0)["scans"], json!({"state": "off", "reason": "link_down"}));
        store.take(T, 0, "wifi_mode", &d(&[("streaming", "unavailable")]).text("error", "no connected wireless interface"));
        assert_eq!(store.json(T, 0)["scans"]["state"], "unavailable");
        store.take(T, 0, "qos", &d(&[("socket", "input"), ("dscp", "46"), ("applied", "ip_tos")]));
        store.take(T, 0, "qos", &d(&[("socket", "audio_tx"), ("applied", "none")]).text("error", "denied"));
        store.take(T, 0, "qos", &d(&[("socket", "input"), ("applied", "off"), ("reason", "setting")]));
        let marks = store.json(T, 0)["marks"].clone();
        assert_eq!(marks.as_array().unwrap().len(), 2, "one for each socket, as it is now");
        assert_eq!(marks[0], json!({"socket": "audio_tx", "applied": "none", "dscp": null, "error": "denied"}));
        assert_eq!(marks[1]["applied"], "off");
        store.wifi = Some(WifiNow::connected("Home Net", "36", "5180 MHz", Some(78)));
        assert_eq!(
            store.json(T, 0)["wifi"],
            json!({"state": "connected", "ssid": "Home Net", "band": "5 GHz", "channel": "36", "freq_mhz": 5180, "signal": 78})
        );
    }

    #[test]
    fn the_band_comes_from_the_frequency_or_as_netsh_says_it() {
        let band = |freq: &str| match WifiNow::connected("x", "1", freq, None) {
            WifiNow::Connected { band, .. } => band,
            _ => None,
        };
        assert_eq!(band("2437 MHz").as_deref(), Some("2.4 GHz"));
        assert_eq!(band("5745 MHz").as_deref(), Some("5 GHz"));
        assert_eq!(band("5955 MHz").as_deref(), Some("6 GHz"));
        assert_eq!(band("5 GHz").as_deref(), Some("5 GHz"));
        assert_eq!(band(""), None);
    }

    #[test]
    fn the_path_is_served_and_its_neighbour_the_log_is_not_mistaken_for_it() {
        assert!(answer(PATH).is_some());
        assert!(answer("/api/local/link?x=1").is_some());
        assert!(answer("/api/local/linklog").is_none());
        assert!(answer("/api/local/links").is_none());
        let json: Value = serde_json::from_str(&answer(PATH).unwrap()).unwrap();
        assert!(json["events"].is_array());
    }
}
