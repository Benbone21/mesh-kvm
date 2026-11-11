//! Connection details: each device's `/api/local/link` as the app reads it, and what the panel and
//! its copy say about it, as plain values. No egui types in here, only `Tone`.

use super::model::{POOR_LOSS_PERCENT, POOR_ROUND_TRIP_MS};
use super::theme::Tone;
use super::view::percent;
use serde::Deserialize;
use std::time::Instant;

pub const PATH: &str = crate::linkdetails::PATH;
/// The version that first serves the details, for a device that's older.
pub const FIRST_VERSION: &str = "0.9.4";
/// What a value shows when there is nothing to show.
pub const NONE: &str = "\u{2014}";
/// A drop this recent stands out.
const RECENT_DROP_MS: i64 = 60_000;
/// An answer this old is said to be.
const STALE_AFTER_MS: i64 = 5000;
pub const JITTER_HINT: &str =
    "How far each round trip is from the one before, on average: the mean of the differences between one measurement and the next (the jitter).";

// ---- What the service sends ----

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Rtt {
    pub min_ms: f32,
    pub avg_ms: f32,
    pub max_ms: f32,
    pub jitter_ms: Option<f32>,
    pub answers: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Sound {
    pub loss_percent: Option<f32>,
    pub buffer_ms: Option<u32>,
    pub buffer_peak_ms: u32,
    pub received: u64,
    /// Packets that weren't there in time for their turn: rebuilt from repair data or filled in.
    /// `repaired` and `late` are parts of this.
    pub missed: u64,
    pub repaired: u64,
    pub late: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Sample {
    pub at_ms: i64,
    pub online: bool,
    pub rtt: Option<Rtt>,
    pub loss_in: Option<f32>,
    pub loss_out: Option<f32>,
    pub in_per_s: Option<f32>,
    pub out_per_s: Option<f32>,
    pub failed_sends: u64,
    pub failed_last: Option<String>,
    pub worst_freeze_ms: u64,
    pub sound: Option<Sound>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Window {
    pub samples: usize,
    pub rtt: Option<Rtt>,
    pub failed_sends: u64,
    pub worst_freeze_ms: Option<u64>,
    pub sound: Option<Sound>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Drop {
    pub at_ms: i64,
    pub detected_ms: i64,
    pub back_ms: Option<i64>,
    pub length_ms: Option<u64>,
    pub cause: Option<String>,
    pub restarted: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Stability {
    pub drops_today: u32,
    pub last_drop: Option<Drop>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Wifi {
    pub state: String,
    pub ssid: Option<String>,
    pub band: Option<String>,
    pub channel: Option<String>,
    pub freq_mhz: Option<u32>,
    pub signal: Option<u32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Scans {
    pub state: String,
    pub reason: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Mark {
    pub socket: String,
    pub applied: String,
    pub dscp: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Control {
    pub state: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Event {
    pub at_ms: i64,
    pub kind: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
    pub cause: Option<String>,
}

/// One device's `/api/local/link`. Anything missing reads as not known.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LinkDetails {
    pub version: String,
    pub now_ms: i64,
    pub started_ms: Option<i64>,
    pub online: Option<bool>,
    pub latest: Option<Sample>,
    pub ten_min: Window,
    pub stability: Stability,
    pub wifi: Option<Wifi>,
    pub scans: Option<Scans>,
    pub marks: Vec<Mark>,
    pub control: Control,
    pub events: Vec<Event>,
}

/// Why a device's details can't be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// Its service is older than the details.
    NeedsUpdate,
    /// Nothing answered: this device's service, or the other device through it.
    NotAnswering,
    OutOfReach,
    NotPaired,
    Unreadable,
    /// The service turned the ask away.
    Refused,
}

impl Missing {
    pub fn reason(self) -> String {
        match self {
            Missing::NeedsUpdate => format!("Needs {FIRST_VERSION}"),
            Missing::NotAnswering => "Not answering".into(),
            Missing::OutOfReach => "Out of reach".into(),
            Missing::NotPaired => "Not paired".into(),
            Missing::Unreadable => "Answer couldn't be read".into(),
            Missing::Refused => "Refused".into(),
        }
    }
}

/// The last ask of one device.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Fetched {
    #[default]
    Waiting,
    Got(Box<LinkDetails>, Instant),
    Missing(Missing),
}

/// What an ask for `/api/local/link` came back with, here or through the peer proxy.
pub fn fetched(reply: Result<(u16, String), String>, at: Instant) -> Fetched {
    match reply {
        Ok((200, body)) => match serde_json::from_str::<LinkDetails>(&body) {
            Ok(details) => Fetched::Got(Box::new(details), at),
            Err(_) => Fetched::Missing(Missing::Unreadable),
        },
        Ok((404, _)) => Fetched::Missing(Missing::NeedsUpdate),
        Ok((502, _)) => Fetched::Missing(Missing::OutOfReach),
        Ok((403, _)) => Fetched::Missing(Missing::Refused),
        Ok(_) | Err(_) => Fetched::Missing(Missing::NotAnswering),
    }
}

// ---- What the panel says ----

#[derive(Debug, Clone, PartialEq)]
pub struct DetailCell {
    pub text: String,
    pub tone: Tone,
    pub tooltip: Option<String>,
    /// What the copy says, when it says more than the panel: a relative time with its exact one.
    pub copy: Option<String>,
}

impl DetailCell {
    fn new(text: impl Into<String>, tone: Tone) -> DetailCell {
        DetailCell { text: text.into(), tone, tooltip: None, copy: None }
    }

    /// The exact time of what the text says relatively, for the copy.
    fn at(mut self, exact: &str) -> DetailCell {
        self.copy = Some(format!("{} ({exact})", self.text));
        self
    }

    /// The text as the copy has it.
    pub fn copied(&self) -> &str {
        self.copy.as_deref().unwrap_or(&self.text)
    }

    fn text(text: impl Into<String>) -> DetailCell {
        DetailCell::new(text, Tone::Text)
    }

    fn dim(text: impl Into<String>) -> DetailCell {
        DetailCell::new(text, Tone::Dim)
    }

    fn none() -> DetailCell {
        DetailCell::dim(NONE)
    }

    fn tip(mut self, tooltip: impl Into<String>) -> DetailCell {
        self.tooltip = Some(tooltip.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetailRow {
    pub label: &'static str,
    pub cells: [DetailCell; 2],
    /// What the row means, for its label's tooltip.
    pub hint: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetailGroup {
    pub heading: &'static str,
    pub rows: Vec<DetailRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryLine {
    /// Such as `12:27:49`.
    pub time: String,
    pub text: String,
    pub tone: Tone,
    /// The date and time in full, and what the line was logged with.
    pub tooltip: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetailsView {
    /// Under each device's name: why there are no figures, or how old they are.
    pub status: [Option<DetailCell>; 2],
    pub groups: Vec<DetailGroup>,
    /// Each device's events, newest first.
    pub history: [Vec<HistoryLine>; 2],
}

/// One column: its device's name, the other device's, and what was fetched from it.
pub struct Column<'a> {
    pub name: String,
    pub other: String,
    pub fetched: &'a Fetched,
    /// This device, whose clock the window's times are on.
    pub here: bool,
}

/// A device's details and how far on its clock is now, from its answer and the time since.
struct Seen<'a> {
    d: &'a LinkDetails,
    now_ms: i64,
    name: &'a str,
    other: &'a str,
    here: bool,
    offset_s: i32,
}

fn whole(ms: f32) -> String {
    format!("{}", ms.round() as i64)
}

/// A length of time: `0.4 s`, `12 s`, `3 min 5 s`, `2 h 10 min`.
pub fn duration(ms: i64) -> String {
    let ms = ms.max(0);
    let s = ms / 1000;
    if ms < 10_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        let rest = s % 60;
        if rest == 0 || s >= 600 { format!("{} min", s / 60) } else { format!("{} min {rest} s", s / 60) }
    } else if s < 86_400 {
        let rest = s % 3600 / 60;
        if rest == 0 { format!("{} h", s / 3600) } else { format!("{} h {rest} min", s / 3600) }
    } else {
        format!("{} d", s / 86_400)
    }
}

/// How long ago, roughly: `just now`, `40 s ago`, `3 min ago`, `2 h ago`.
pub fn ago(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..5 => "just now".into(),
        5..60 => format!("{s} s ago"),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

/// `12:27:49` on this device's clock.
pub fn clock(unix_ms: i64, offset_s: i32) -> String {
    let local = unix_ms + i64::from(offset_s) * 1000;
    let in_day = local.rem_euclid(86_400_000);
    format!("{:02}:{:02}:{:02}", in_day / 3_600_000, in_day / 60_000 % 60, in_day / 1000 % 60)
}

/// `2026-11-11 12:27:49`.
pub fn date_time(unix_ms: i64, offset_s: i32) -> String {
    let local = unix_ms + i64::from(offset_s) * 1000;
    let (year, month, day) = crate::linklog::civil_from_days(local.div_euclid(86_400_000));
    format!("{year:04}-{month:02}-{day:02} {}", clock(unix_ms, offset_s))
}

/// What a kind of failed send was.
fn send_kind(kind: &str) -> &'static str {
    match kind {
        "would_block" => "the send would have had to wait",
        "no_buffer" => "no room to send",
        "unreachable" => "no route to the other device",
        "refused" => "refused",
        "denied" => "not allowed",
        "too_big" => "too big to send",
        _ => "another error",
    }
}

fn socket_name(socket: &str) -> &str {
    match socket {
        "input" => "input",
        "audio_tx" => "sound out",
        "audio_rx" => "sound in",
        other => other,
    }
}

/// The round trip as `6 ms, from 4 to 12 ms`, standing out when its worst is poor.
fn round_trip(rtt: Option<&Rtt>, online: bool) -> DetailCell {
    let Some(t) = rtt.filter(|t| t.answers > 0) else {
        return if online { DetailCell::dim("No answers yet") } else { DetailCell::none() };
    };
    let tone = if t.max_ms >= POOR_ROUND_TRIP_MS as f32 { Tone::Warn } else { Tone::Text };
    let (min, avg, max) = (whole(t.min_ms), whole(t.avg_ms), whole(t.max_ms));
    let answers = if t.answers == 1 { "1 answer".to_string() } else { format!("{} answers", t.answers) };
    DetailCell::new(format!("{avg} ms, from {min} to {max} ms"), tone)
        .tip(format!("Lowest {min} ms, typical {avg} ms, worst {max} ms, from {answers}."))
}

fn jitter(rtt: Option<&Rtt>) -> DetailCell {
    match rtt.and_then(|t| t.jitter_ms) {
        Some(j) => DetailCell::text(format!("{j:.1} ms")),
        None => DetailCell::none(),
    }
}

fn loss(value: Option<f32>) -> DetailCell {
    match value {
        Some(v) => DetailCell::new(format!("{}%", percent(v)), if v >= POOR_LOSS_PERCENT { Tone::Warn } else { Tone::Text }),
        None => DetailCell::none(),
    }
}

fn rate(value: Option<f32>) -> DetailCell {
    value.map_or_else(DetailCell::none, |v| DetailCell::text(format!("{} packets/s", whole(v))))
}

impl Seen<'_> {
    fn online(&self) -> bool {
        self.d.online == Some(true)
    }

    fn latest(&self) -> Option<&Sample> {
        self.d.latest.as_ref()
    }

    fn failed_sends(&self) -> DetailCell {
        let recent = self.latest().map_or(0, |s| s.failed_sends);
        let cell = DetailCell::text(format!("{recent} in 10 s, {} in 10 min", self.d.ten_min.failed_sends));
        match self.latest().and_then(|s| s.failed_last.as_deref()) {
            Some(kind) => cell.tip(format!("The last one: {}.", send_kind(kind))),
            None => cell,
        }
    }

    fn cause(&self, drop: &Drop) -> String {
        let (name, other) = (self.name, self.other);
        match (drop.cause.as_deref(), drop.restarted.as_deref()) {
            (Some("wifi"), _) => format!("Wi-Fi changed on {name}"),
            (Some("asleep"), _) => format!("{name} was asleep"),
            (Some("restart"), Some("this")) => format!("{name} restarted"),
            (Some("restart"), _) => format!("{other} restarted"),
            (Some("unreachable"), _) => format!("{other} was out of reach"),
            (Some(_), _) => "cause unknown".into(),
            (None, _) if drop.back_ms.is_none() => "still down".into(),
            (None, _) => "cause not known yet".into(),
        }
    }

    fn since_last_drop(&self) -> DetailCell {
        match &self.d.stability.last_drop {
            Some(drop) if drop.back_ms.is_none() => DetailCell::new("Down now", Tone::Danger),
            Some(drop) => {
                // From when it started, as the last drop's own time is.
                let since = self.now_ms - drop.at_ms;
                DetailCell::new(duration(since), if since < RECENT_DROP_MS { Tone::Warn } else { Tone::Text })
            }
            None => match self.d.started_ms {
                Some(started) => {
                    let exact = date_time(started, self.offset_s);
                    DetailCell::text(format!("None since start, {}", ago(self.now_ms - started))).tip(format!("Started at {exact}.")).at(&exact)
                }
                None => DetailCell::text("None"),
            },
        }
    }

    fn last_drop(&self, offset_s: i32) -> [DetailCell; 2] {
        let Some(drop) = &self.d.stability.last_drop else { return [DetailCell::dim("None"), DetailCell::none()] };
        let when = ago(self.now_ms - drop.at_ms);
        let exact = date_time(drop.at_ms, offset_s);
        let (text, tone) = match drop.length_ms {
            Some(length) => {
                let tone = if self.now_ms - drop.at_ms < RECENT_DROP_MS { Tone::Warn } else { Tone::Text };
                (format!("{when}, for {}", duration(length as i64)), tone)
            }
            None => (format!("{when}, still down"), Tone::Danger),
        };
        let cause = self.cause(drop);
        let cause_tone = if drop.cause.is_some() { Tone::Text } else { Tone::Dim };
        let mut cause_text = cause;
        if let Some(first) = cause_text.get(..1) {
            cause_text = first.to_uppercase() + &cause_text[1..];
        }
        [DetailCell::new(text, tone).tip(format!("Last heard at {exact}.")).at(&exact), DetailCell::new(cause_text, cause_tone)]
    }

    fn wifi(&self) -> [DetailCell; 3] {
        let Some(w) = &self.d.wifi else { return [DetailCell::dim("Not known yet"), DetailCell::none(), DetailCell::none()] };
        match w.state.as_str() {
            "connected" => {
                let ssid = w.ssid.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "Hidden network".into());
                let band = match (&w.band, &w.channel) {
                    (Some(band), Some(channel)) => format!("{band}, channel {channel}"),
                    (Some(band), None) => band.clone(),
                    (None, Some(channel)) => format!("Channel {channel}"),
                    (None, None) => NONE.into(),
                };
                let mut band = DetailCell::text(band);
                if let Some(mhz) = w.freq_mhz {
                    band = band.tip(format!("{mhz} MHz"));
                }
                let signal = w.signal.map_or_else(DetailCell::none, |s| DetailCell::text(format!("{s}%")));
                [DetailCell::text(ssid), band, signal]
            }
            "none" => [DetailCell::dim("Not on Wi-Fi"), DetailCell::none(), DetailCell::none()],
            _ => {
                let cell = DetailCell::dim("Can't tell");
                let cell = match &w.error {
                    Some(error) => cell.tip(error.clone()),
                    None => cell,
                };
                [cell, DetailCell::none(), DetailCell::none()]
            }
        }
    }

    fn scans(&self) -> DetailCell {
        let Some(scans) = &self.d.scans else { return DetailCell::none() };
        match scans.state.as_str() {
            "paused" => DetailCell::text("Yes"),
            "not_paused" => DetailCell::new("No, they couldn't be", Tone::Warn).tip("Streaming mode is on, but Windows kept scanning."),
            "off" => match scans.reason.as_deref() {
                Some("link_down") => DetailCell::dim("No, the link is down"),
                Some("setting") => DetailCell::dim("No, Low-latency Wi-Fi is off"),
                _ => DetailCell::dim("No"),
            },
            "unavailable" => {
                let cell = DetailCell::dim("No, not on this Wi-Fi");
                match &scans.error {
                    Some(error) => cell.tip(error.clone()),
                    None => cell,
                }
            }
            "not_here" => DetailCell::dim("Only done on Windows"),
            _ => DetailCell::dim("Not yet"),
        }
    }

    fn marks(&self) -> DetailCell {
        // Input first, then sound out and in, whatever order they came in.
        let rank = |m: &&Mark| ["input", "audio_tx", "audio_rx"].iter().position(|s| *s == m.socket).unwrap_or(3);
        let mut marks: Vec<&Mark> = self.d.marks.iter().collect();
        marks.sort_by_key(rank);
        if marks.is_empty() {
            return DetailCell::dim("None yet");
        }
        let details: Vec<String> = marks
            .iter()
            .map(|m| {
                let name = socket_name(&m.socket);
                match (m.applied.as_str(), &m.dscp, &m.error) {
                    ("none", _, Some(error)) => format!("{name}: couldn't, {error}"),
                    ("off", ..) => format!("{name}: off"),
                    (how, Some(dscp), _) => format!("{name}: DSCP {dscp}, {how}"),
                    (how, None, _) => format!("{name}: {how}"),
                }
            })
            .collect();
        let tooltip = details.join("\n");
        let on: Vec<&str> = marks.iter().filter(|m| !["none", "off", ""].contains(&m.applied.as_str())).map(|m| socket_name(&m.socket)).collect();
        let failed: Vec<&str> = marks.iter().filter(|m| m.applied == "none").map(|m| socket_name(&m.socket)).collect();
        if !failed.is_empty() {
            return DetailCell::new(format!("Couldn't for {}", failed.join(", ")), Tone::Warn).tip(tooltip);
        }
        if on.is_empty() {
            return DetailCell::dim("Off").tip(tooltip);
        }
        DetailCell::text(format!("On: {}", on.join(", "))).tip(tooltip)
    }

    fn sound(&self) -> [DetailCell; 2] {
        let now = self.latest().and_then(|s| s.sound.as_ref());
        let window = self.d.ten_min.sound.as_ref();
        let Some(window) = window.filter(|_| now.is_some()) else {
            return [DetailCell::dim("No sound coming in"), DetailCell::none()];
        };
        let buffer = match now.and_then(|s| s.buffer_ms) {
            Some(ms) => DetailCell::text(format!("{ms} ms now, {} ms peak", window.buffer_peak_ms.max(ms)))
                .tip("The delay sound waits in the buffer to even out the link. The peak is the most in the last 10 min."),
            None => DetailCell::none(),
        };
        let counts = DetailCell::text(format!("{} ({} repaired, {} late)", window.missed, window.repaired, window.late)).tip(format!(
            "In the last 10 min, of {} packets received, {} weren't there in time to play. {} of those were rebuilt from repair data and the rest filled in, and {} of them came later, after their turn had gone.",
            window.received, window.missed, window.repaired, window.late
        ));
        [buffer, counts]
    }

    fn own(&self) -> [DetailCell; 3] {
        let freeze = match self.d.ten_min.worst_freeze_ms {
            Some(ms) => {
                let tone = if ms >= crate::linklog::STALL_MS { Tone::Warn } else { Tone::Text };
                DetailCell::new(format!("{ms} ms at worst"), tone)
                    .tip("The longest mesh-kvm's input loop came round late in the last 10 min. Over 300 ms counts as a stall.")
            }
            None => DetailCell::none(),
        };
        let control = match self.d.control.state.as_str() {
            "up" => DetailCell::text("Up"),
            "down" => {
                let cell = DetailCell::new("Down", Tone::Warn);
                match &self.d.control.error {
                    Some(error) => cell.tip(error.clone()),
                    None => cell,
                }
            }
            "unused" => DetailCell::dim("Not used yet"),
            _ => DetailCell::none(),
        };
        let version = if self.d.version.is_empty() { DetailCell::none() } else { DetailCell::text(self.d.version.clone()) };
        [freeze, control, version]
    }

    fn history(&self, offset_s: i32) -> Vec<HistoryLine> {
        self.d.events.iter().rev().map(|e| self.history_line(e, offset_s)).collect()
    }

    fn history_line(&self, e: &Event, offset_s: i32) -> HistoryLine {
        let (name, other) = (self.name, self.other);
        let text_field = |key: &str| e.fields.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let ms_field = |key: &str| e.fields.get(key).and_then(|v| v.as_f64()).map(|v| v as i64);
        let (text, tone) = match e.kind.as_str() {
            "drop" => {
                let drop = Drop {
                    back_ms: ms_field("length_ms").map(|_| e.at_ms),
                    length_ms: ms_field("length_ms").map(|v| v as u64),
                    cause: e.cause.clone(),
                    restarted: text_field("restarted"),
                    ..Drop::default()
                };
                let length = drop.length_ms.map_or_else(String::new, |l| format!(" for {}", duration(l as i64)));
                (format!("Link dropped{length} \u{2014} {}", self.cause(&drop)), Tone::Warn)
            }
            "return" => match ms_field("length_ms") {
                Some(length) => (format!("Link back after {}", duration(length)), Tone::Text),
                None => ("Link back".into(), Tone::Text),
            },
            "linked" => (format!("Linked with {other}"), Tone::Text),
            "gap" => {
                let silent = duration(ms_field("silent_ms").unwrap_or(0));
                (format!("Nothing came for {silent}, and the link stayed up"), Tone::Text)
            }
            "stall" => (format!("mesh-kvm froze for {} ms", ms_field("late_ms").unwrap_or(0)), Tone::Warn),
            "one_way" if text_field("state").as_deref() == Some("on") => (format!("{other} stopped hearing {name}"), Tone::Warn),
            "one_way" => (format!("{other} hears {name} again"), Tone::Text),
            "wifi" => match (text_field("ssid"), text_field("was_ssid"), text_field("wifi")) {
                (Some(now), Some(was), _) => (format!("Wi-Fi changed from {was} to {now}"), Tone::Warn),
                (None, Some(was), _) => (format!("Wi-Fi {was} was lost"), Tone::Warn),
                (Some(now), None, _) => (format!("On Wi-Fi {now}"), Tone::Text),
                (None, None, Some(state)) if state == "unknown" => ("Couldn't read the Wi-Fi".into(), Tone::Dim),
                _ => ("Not on Wi-Fi".into(), Tone::Text),
            },
            "network" => ("Network addresses changed".into(), Tone::Text),
            "roam" => {
                let on = text_field("ssid").map_or_else(String::new, |s| format!(" on {s}"));
                (format!("Moved to another access point{on}"), Tone::Text)
            }
            "held_up" => (format!("mesh-kvm was held up on {name} for {}", duration(ms_field("ms").unwrap_or(0))), Tone::Warn),
            "asleep" => (format!("{name} was asleep for {}", duration(ms_field("ms").unwrap_or(0))), Tone::Text),
            "suspend" => (format!("{name} went to sleep"), Tone::Text),
            "resume" => (format!("{name} woke up"), Tone::Text),
            "start" => {
                let version = text_field("version").map_or_else(String::new, |v| format!(" {v}"));
                (format!("mesh-kvm{version} started"), Tone::Text)
            }
            "restarting" if text_field("reason").as_deref() == Some("update") => ("mesh-kvm restarting to update".into(), Tone::Text),
            "restarting" => ("mesh-kvm restarting".into(), Tone::Text),
            "restart_failed" => ("Restarting mesh-kvm failed".into(), Tone::Warn),
            "stop" => ("mesh-kvm stopped".into(), Tone::Text),
            other => (other.replace('_', " "), Tone::Text),
        };
        let logged: Vec<String> = e.fields.iter().map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        }).collect();
        let mut tooltip = format!("{}, {}", date_time(e.at_ms, offset_s), ago(self.now_ms - e.at_ms));
        if !self.here {
            tooltip.push_str(&format!(" on {name}'s own clock"));
        }
        if !logged.is_empty() {
            tooltip.push('\n');
            tooltip.push_str(&logged.join(" "));
        }
        HistoryLine { time: clock(e.at_ms, offset_s), text, tone, tooltip }
    }
}

/// Both columns' figures, group by group. `offset_s` is this device's distance from UTC, for times.
pub fn view(columns: [Column; 2], now: Instant, offset_s: i32) -> DetailsView {
    let seen: [Option<Seen>; 2] = std::array::from_fn(|i| match columns[i].fetched {
        Fetched::Got(d, at) => {
            let elapsed = now.saturating_duration_since(*at).as_millis() as i64;
            Some(Seen { d, now_ms: d.now_ms + elapsed, name: &columns[i].name, other: &columns[i].other, here: columns[i].here, offset_s })
        }
        _ => None,
    });
    let status = std::array::from_fn(|i| match columns[i].fetched {
        Fetched::Waiting => Some(DetailCell::dim("Asking\u{2026}")),
        Fetched::Missing(missing) => {
            let tone = if *missing == Missing::NeedsUpdate { Tone::Warn } else { Tone::Dim };
            Some(DetailCell::new(missing.reason(), tone))
        }
        Fetched::Got(_, at) => {
            let age = now.saturating_duration_since(*at).as_millis() as i64;
            (age >= STALE_AFTER_MS).then(|| DetailCell::new(format!("Last heard {}", ago(age)), Tone::Warn))
        }
    });
    // Each row's cell for both columns, `—` where a column has nothing.
    let both = |f: &dyn Fn(&Seen) -> DetailCell| -> [DetailCell; 2] {
        std::array::from_fn(|i| seen[i].as_ref().map_or_else(DetailCell::none, f))
    };
    let row = |label, cells| DetailRow { label, cells, hint: None };
    let groups = vec![
        DetailGroup {
            heading: "Latency",
            rows: vec![
                row("Round trip, last 10 s", both(&|s| round_trip(s.latest().and_then(|l| l.rtt.as_ref()), s.online()))),
                row("Round trip, last 10 min", both(&|s| round_trip(s.d.ten_min.rtt.as_ref(), s.online()))),
                DetailRow { hint: Some(JITTER_HINT), ..row("Varies by, last 10 s", both(&|s| jitter(s.latest().and_then(|l| l.rtt.as_ref())))) },
                DetailRow { hint: Some(JITTER_HINT), ..row("Varies by, last 10 min", both(&|s| jitter(s.d.ten_min.rtt.as_ref()))) },
            ],
        },
        DetailGroup {
            heading: "Packets",
            rows: vec![
                row("Sent", both(&|s| rate(s.latest().and_then(|l| l.out_per_s)))),
                row("Received", both(&|s| rate(s.latest().and_then(|l| l.in_per_s)))),
                row("Lost on the way in", both(&|s| loss(s.latest().and_then(|l| l.loss_in)))),
                row("Lost on the way out", both(&|s| loss(s.latest().and_then(|l| l.loss_out)))),
                row("Failed sends", both(&|s| s.failed_sends())),
            ],
        },
        DetailGroup {
            heading: "Stability",
            rows: vec![
                row("Since last drop", both(&|s| s.since_last_drop())),
                row("Drops today", both(&|s| DetailCell::text(s.d.stability.drops_today.to_string()))),
                row("Last drop", both(&|s| s.last_drop(offset_s)[0].clone())),
                row("Cause of last drop", both(&|s| s.last_drop(offset_s)[1].clone())),
            ],
        },
        DetailGroup {
            heading: "Wi-Fi",
            rows: vec![
                row("Network", both(&|s| s.wifi()[0].clone())),
                row("Band and channel", both(&|s| s.wifi()[1].clone())),
                row("Signal", both(&|s| s.wifi()[2].clone())),
                row("Scans paused", both(&|s| s.scans())),
                row("Priority marks", both(&|s| s.marks())),
            ],
        },
        DetailGroup {
            heading: "Sound",
            rows: vec![
                row("Buffer", both(&|s| s.sound()[0].clone())),
                row("Missed in time", both(&|s| s.sound()[1].clone())),
            ],
        },
        DetailGroup {
            heading: "mesh-kvm itself",
            rows: vec![
                row("Own freezes", both(&|s| s.own()[0].clone())),
                row("Control connection", both(&|s| s.own()[1].clone())),
                row("Version", both(&|s| s.own()[2].clone())),
            ],
        },
    ];
    let history = std::array::from_fn(|i| seen[i].as_ref().map_or_else(Vec::new, |s| s.history(offset_s)));
    DetailsView { status, groups, history }
}

/// What `Copy details` puts on the clipboard: both columns side by side, then each history.
pub fn copy_text(view: &DetailsView, names: [&str; 2], stamp: &str) -> String {
    let mut out = format!("mesh-kvm connection details, {stamp}\n");
    let label_width = view.groups.iter().flat_map(|g| &g.rows).map(|r| r.label.chars().count()).max().unwrap_or(0);
    let first_width = view
        .groups
        .iter()
        .flat_map(|g| &g.rows)
        .map(|r| r.cells[0].copied().chars().count())
        .chain([names[0].chars().count()])
        .max()
        .unwrap_or(0);
    let line = |label: &str, first: &str, second: &str| {
        let text = format!("{label:<label_width$}  {first:<first_width$}  {second}");
        text.trim_end().to_string() + "\n"
    };
    out.push('\n');
    out.push_str(&line("", names[0], names[1]));
    let status: Vec<String> = view.status.iter().map(|s| s.as_ref().map_or_else(String::new, |c| c.text.clone())).collect();
    if status.iter().any(|s| !s.is_empty()) {
        out.push_str(&line("", &status[0], &status[1]));
    }
    for group in &view.groups {
        out.push('\n');
        out.push_str(&group.heading.to_uppercase());
        out.push('\n');
        for row in &group.rows {
            out.push_str(&line(row.label, row.cells[0].copied(), row.cells[1].copied()));
        }
    }
    for (i, (name, history)) in names.iter().zip(&view.history).enumerate() {
        let clock = if i == 0 { "" } else { ", times on its own clock" };
        out.push_str(&format!("\nHISTORY, {}{clock}\n", name.to_uppercase()));
        if history.is_empty() {
            out.push_str("Nothing yet\n");
        }
        for entry in history {
            out.push_str(&format!("{}  {}\n", entry.time, entry.text));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const T: i64 = 1_762_862_400_000; // 12:00:00 UTC

    fn details(json: serde_json::Value) -> LinkDetails {
        serde_json::from_value(json).unwrap()
    }

    /// What the service sends for a healthy link, as `linkdetails` builds it.
    fn healthy() -> serde_json::Value {
        serde_json::json!({
            "version": "0.9.4", "pid": 10, "now_ms": T, "started_ms": T - 7_200_000, "online": true, "since_ms": T - 7_000_000,
            "latest": {"at_ms": T - 4000, "online": true, "rtt": {"min_ms": 3.0, "avg_ms": 5.4, "max_ms": 12.0, "jitter_ms": 1.24, "answers": 40},
                       "loss_in": 0.2, "loss_out": 0.0, "in_per_s": 61.0, "out_per_s": 59.6, "failed_sends": 0, "failed_last": null,
                       "worst_freeze_ms": 4,
                       "sound": {"loss_percent": 0.4, "buffer_ms": 42, "buffer_peak_ms": 60, "received": 1000, "missed": 2, "repaired": 1, "late": 0}},
            "ten_min": {"from_ms": T - 600_000, "samples": 60, "rtt": {"min_ms": 2.0, "avg_ms": 6.0, "max_ms": 48.0, "jitter_ms": 2.0, "answers": 2400},
                        "failed_sends": 3, "worst_freeze_ms": 12,
                        "sound": {"buffer_peak_ms": 80, "received": 60000, "missed": 5, "repaired": 3, "late": 1}},
            "stability": {"drops_today": 0, "last_drop": null},
            "wifi": {"state": "connected", "ssid": "Home Net", "band": "5 GHz", "channel": "36", "freq_mhz": 5180, "signal": 78},
            "scans": {"state": "paused"},
            "marks": [{"socket": "audio_rx", "applied": "qwave", "dscp": "56", "error": null},
                      {"socket": "input", "applied": "qwave", "dscp": "56", "error": null}],
            "control": {"state": "up", "error": null},
            "events": [{"at_ms": T - 7_200_000, "kind": "start", "fields": {"version": "0.9.4", "pid": 10}},
                       {"at_ms": T - 7_000_000, "kind": "linked", "fields": {"down_ms": 200}}],
        })
    }

    fn got(json: serde_json::Value, at: Instant) -> Fetched {
        Fetched::Got(Box::new(details(json)), at)
    }

    fn columns<'a>(a: &'a Fetched, b: &'a Fetched) -> [Column<'a>; 2] {
        [
            Column { name: "Bread's desk".into(), other: "ThinkPad".into(), fetched: a, here: true },
            Column { name: "ThinkPad".into(), other: "Bread's desk".into(), fetched: b, here: false },
        ]
    }

    fn cell<'a>(view: &'a DetailsView, label: &str, column: usize) -> &'a DetailCell {
        let row = view.groups.iter().flat_map(|g| &g.rows).find(|r| r.label == label).unwrap_or_else(|| panic!("{label}"));
        &row.cells[column]
    }

    #[test]
    fn the_answer_is_read_and_an_older_or_missing_device_says_why() {
        let now = Instant::now();
        assert!(matches!(fetched(Ok((200, healthy().to_string())), now), Fetched::Got(..)));
        assert_eq!(fetched(Ok((404, r#"{"error":"not found"}"#.into())), now), Fetched::Missing(Missing::NeedsUpdate));
        assert_eq!(fetched(Ok((502, "{}".into())), now), Fetched::Missing(Missing::OutOfReach));
        assert_eq!(fetched(Ok((403, "{}".into())), now), Fetched::Missing(Missing::Refused));
        assert_eq!(Missing::Refused.reason(), "Refused");
        assert_eq!(fetched(Err("refused".into()), now), Fetched::Missing(Missing::NotAnswering));
        assert_eq!(fetched(Ok((200, "not json".into())), now), Fetched::Missing(Missing::Unreadable));
        assert_eq!(Missing::NeedsUpdate.reason(), "Needs 0.9.4");
    }

    #[test]
    fn missing_fields_read_as_not_known() {
        let sparse = details(serde_json::json!({"now_ms": T, "future": {"a": 1}, "latest": {"at_ms": T}}));
        assert_eq!(sparse.latest.as_ref().unwrap().rtt, None);
        assert_eq!((sparse.online, sparse.wifi.clone(), sparse.marks.len()), (None, None, 0));
        let a = Fetched::Got(Box::new(sparse), Instant::now());
        let b = Fetched::Missing(Missing::NotAnswering);
        let view = view(columns(&a, &b), Instant::now(), 0);
        assert_eq!(cell(&view, "Round trip, last 10 s", 0).text, NONE);
        assert_eq!(cell(&view, "Network", 0).text, "Not known yet");
        assert_eq!(cell(&view, "Version", 0).text, NONE);
        assert_eq!(cell(&view, "Sent", 0).text, NONE);
    }

    #[test]
    fn a_healthy_link_reads_in_plain_words_with_units() {
        let now = Instant::now();
        let (a, b) = (got(healthy(), now), got(healthy(), now));
        let view = view(columns(&a, &b), now, 0);
        let rtt = cell(&view, "Round trip, last 10 s", 0);
        assert_eq!((rtt.text.as_str(), rtt.tone), ("5 ms, from 3 to 12 ms", Tone::Text));
        assert_eq!(rtt.tooltip.as_deref(), Some("Lowest 3 ms, typical 5 ms, worst 12 ms, from 40 answers."));
        assert_eq!(cell(&view, "Round trip, last 10 min", 1).text, "6 ms, from 2 to 48 ms");
        assert_eq!(cell(&view, "Varies by, last 10 s", 0).text, "1.2 ms");
        assert_eq!(cell(&view, "Varies by, last 10 min", 0).text, "2.0 ms");
        assert_eq!(cell(&view, "Sent", 0).text, "60 packets/s");
        assert_eq!(cell(&view, "Received", 0).text, "61 packets/s");
        assert_eq!(cell(&view, "Lost on the way in", 0).text, "0.2%");
        assert_eq!(cell(&view, "Lost on the way out", 0).text, "0%");
        assert_eq!(cell(&view, "Failed sends", 0).text, "0 in 10 s, 3 in 10 min");
        assert_eq!(cell(&view, "Since last drop", 0).text, "None since start, 2 h ago");
        assert_eq!(cell(&view, "Drops today", 0).text, "0");
        assert_eq!(cell(&view, "Last drop", 0).text, "None");
        assert_eq!(cell(&view, "Network", 0).text, "Home Net");
        assert_eq!(cell(&view, "Band and channel", 0).text, "5 GHz, channel 36");
        assert_eq!(cell(&view, "Band and channel", 0).tooltip.as_deref(), Some("5180 MHz"));
        assert_eq!(cell(&view, "Signal", 0).text, "78%");
        assert_eq!(cell(&view, "Scans paused", 0).text, "Yes");
        assert_eq!(cell(&view, "Priority marks", 0).text, "On: input, sound in");
        assert_eq!(cell(&view, "Priority marks", 0).tooltip.as_deref(), Some("input: DSCP 56, qwave\nsound in: DSCP 56, qwave"));
        assert_eq!(cell(&view, "Buffer", 0).text, "42 ms now, 80 ms peak");
        let missed = cell(&view, "Missed in time", 0);
        assert_eq!(missed.text, "5 (3 repaired, 1 late)", "the repaired and the late are parts of the missed, not added to them");
        assert!(missed.tooltip.as_deref().unwrap().starts_with("In the last 10 min, of 60000 packets received, 5 weren't there in time to play."));
        assert_eq!(cell(&view, "Own freezes", 0).text, "12 ms at worst");
        assert_eq!(cell(&view, "Control connection", 0).text, "Up");
        assert_eq!(cell(&view, "Version", 0).text, "0.9.4");
        assert_eq!(view.status, [None, None]);
        let row = view.groups[0].rows.iter().find(|r| r.label == "Varies by, last 10 s").unwrap();
        assert!(row.hint.unwrap().contains("mean of the differences"), "the jitter says what it is");
    }

    #[test]
    fn poor_figures_stand_out_at_the_windows_thresholds() {
        let now = Instant::now();
        let mut poor = healthy();
        poor["latest"]["rtt"]["max_ms"] = 150.0.into();
        poor["latest"]["loss_out"] = 5.0.into();
        poor["ten_min"]["worst_freeze_ms"] = 300.into();
        poor["control"] = serde_json::json!({"state": "down", "error": "connection timed out"});
        let mut fine = healthy();
        fine["latest"]["rtt"]["max_ms"] = 149.0.into();
        fine["latest"]["loss_out"] = 4.9.into();
        let (a, b) = (got(poor, now), got(fine, now));
        let view = view(columns(&a, &b), now, 0);
        for label in ["Round trip, last 10 s", "Lost on the way out", "Own freezes", "Control connection"] {
            assert_eq!(cell(&view, label, 0).tone, Tone::Warn, "{label}");
        }
        assert_eq!(cell(&view, "Round trip, last 10 s", 1).tone, Tone::Text);
        assert_eq!(cell(&view, "Lost on the way out", 1).tone, Tone::Text);
        assert_eq!(cell(&view, "Control connection", 0).tooltip.as_deref(), Some("connection timed out"));
    }

    fn with_drop(cause: Option<&str>, restarted: Option<&str>, length: Option<u64>, ago_ms: i64) -> serde_json::Value {
        let mut json = healthy();
        let at = T - ago_ms;
        json["stability"] = serde_json::json!({"drops_today": 2, "last_drop": {
            "at_ms": at, "detected_ms": at + 1000, "back_ms": length.map(|l| at + l as i64), "length_ms": length,
            "cause": cause, "restarted": restarted}});
        json["events"] = serde_json::json!([
            {"at_ms": at + 1000, "kind": "drop", "fields": {"silent_ms": 1000, "length_ms": length, "restarted": restarted}, "cause": cause},
            {"at_ms": at + 1200, "kind": "wifi", "fields": {"ssid": "Office", "was_ssid": "Home Net"}},
            {"at_ms": at + 1500, "kind": "return", "fields": {"down_ms": 500, "length_ms": length}},
        ]);
        if length.is_none() {
            json["online"] = false.into();
            json["events"].as_array_mut().unwrap().truncate(2);
        }
        json
    }

    #[test]
    fn a_recent_wifi_drop_stands_out_and_says_why() {
        let now = Instant::now();
        let (a, b) = (got(with_drop(Some("wifi"), None, Some(1500), 30_000), now), got(healthy(), now));
        let view = view(columns(&a, &b), now, 0);
        let since = cell(&view, "Since last drop", 0);
        assert_eq!((since.text.as_str(), since.tone), ("30 s", Tone::Warn));
        assert_eq!(cell(&view, "Drops today", 0).text, "2");
        let last = cell(&view, "Last drop", 0);
        assert_eq!((last.text.as_str(), last.tone), ("30 s ago, for 1.5 s", Tone::Warn));
        assert_eq!(last.tooltip.as_deref(), Some("Last heard at 2025-11-11 11:59:30."));
        assert_eq!(cell(&view, "Cause of last drop", 0).text, "Wi-Fi changed on Bread's desk");
        let history: Vec<(&str, &str)> = view.history[0].iter().map(|h| (h.time.as_str(), h.text.as_str())).collect();
        assert_eq!(
            history[..3],
            [("11:59:31", "Link back after 1.5 s"), ("11:59:31", "Wi-Fi changed from Home Net to Office"), ("11:59:31", "Link dropped for 1.5 s \u{2014} Wi-Fi changed on Bread's desk")]
        );
        assert!(view.history[0][2].tooltip.starts_with("2025-11-11 11:59:31, 29 s ago\n"), "{}", view.history[0][2].tooltip);
        assert!(view.history[1][0].tooltip.contains(" on ThinkPad's own clock"), "the other device's times are its own: {}", view.history[1][0].tooltip);
    }

    #[test]
    fn a_drop_line_reads_as_the_brief_has_it_and_names_who_restarted() {
        let now = Instant::now();
        let (a, b) = (got(with_drop(Some("restart"), Some("other"), Some(1000), 600_000), now), got(with_drop(Some("restart"), Some("this"), Some(1000), 600_000), now));
        let view = view(columns(&a, &b), now, 0);
        let line = &view.history[0][2];
        assert_eq!(format!("{}  {}", line.time, line.text), "11:50:01  Link dropped for 1.0 s \u{2014} ThinkPad restarted");
        assert_eq!(cell(&view, "Cause of last drop", 0).text, "ThinkPad restarted");
        assert_eq!(cell(&view, "Cause of last drop", 1).text, "ThinkPad restarted", "the other device's own restart");
        assert_eq!(cell(&view, "Last drop", 0).tone, Tone::Text, "ten minutes ago no longer stands out");
        for (cause, says) in [("asleep", "Bread's desk was asleep"), ("unreachable", "ThinkPad was out of reach"), ("unknown", "Cause unknown")] {
            let a = got(with_drop(Some(cause), None, Some(1000), 600_000), now);
            assert_eq!(cell(&super::view(columns(&a, &b), now, 0), "Cause of last drop", 0).text, says);
        }
    }

    #[test]
    fn a_drop_still_down_is_danger_and_has_no_cause_yet() {
        let now = Instant::now();
        let (a, b) = (got(with_drop(None, None, None, 12_000), now), Fetched::Waiting);
        let view = view(columns(&a, &b), now, 0);
        assert_eq!((cell(&view, "Since last drop", 0).text.as_str(), cell(&view, "Since last drop", 0).tone), ("Down now", Tone::Danger));
        assert_eq!(cell(&view, "Last drop", 0).text, "12 s ago, still down");
        assert_eq!(cell(&view, "Cause of last drop", 0).text, "Still down");
        assert_eq!(view.history[0][1].text, "Link dropped \u{2014} still down");
        assert_eq!(view.status[1].as_ref().unwrap().text, "Asking\u{2026}");
    }

    #[test]
    fn an_unreachable_or_older_device_shows_dashes_and_a_short_reason() {
        let now = Instant::now();
        let a = got(healthy(), now);
        for (missing, says, tone) in [(Missing::OutOfReach, "Out of reach", Tone::Dim), (Missing::NeedsUpdate, "Needs 0.9.4", Tone::Warn)] {
            let b = Fetched::Missing(missing);
            let view = view(columns(&a, &b), now, 0);
            assert_eq!(view.status[1], Some(DetailCell::new(says, tone)));
            for row in view.groups.iter().flat_map(|g| &g.rows) {
                assert_eq!((row.cells[1].text.as_str(), row.cells[1].tone), (NONE, Tone::Dim), "{}", row.label);
            }
            assert!(view.history[1].is_empty());
        }
    }

    #[test]
    fn an_answer_that_stops_coming_says_how_old_it_is() {
        let at = Instant::now();
        let (a, b) = (got(healthy(), at), got(healthy(), at));
        let later = at + Duration::from_secs(8);
        let view = view(columns(&a, &b), later, 0);
        assert_eq!(view.status[0], Some(DetailCell::new("Last heard 8 s ago", Tone::Warn)));
        // Its own times go on from its clock.
        assert_eq!(cell(&view, "Since last drop", 0).text, "None since start, 2 h ago");
    }

    #[test]
    fn wifi_scans_and_marks_say_how_they_stand_on_each_system() {
        let now = Instant::now();
        let mut linux = healthy();
        linux["scans"] = serde_json::json!({"state": "not_here"});
        linux["marks"] = serde_json::json!([{"socket": "input", "applied": "ip_tos", "dscp": "46"}, {"socket": "audio_tx", "applied": "none", "error": "denied"}]);
        linux["wifi"] = serde_json::json!({"state": "unknown", "error": "nmcli didn't answer within 3 s"});
        let mut off = healthy();
        off["scans"] = serde_json::json!({"state": "off", "reason": "setting"});
        off["marks"] = serde_json::json!([{"socket": "input", "applied": "off"}]);
        off["wifi"] = serde_json::json!({"state": "none"});
        let (a, b) = (got(linux, now), got(off, now));
        let view = view(columns(&a, &b), now, 0);
        assert_eq!(cell(&view, "Scans paused", 0).text, "Only done on Windows");
        assert_eq!(cell(&view, "Scans paused", 1).text, "No, Low-latency Wi-Fi is off");
        let marks = cell(&view, "Priority marks", 0);
        assert_eq!((marks.text.as_str(), marks.tone), ("Couldn't for sound out", Tone::Warn));
        assert_eq!(marks.tooltip.as_deref(), Some("input: DSCP 46, ip_tos\nsound out: couldn't, denied"));
        assert_eq!(cell(&view, "Priority marks", 1).text, "Off");
        assert_eq!(cell(&view, "Network", 0).text, "Can't tell");
        assert_eq!(cell(&view, "Network", 0).tooltip.as_deref(), Some("nmcli didn't answer within 3 s"));
        assert_eq!(cell(&view, "Network", 1).text, "Not on Wi-Fi");
    }

    #[test]
    fn no_sound_coming_in_says_so() {
        let now = Instant::now();
        let mut quiet = healthy();
        quiet["latest"]["sound"] = serde_json::Value::Null;
        let (a, b) = (got(quiet, now), got(healthy(), now));
        let view = view(columns(&a, &b), now, 0);
        assert_eq!(cell(&view, "Buffer", 0).text, "No sound coming in");
        assert_eq!(cell(&view, "Missed in time", 0).text, NONE);
    }

    #[test]
    fn times_and_lengths_read_plainly() {
        assert_eq!((duration(400), duration(1000), duration(12_400), duration(185_000), duration(7_800_000)), ("0.4 s".into(), "1.0 s".into(), "12 s".into(), "3 min 5 s".into(), "2 h 10 min".into()));
        assert_eq!((ago(1000), ago(40_000), ago(180_000), ago(7_300_000)), ("just now".into(), "40 s ago".into(), "3 min ago".into(), "2 h ago".into()));
        assert_eq!(clock(T + 7 * 3_600_000 + 27 * 60_000 + 49_000, -7 * 3600), "12:27:49");
        assert_eq!(date_time(T, 3600), "2025-11-11 13:00:00");
    }

    #[test]
    fn every_kind_of_event_has_a_plain_line() {
        let now = Instant::now();
        let mut json = healthy();
        let kinds = [
            ("gap", serde_json::json!({"silent_ms": 600, "while_busy": true}), "Nothing came for 0.6 s, and the link stayed up"),
            ("stall", serde_json::json!({"late_ms": 420, "loop": "input"}), "mesh-kvm froze for 420 ms"),
            ("one_way", serde_json::json!({"state": "on"}), "ThinkPad stopped hearing Bread's desk"),
            ("roam", serde_json::json!({"ssid": "Home Net", "from": "aa", "to": "bb"}), "Moved to another access point on Home Net"),
            ("asleep", serde_json::json!({"ms": 3_600_000, "how": "clock"}), "Bread's desk was asleep for 1 h"),
            ("held_up", serde_json::json!({"ms": 6200, "how": "late"}), "mesh-kvm was held up on Bread's desk for 6.2 s"),
            ("suspend", serde_json::json!({}), "Bread's desk went to sleep"),
            ("resume", serde_json::json!({"how": "auto"}), "Bread's desk woke up"),
            ("restarting", serde_json::json!({"reason": "update"}), "mesh-kvm restarting to update"),
            ("wifi", serde_json::json!({"wifi": "none", "was_ssid": "Home Net"}), "Wi-Fi Home Net was lost"),
            ("network", serde_json::json!({"added": "eth0=10.0.0.2/24"}), "Network addresses changed"),
            ("start", serde_json::json!({"version": "0.9.4"}), "mesh-kvm 0.9.4 started"),
        ];
        json["events"] = kinds.iter().map(|(kind, fields, _)| serde_json::json!({"at_ms": T, "kind": kind, "fields": fields})).collect();
        let (a, b) = (got(json, now), got(healthy(), now));
        let view = view(columns(&a, &b), now, 0);
        let said: Vec<&str> = view.history[0].iter().rev().map(|h| h.text.as_str()).collect();
        let wanted: Vec<&str> = kinds.iter().map(|(_, _, says)| *says).collect();
        assert_eq!(said, wanted);
        assert_eq!(view.history[1][0].text, "Linked with Bread's desk");
    }

    #[test]
    fn the_copy_has_both_columns_lined_up_and_each_history() {
        let now = Instant::now();
        let (a, b) = (got(with_drop(Some("restart"), Some("other"), Some(1000), 600_000), now), Fetched::Missing(Missing::NeedsUpdate));
        let view = view(columns(&a, &b), now, 0);
        let text = copy_text(&view, ["Bread's desk", "ThinkPad"], "2025-11-11 12:00:00");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "mesh-kvm connection details, 2025-11-11 12:00:00");
        assert!(lines.contains(&"LATENCY"), "{text}");
        let rtt = lines.iter().find(|l| l.starts_with("Round trip, last 10 s")).unwrap();
        let header = lines.iter().find(|l| l.contains("Bread's desk") && l.contains("ThinkPad") && l.starts_with(' ')).unwrap();
        assert_eq!(rtt.find("5 ms"), header.find("Bread's desk"), "the first column lines up:\n{text}");
        assert_eq!(rtt.find(NONE), header.find("ThinkPad"), "and the second:\n{text}");
        assert!(lines.iter().any(|l| l.contains("Needs 0.9.4")), "{text}");
        assert!(text.contains("HISTORY, BREAD'S DESK\n11:50:01  Link back after 1.0 s\n"), "{text}");
        assert!(text.contains("11:50:01  Link dropped for 1.0 s \u{2014} ThinkPad restarted"), "{text}");
        assert!(text.contains("HISTORY, THINKPAD, times on its own clock\nNothing yet\n"), "{text}");
        let last = lines.iter().find(|l| l.starts_with("Last drop ")).unwrap();
        assert!(last.contains("10 min ago, for 1.0 s (2025-11-11 11:50:00)"), "the exact time goes with a relative one: {last}");
        assert!(lines.iter().all(|l| l.len() == l.trim_end().len()), "no trailing spaces");
    }
}
