//! What the window says about a snapshot, as plain values. No egui types in here, only `Tone`.

use super::model::{self, ControlsEnabled, Device, DeviceStatus, ServiceStart, Snapshot, POOR_LOSS_PERCENT, POOR_ROUND_TRIP_MS};
use super::theme::{Tone, DEVICE_COLUMN, SETTINGS_WIDTH, SPACE_5};

/// The sentence in the status bar, and what the bar offers beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusLine {
    pub square: Tone,
    /// The first words, in `text-dim`.
    pub lead: String,
    pub rest: String,
    /// `rest` is a device name, which is set in semibold.
    pub rest_strong: bool,
    /// The device the Switch button hands control to, while switching works.
    pub switch_to: Option<Device>,
    pub show_release: bool,
}

/// This device has no session with the other one yet, or was never told its address.
fn is_unpaired(local: &DeviceStatus, snapshot: &Snapshot) -> bool {
    model::needs_peer(snapshot) || local.secure == "unpaired"
}

/// The other device is running a version that can't encrypt, and nothing from it is used.
fn is_old_peer(local: &DeviceStatus) -> bool {
    local.old_peer && (local.secure != "encrypted" || !local.peer_online)
}

/// The other device can be heard and can't hear this one.
fn is_one_way(local: &DeviceStatus) -> bool {
    local.link.as_ref().is_some_and(|l| l.one_way && (local.peer_online || l.one_way_held))
}

pub fn status_line(snapshot: &Snapshot, start: &ServiceStart, on: &ControlsEnabled) -> StatusLine {
    let line = |square, lead: &str, rest: String, rest_strong, switch_to| StatusLine {
        square,
        lead: lead.into(),
        rest,
        rest_strong,
        switch_to,
        show_release: snapshot.local.is_some(),
    };
    let Some(local) = &snapshot.local else {
        return if *start == ServiceStart::Starting {
            line(Tone::Dim, "Starting.", "Starting mesh-kvm on this device.".into(), false, None)
        } else {
            line(Tone::Danger, "Not running.", "mesh-kvm isn't running on this device.".into(), false, None)
        };
    };
    let other = snapshot.title(snapshot.in_order()[1]);
    if is_unpaired(local, snapshot) {
        return line(Tone::Warn, "Not paired.", "Everything stays on each device.".into(), false, None);
    }
    if is_old_peer(local) {
        return line(Tone::Warn, "Not linked.", format!("{other} needs an update before it can pair."), false, None);
    }
    if is_one_way(local) {
        return line(Tone::Warn, "One way.", format!("{other} can't hear this device."), false, None);
    }
    if local.secure == "connecting" {
        return line(Tone::Dim, "Connecting to", other, true, None);
    }
    if !snapshot.linked() {
        return line(Tone::Danger, "Link down.", "Each device is using its own keyboard and mouse.".into(), false, None);
    }
    let active = snapshot.active().unwrap_or(snapshot.in_order()[0]);
    let square = if degraded(snapshot) { Tone::Warn } else { Tone::Accent };
    line(square, "Control", snapshot.title(active), true, on.switch.then(|| active.other()))
}

/// The five figures along the top of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    RoundTrip,
    Lost,
    AudioLost,
    AudioBuffer,
    Link,
}

pub const STATS: [Stat; 5] = [Stat::RoundTrip, Stat::Lost, Stat::AudioLost, Stat::AudioBuffer, Stat::Link];

impl Stat {
    pub fn label(self) -> &'static str {
        match self {
            Stat::RoundTrip => "ROUND TRIP",
            Stat::Lost => "LOST",
            Stat::AudioLost => "AUDIO LOST",
            Stat::AudioBuffer => "AUDIO BUFFER",
            Stat::Link => "LINK",
        }
    }

    /// The label as a screen reader says it.
    pub fn spoken(self) -> &'static str {
        match self {
            Stat::RoundTrip => "Round trip",
            Stat::Lost => "Lost",
            Stat::AudioLost => "Audio lost",
            Stat::AudioBuffer => "Audio buffer",
            Stat::Link => "Link",
        }
    }

    /// The least a sparkline's scale tops out at, so a quiet line stays flat instead of
    /// stretching to fill its box.
    pub fn floor(self) -> f32 {
        match self {
            Stat::RoundTrip => 20.0,
            Stat::Lost => 5.0,
            Stat::AudioLost => 5.0,
            Stat::AudioBuffer => 100.0,
            Stat::Link => 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatValue {
    pub text: String,
    pub tone: Tone,
    /// What the sparkline plots, or `None` while the figure isn't known.
    pub sample: Option<f32>,
    pub spoken: String,
}

/// A figure with one decimal, and no `.0`.
pub fn percent(value: f32) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").map_or(text.clone(), str::to_string)
}

pub fn stat_value(stat: Stat, snapshot: &Snapshot) -> StatValue {
    let unknown = || StatValue { text: "--".into(), tone: Tone::Dim, sample: None, spoken: format!("{}, not known", stat.spoken()) };
    let Some(local) = &snapshot.local else { return unknown() };
    if is_unpaired(local, snapshot) {
        return unknown();
    }
    let link = local.link.as_ref().filter(|l| l.measured && local.peer_online);
    let audio = local.audio.rx_live.then_some(&local.audio);
    let known = |text: String, tone, sample: f32, spoken: String| StatValue {
        text,
        tone,
        sample: Some(sample),
        spoken: format!("{}, {spoken}", stat.spoken()),
    };
    match stat {
        Stat::RoundTrip => link.and_then(|l| l.round_trip_ms).map_or_else(unknown, |ms| {
            let tone = if ms >= POOR_ROUND_TRIP_MS { Tone::Warn } else { Tone::Text };
            known(format!("{ms} ms"), tone, ms as f32, format!("{ms} milliseconds"))
        }),
        Stat::Lost => link.and_then(|l| l.loss()).map_or_else(unknown, |loss| {
            let tone = if loss >= POOR_LOSS_PERCENT { Tone::Warn } else { Tone::Text };
            known(format!("{}%", percent(loss)), tone, loss, format!("{} percent", percent(loss)))
        }),
        Stat::AudioLost => audio.map_or_else(unknown, |a| {
            known(format!("{}%", percent(a.rx_loss)), Tone::Text, a.rx_loss, format!("{} percent", percent(a.rx_loss)))
        }),
        Stat::AudioBuffer => audio.map_or_else(unknown, |a| {
            known(format!("{} ms", a.rx_buffer_ms), Tone::Text, a.rx_buffer_ms as f32, format!("{} milliseconds", a.rx_buffer_ms))
        }),
        Stat::Link => {
            let (word, tone) = link_word(snapshot);
            let sample = (!is_old_peer(local)).then(|| if snapshot.linked() { 1.0 } else { 0.0 });
            StatValue { text: word.into(), tone, sample, spoken: format!("{}, {word}", stat.spoken()) }
        }
    }
}

/// The LINK figure's word.
pub fn link_word(snapshot: &Snapshot) -> (&'static str, Tone) {
    let Some(local) = &snapshot.local else { return ("--", Tone::Dim) };
    if is_unpaired(local, snapshot) {
        return ("Not paired", Tone::Warn);
    }
    if is_old_peer(local) {
        return ("Needs update", Tone::Warn);
    }
    if is_one_way(local) {
        return ("One way", Tone::Warn);
    }
    if local.secure == "connecting" {
        return ("Connecting", Tone::Dim);
    }
    match (snapshot.linked(), local.secure == "encrypted") {
        (true, true) => ("Secure", Tone::Text),
        (true, false) => ("Linked", Tone::Text),
        (false, _) => ("Link down", Tone::Danger),
    }
}

/// The link is up and measured at a figure the README's Bad Wi-Fi section calls poor.
pub fn degraded(snapshot: &Snapshot) -> bool {
    let Some(local) = &snapshot.local else { return false };
    let Some(link) = local.link.as_ref().filter(|l| l.measured && local.peer_online) else { return false };
    link.loss().is_some_and(|loss| loss >= POOR_LOSS_PERCENT) || link.round_trip_ms.is_some_and(|ms| ms >= POOR_ROUND_TRIP_MS)
}

/// A line under the update line.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub text: String,
    pub tone: Tone,
    /// It can be dismissed, which only the client's last error can.
    pub dismiss: bool,
}

/// What the window has to say beyond the status line, which already covers a service that isn't
/// running and a pair that isn't paired.
pub fn notices(snapshot: &Snapshot, start: &ServiceStart, last_error: Option<String>, updating: bool) -> Vec<Notice> {
    let mut all = Vec::new();
    let said_above = snapshot.local.as_ref().is_none_or(|local| is_unpaired(local, snapshot));
    // A device dropping out is what an update looks like, and the update line says so.
    if !updating {
        let reason = if matches!(start, ServiceStart::Failed(_)) { model::start_notice(start, snapshot) } else { None };
        let text = reason.or_else(|| if said_above { None } else { model::status_message(snapshot) });
        if let Some(text) = text {
            let tone = if model::notice_is_serious(snapshot, start) { Tone::Danger } else { Tone::Warn };
            all.push(Notice { text, tone, dismiss: false });
        }
    }
    let notes = Device::BOTH.into_iter().filter_map(|d| snapshot.device(d)?.display_change.note.clone());
    all.extend(notes.map(|text| Notice { text, tone: Tone::Warn, dismiss: false }));
    all.extend(last_error.map(|text| Notice { text, tone: Tone::Danger, dismiss: true }));
    all
}

/// How the window divides its width and height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameLayout {
    pub settings_width: f32,
    /// The layout canvas goes below the settings.
    pub stacked: bool,
    /// How many stats go on each of the two rows.
    pub stat_rows: [usize; 2],
    /// Only the body scrolls, with the status bar and stats pinned.
    pub body_scrolls: bool,
}

pub const STACKED_CANVAS_HEIGHT: f32 = 320.0;

pub fn frame_layout(width: f32, height: f32) -> FrameLayout {
    let (settings_width, stacked) = if width >= 1100.0 {
        (SETTINGS_WIDTH, false)
    } else if width >= 900.0 {
        (width * 55.0 / 100.0, false)
    } else {
        (width, true)
    };
    FrameLayout { settings_width, stacked, stat_rows: if stacked { [3, 2] } else { [5, 0] }, body_scrolls: height < 600.0 }
}

/// What a settings row's label has left after the margins and the two device columns.
pub fn label_width(settings_width: f32) -> f32 {
    settings_width - 2.0 * SPACE_5 - 2.0 * DEVICE_COLUMN
}

/// The longest a device name is shown.
pub const MAX_SHOWN_NAME: usize = 24;

/// A name cut to fit, and the whole name for a tooltip when it was cut.
pub fn fit_name(name: &str) -> (String, Option<String>) {
    if name.chars().count() <= MAX_SHOWN_NAME {
        return (name.to_string(), None);
    }
    let cut: String = name.chars().take(MAX_SHOWN_NAME).collect();
    (format!("{cut}…"), Some(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fixtures::*;
    use crate::app::model::LinkStatus;

    #[test]
    fn the_status_line_says_who_has_control_and_offers_the_switch() {
        let s = secure(linked(), "encrypted");
        let line = status_line(&s, &ServiceStart::Done, &model::enabled(&s));
        // The fixture's devices report hostnames, and the line shows the name a device reports.
        assert_eq!((line.square, line.lead.as_str(), line.rest.as_str(), line.rest_strong), (Tone::Accent, "Control", "debian-host", true));
        assert_eq!(line.switch_to, Some(Device::Windows));
        assert!(line.show_release);
    }

    #[test]
    fn unpaired_hides_the_switch_and_keeps_release_all() {
        let s = unpaired();
        let line = status_line(&s, &ServiceStart::Done, &model::enabled(&s));
        assert_eq!((line.square, line.lead.as_str(), line.rest.as_str()), (Tone::Warn, "Not paired.", "Everything stays on each device."));
        assert_eq!((line.switch_to, line.show_release), (None, true));
    }

    #[test]
    fn every_link_state_has_its_square_and_words() {
        let encrypted = || secure(linked(), "encrypted");
        let words = |s: &Snapshot, start: &ServiceStart| {
            let line = status_line(s, start, &model::enabled(s));
            (line.square, line.lead, line.rest, line.switch_to, line.show_release)
        };
        let down = words(&link_down(encrypted()), &ServiceStart::Done);
        assert_eq!(down, (Tone::Danger, "Link down.".into(), "Each device is using its own keyboard and mouse.".into(), None, true));

        let connecting_state = secure(linked(), "connecting");
        let connecting = status_line(&connecting_state, &ServiceStart::Done, &model::enabled(&connecting_state));
        assert_eq!((connecting.square, connecting.lead.as_str(), connecting.rest.as_str(), connecting.rest_strong), (Tone::Dim, "Connecting to", "windows-host", true));
        assert_eq!(connecting.switch_to, None);

        let mut one_way = encrypted();
        one_way.local.as_mut().unwrap().link = Some(LinkStatus { one_way: true, ..Default::default() });
        assert_eq!(words(&one_way, &ServiceStart::Done), (Tone::Warn, "One way.".into(), "windows-host can't hear this device.".into(), None, true));

        let mut old = secure(linked(), "connecting");
        old.local.as_mut().unwrap().old_peer = true;
        assert_eq!(words(&old, &ServiceStart::Done), (Tone::Warn, "Not linked.".into(), "windows-host needs an update before it can pair.".into(), None, true));

        let slow = link(encrypted(), 200, 0.0);
        let line = status_line(&slow, &ServiceStart::Done, &model::enabled(&slow));
        assert_eq!((line.square, line.lead.as_str(), line.switch_to), (Tone::Warn, "Control", Some(Device::Windows)));

        let none = Snapshot::default();
        assert_eq!(
            words(&none, &ServiceStart::Starting),
            (Tone::Dim, "Starting.".into(), "Starting mesh-kvm on this device.".into(), None, false)
        );
        let failed = ServiceStart::Failed("Couldn't start it.".into());
        assert_eq!(
            words(&none, &failed),
            (Tone::Danger, "Not running.".into(), "mesh-kvm isn't running on this device.".into(), None, false)
        );
    }

    #[test]
    fn a_figure_is_dashes_until_it_is_known() {
        for stat in STATS[..4].iter() {
            let v = stat_value(*stat, &unpaired());
            assert_eq!((v.text.as_str(), v.tone, v.sample), ("--", Tone::Dim, None));
        }
        assert_eq!(link_word(&unpaired()), ("Not paired", Tone::Warn));
        assert_eq!(stat_value(Stat::Link, &unpaired()).sample, None);
        // Nothing from before the link went down is shown, and nothing before audio arrives.
        let down = link_down(link(secure(linked(), "encrypted"), 6, 0.4));
        assert_eq!(stat_value(Stat::RoundTrip, &down).text, "--");
        assert_eq!(stat_value(Stat::AudioLost, &linked()).sample, None);
    }

    #[test]
    fn figures_stand_out_at_the_readme_thresholds() {
        let at = |ms, loss| link(secure(linked(), "encrypted"), ms, loss);
        assert_eq!(stat_value(Stat::RoundTrip, &at(6, 0.4)).text, "6 ms");
        assert_eq!(stat_value(Stat::RoundTrip, &at(149, 0.4)).tone, Tone::Text);
        assert_eq!(stat_value(Stat::RoundTrip, &at(150, 0.4)).tone, Tone::Warn);
        assert_eq!(stat_value(Stat::Lost, &at(6, 0.4)).text, "0.4%");
        assert_eq!(stat_value(Stat::Lost, &at(6, 4.9)).tone, Tone::Text);
        assert_eq!(stat_value(Stat::Lost, &at(6, 5.0)).tone, Tone::Warn);
        let audio = audio_live(secure(linked(), "encrypted"), 0.0, 42);
        assert_eq!(stat_value(Stat::AudioLost, &audio).text, "0%");
        assert_eq!(stat_value(Stat::AudioBuffer, &audio).text, "42 ms");
        assert_eq!(stat_value(Stat::RoundTrip, &at(6, 0.4)).spoken, "Round trip, 6 milliseconds");
        assert_eq!(stat_value(Stat::Lost, &at(6, 0.4)).spoken, "Lost, 0.4 percent");
        assert_eq!(stat_value(Stat::AudioBuffer, &audio).spoken, "Audio buffer, 42 milliseconds");
        assert_eq!(stat_value(Stat::RoundTrip, &unpaired()).spoken, "Round trip, not known");
    }

    #[test]
    fn a_percentage_drops_a_trailing_zero_decimal() {
        assert_eq!((percent(0.0).as_str(), percent(12.0).as_str(), percent(0.4).as_str(), percent(4.96).as_str()), ("0", "12", "0.4", "5"));
    }

    #[test]
    fn link_says_secure_connecting_down_or_needs_update() {
        assert_eq!(link_word(&secure(linked(), "encrypted")), ("Secure", Tone::Text));
        assert_eq!(link_word(&linked()), ("Linked", Tone::Text));
        assert_eq!(link_word(&secure(linked(), "connecting")), ("Connecting", Tone::Dim));
        assert_eq!(link_word(&link_down(secure(linked(), "encrypted"))), ("Link down", Tone::Danger));
        let mut old = secure(linked(), "connecting");
        old.local.as_mut().unwrap().old_peer = true;
        assert_eq!(link_word(&old), ("Needs update", Tone::Warn));
        let mut one_way = secure(linked(), "encrypted");
        one_way.local.as_mut().unwrap().link = Some(LinkStatus { one_way: true, ..Default::default() });
        assert_eq!(link_word(&one_way), ("One way", Tone::Warn));
    }

    #[test]
    fn the_window_width_picks_the_layout() {
        let at = |w, h| frame_layout(w, h);
        assert_eq!(at(1280.0, 760.0), FrameLayout { settings_width: 700.0, stacked: false, stat_rows: [5, 0], body_scrolls: false });
        assert_eq!(at(1100.0, 760.0).settings_width, 700.0);
        assert!((at(1099.0, 760.0).settings_width - 604.45).abs() < 0.01);
        assert_eq!(at(900.0, 760.0).settings_width, 495.0);
        let narrow = at(899.0, 760.0);
        assert!(narrow.stacked && narrow.stat_rows == [3, 2] && narrow.settings_width == 899.0);
        assert!(at(1280.0, 599.0).body_scrolls && !at(1280.0, 600.0).body_scrolls);
        assert_eq!(label_width(700.0), 400.0);
    }

    #[test]
    fn names_past_24_characters_are_cut_with_an_ellipsis() {
        assert_eq!(fit_name("Debian"), ("Debian".into(), None));
        let long = "A desktop with a very long name"; // 31 characters
        let (shown, full) = fit_name(long);
        assert_eq!(shown.chars().count(), 25);
        assert!(shown.ends_with('…'));
        assert_eq!(full.as_deref(), Some(long));
        assert_eq!(fit_name(&"x".repeat(24)).1, None);
    }

    #[test]
    fn notices_leave_out_what_the_status_line_already_says() {
        assert!(notices(&unpaired(), &ServiceStart::Done, None, false).is_empty());
        let none = Snapshot::default();
        assert!(notices(&none, &ServiceStart::Starting, None, false).is_empty());
        assert!(notices(&none, &ServiceStart::Done, None, false).is_empty());
        let n = notices(&linked(), &ServiceStart::Done, Some("Laptop didn't accept that change.".into()), false);
        assert_eq!((n.last().unwrap().tone, n.last().unwrap().dismiss), (Tone::Danger, true));
        let failed = notices(&none, &ServiceStart::Failed("Couldn't start it.".into()), None, false);
        assert_eq!(failed, vec![Notice { text: "Couldn't start it.".into(), tone: Tone::Danger, dismiss: false }]);
        // A device dropping out is what an update looks like, so an update in progress hides the link notices.
        let mut alone = linked();
        alone.peer = None;
        assert_eq!(notices(&alone, &ServiceStart::Done, None, false).len(), 1);
        assert!(notices(&alone, &ServiceStart::Done, None, true).is_empty());
    }
}
