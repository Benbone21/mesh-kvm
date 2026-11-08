//! The settings table as plain values: each row's label and what each device's cell says and
//! does. No egui types in here, only `Tone`.

use super::model::{self, AudioDevice, Control, ControlsEnabled, Device, DeviceRole, DeviceStatus, Display, Edits, LayoutState, Picks, Placement, Snapshot};
use super::theme::Tone;
use super::view::fit_name;
use crate::audio::BITRATES;
use crate::platform::{DisplayInfo, DisplayMode};
use std::ops::RangeInclusive;
use std::time::Instant;

pub const MIN_POINTER_SPEED: f64 = 0.05;
pub const MAX_POINTER_SPEED: f64 = 2.0;
pub const VOLUME_RANGE: RangeInclusive<f64> = 0.0..=150.0;
pub const CANT_CHANGE: &str = "Its resolution and refresh rate can't be changed from here right now.";
const WAIT_FOR_SCREEN: &str = "Wait for the screen change to finish.";
const DROPDOWN: &str = " \u{25be}";

/// A heading in the settings table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Input,
    Screens,
    Clipboard,
    Audio,
    Games,
    Startup,
    BothDevices,
    Updates,
}

impl Group {
    pub fn heading(self) -> &'static str {
        match self {
            Group::Input => "INPUT",
            Group::Screens => "SCREENS",
            Group::Clipboard => "CLIPBOARD",
            Group::Audio => "AUDIO",
            Group::Games => "GAMES",
            Group::Startup => "STARTUP",
            Group::BothDevices => "BOTH DEVICES",
            Group::Updates => "UPDATES",
        }
    }
}

pub const ORDER: [Group; 8] =
    [Group::Input, Group::Screens, Group::Clipboard, Group::Audio, Group::Games, Group::Startup, Group::BothDevices, Group::Updates];

/// One group's rows, and the captions under them. A group can have no rows, as SCREENS does
/// before there is a layout.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupRows {
    pub group: Group,
    pub rows: Vec<Row>,
    pub notes: Vec<Note>,
}

/// A caption under a group, starting with the device it is about.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub text: String,
    pub tone: Tone,
}

/// One setting, with this device's cell first.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub cells: [Cell; 2],
    /// Why the last change to it was refused, shown under the row.
    pub error: Option<String>,
}

/// What one device's cell shows, and what it does.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub value: String,
    pub tone: Tone,
    pub kind: CellKind,
    /// Why it can't be changed here, which also stops it being changed.
    pub read_only: Option<String>,
    pub tooltip: Option<String>,
    /// The value is one the person set, which the service hasn't confirmed yet.
    pub saving: bool,
    /// The monitor it is about, to light up on the canvas.
    pub monitor: Option<(Device, String)>,
}

impl Cell {
    /// A cell for a setting that doesn't apply to that device, which shows nothing.
    pub fn blank() -> Cell {
        Cell::new(String::new(), Tone::Dim, CellKind::Blank)
    }

    fn new(value: String, tone: Tone, kind: CellKind) -> Cell {
        Cell { value, tone, kind, read_only: None, tooltip: None, saving: false, monitor: None }
    }

    /// The same cell, unchangeable for this reason and shown dimmed.
    fn read_only(self, why: impl Into<String>) -> Cell {
        Cell { read_only: Some(why.into()), tone: Tone::Dim, ..self }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CellKind {
    Blank,
    Plain,
    Toggle(Setting),
    Number { setting: Setting, value: f64, range: RangeInclusive<f64>, step: f64 },
    Dropdown { pick: Pick, options: Vec<Choice> },
    Name,
    /// An audio level meter, in percent.
    Level(u32),
    Action(ActionKind),
}

/// What a toggle or number changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// A setting on the cell's device, sent through `Control`.
    Device(Control),
    EdgePush,
    AutoGameLock,
    ManualGameLock,
}

/// What a dropdown chooses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    Audio(DeviceRole),
    Bitrate,
    /// A mode for the monitor with this id.
    Mode(String),
    Placement,
    UiScale,
}

/// One option in a dropdown.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub label: String,
    pub value: ChoiceValue,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChoiceValue {
    Id(String),
    Kbps(u32),
    Mode(DisplayMode),
    Placement(Placement),
    Scale(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    TestSound,
    MakeMain,
    ChangeAddress,
    Pair,
    Unpair,
    ResetSizes,
    CheckUpdate,
    ShowLicences,
}

/// Everything the rows are worked out from.
pub struct RowsInput<'a> {
    pub snapshot: &'a Snapshot,
    /// The layout the window shows, which may be ahead of the poll.
    pub layout: Option<&'a LayoutState>,
    pub edits: &'a Edits,
    pub picks: &'a Picks,
    pub on: &'a ControlsEnabled,
    pub row_error: &'a dyn Fn((Device, Control)) -> Option<String>,
    pub ui_scale: f32,
    pub main_changing: bool,
    pub main_error: Option<&'a str>,
    pub unpairing: bool,
    pub now: Instant,
}

/// Every group's rows, in `ORDER`.
pub fn groups(input: &RowsInput) -> Vec<GroupRows> {
    let rows = Rows { input, order: input.snapshot.in_order() };
    vec![rows.input_group(), rows.screens(), rows.clipboard(), rows.audio(), rows.games(), rows.startup()]
}

/// The rows of each group, worked out from one input.
struct Rows<'a, 'b> {
    input: &'a RowsInput<'b>,
    order: [Device; 2],
}

impl Rows<'_, '_> {
    fn status(&self, device: Device) -> Option<&DeviceStatus> {
        self.input.snapshot.device(device)
    }

    /// A row with a cell from `cell` for each device that has a status, and blank for one without.
    fn row(&self, label: impl Into<String>, cell: impl Fn(Device, &DeviceStatus) -> Cell) -> Row {
        let cells = self.order.map(|device| self.status(device).map_or_else(Cell::blank, |status| cell(device, status)));
        Row { label: label.into(), cells, error: None }
    }

    /// A row for one `Control`, which carries the reason a change to it was refused.
    fn control_row(&self, label: &str, control: Control, cell: impl Fn(Device, &DeviceStatus) -> Cell) -> Row {
        let errors: Vec<String> = self.order.iter().filter_map(|device| (self.input.row_error)((*device, control))).collect();
        let error = (!errors.is_empty()).then(|| errors.join(" "));
        Row { error, ..self.row(label, cell) }
    }

    /// A control's value as shown: what the person just set, else the poll.
    fn shown(&self, device: Device, status: &DeviceStatus, control: Control) -> (f64, bool) {
        let (key, polled, now) = ((device, control), control.read(status), self.input.now);
        (self.input.edits.shown(key, polled, now), self.input.edits.is_saving(key, polled, now))
    }

    fn toggle(&self, device: Device, status: &DeviceStatus, control: Control) -> Cell {
        let (value, saving) = self.shown(device, status, control);
        Cell { saving, ..on_off(value != 0.0, Setting::Device(control)) }
    }

    fn number(
        &self,
        device: Device,
        status: &DeviceStatus,
        control: Control,
        format: impl Fn(f64) -> String,
        range: RangeInclusive<f64>,
        step: f64,
    ) -> Cell {
        let (value, saving) = self.shown(device, status, control);
        let kind = CellKind::Number { setting: Setting::Device(control), value, range, step };
        Cell { saving, ..Cell::new(format(value), Tone::Text, kind) }
    }

    /// The cell as given, unless the device's service is too old to take the change.
    fn unless_older(&self, device: Device, status: &DeviceStatus, cell: Cell) -> Cell {
        if status.is_older() {
            let title = self.input.snapshot.title(device);
            cell.read_only(format!("{title} is running an older mesh-kvm. Update it to change this."))
        } else {
            cell
        }
    }

    fn note(&self, device: Device, text: &str, tone: Tone) -> Note {
        Note { text: format!("{}: {text}", self.input.snapshot.title(device)), tone }
    }

    fn input_group(&self) -> GroupRows {
        let toggle = |control| move |d, s: &DeviceStatus| self.unless_older(d, s, self.toggle(d, s, control));
        let speed = |d, s: &DeviceStatus| {
            let range = MIN_POINTER_SPEED..=MAX_POINTER_SPEED;
            self.unless_older(d, s, self.number(d, s, Control::PointerSpeed, |v| format!("{v:.2}\u{d7}"), range, 0.05))
        };
        let rows = vec![
            self.control_row("Share mouse", Control::MouseSharing, toggle(Control::MouseSharing)),
            self.control_row("Share keyboard", Control::KeyboardSharing, toggle(Control::KeyboardSharing)),
            self.control_row("Pointer speed on the other device", Control::PointerSpeed, speed),
        ];
        GroupRows { group: Group::Input, rows, notes: Vec::new() }
    }

    /// The resolution and rate of each screen, numbered in the layout's order when either device
    /// has more than one.
    fn screens(&self) -> GroupRows {
        let rects = self.order.map(|device| self.input.layout.map(|l| l.config.rects(device)).unwrap_or_default());
        let count = rects.iter().map(Vec::len).max().unwrap_or(0);
        let mut rows = Vec::new();
        for index in 0..count {
            let (size, rate) = if count == 1 {
                ("Resolution".to_string(), "Refresh rate".to_string())
            } else {
                (format!("Screen {} resolution", index + 1), format!("Screen {} refresh rate", index + 1))
            };
            let [(size_here, rate_here), (size_there, rate_there)] = [0, 1].map(|column| {
                let device = self.order[column];
                match rects[column].get(index).filter(|_| self.status(device).is_some()) {
                    Some(rect) => self.screen(device, rect),
                    None => (Cell::blank(), Cell::blank()),
                }
            });
            rows.push(Row { label: size, cells: [size_here, size_there], error: None });
            rows.push(Row { label: rate, cells: [rate_here, rate_there], error: None });
        }
        GroupRows { group: Group::Screens, rows, notes: Vec::new() }
    }

    /// One screen's resolution and refresh rate cells.
    fn screen(&self, device: Device, rect: &Display) -> (Cell, Cell) {
        let display = model::display_for(self.input.snapshot.displays(device), rect);
        let (name, size, rate) = model::monitor_label(rect, display);
        let about = |cell: Cell| Cell { tooltip: Some(name.clone()), monitor: Some((device, rect.name.clone())), ..cell };
        let menus = display.and_then(|d| Some((d, d.current.as_ref()?))).filter(|(d, _)| d.can_change && !d.modes.is_empty());
        let Some((display, current)) = menus else {
            // The desktop isn't answering about modes, so nothing is offered that couldn't be
            // undone. What's known of the screen still shows.
            let size = Cell::new(compact(&size), Tone::Text, CellKind::Plain).read_only(CANT_CHANGE);
            let rate = Cell::new(rate.unwrap_or_default(), Tone::Text, CellKind::Plain).read_only(CANT_CHANGE);
            return (about(size), about(rate));
        };
        let (size, rate) = mode_menus(display, current);
        // Another change can't start while one is waiting to be kept or undone.
        let waiting = self.input.snapshot.mode_change().is_some();
        let hold = |cell: Cell| if waiting { cell.read_only(WAIT_FOR_SCREEN) } else { cell };
        (about(hold(size)), about(hold(rate)))
    }

    fn clipboard(&self) -> GroupRows {
        let toggle = |control| move |d, s: &DeviceStatus| self.unless_older(d, s, self.toggle(d, s, control));
        let rows = vec![
            self.control_row("Send clipboard", Control::ClipboardSend, toggle(Control::ClipboardSend)),
            self.control_row("Receive clipboard", Control::ClipboardReceive, toggle(Control::ClipboardReceive)),
        ];
        let mut notes = Vec::new();
        for device in self.order {
            let Some(clipboard) = self.status(device).map(|s| &s.clipboard) else { continue };
            if !clipboard.note.is_empty() {
                notes.push(self.note(device, &clipboard.note, if clipboard.note_failed { Tone::Warn } else { Tone::Dim }));
            }
        }
        GroupRows { group: Group::Clipboard, rows, notes }
    }

    fn audio(&self) -> GroupRows {
        // A status without these has none of the audio, and anything shown would be made up.
        let audio_row = |label: &str, control: Option<Control>, cell: &dyn Fn(Device, &DeviceStatus) -> Cell| {
            let cell = |d, s: &DeviceStatus| if s.audio.play_only_on_other.is_some() { cell(d, s) } else { Cell::blank() };
            match control {
                Some(control) => self.control_row(label, control, cell),
                None => self.row(label, cell),
            }
        };
        let volume = |control| move |d, s: &DeviceStatus| self.number(d, s, control, |v| format!("{v:.0}%"), VOLUME_RANGE, 5.0);
        let rows = vec![
            audio_row("Output to send", None, &|d, s| self.audio_device(d, DeviceRole::Output, &s.audio.outputs, &s.audio.output)),
            audio_row("Microphone or line-in", None, &|d, s| self.microphone(d, s)),
            audio_row("Play only on the other device", Some(Control::PlayOnlyOnOther), &|d, s| self.toggle(d, s, Control::PlayOnlyOnOther)),
            audio_row("Send volume", Some(Control::SendVolume), &volume(Control::SendVolume)),
            audio_row("Audio quality", Some(Control::AudioBitrate), &|d, s| self.bitrate(d, s)),
            audio_row("Level sent", None, &|_, s| level(s.audio.tx_peak)),
            audio_row("Play audio from the other device", Some(Control::AudioPlay), &|d, s| self.toggle(d, s, Control::AudioPlay)),
            audio_row("Play received audio on", None, &|d, s| self.audio_device(d, DeviceRole::Play, &s.audio.outputs, &s.audio.play)),
            audio_row("Playback volume", Some(Control::PlayVolume), &volume(Control::PlayVolume)),
            audio_row("Level received", None, &|_, s| level(s.audio.rx_peak)),
            self.row("Test sound", |d, _| self.test_sound(d)),
        ];
        GroupRows { group: Group::Audio, rows, notes: self.audio_notes() }
    }

    /// A dropdown of real devices. A new pick shows at once, ahead of the next status.
    fn audio_device(&self, device: Device, role: DeviceRole, devices: &[AudioDevice], polled: &str) -> Cell {
        let current = self.input.picks.shown((device, role), polled, self.input.now);
        let fixed = role.fixed_options().iter().map(|(id, name)| (*id, *name));
        let options = fixed
            .chain(devices.iter().map(|d| (d.id.as_str(), d.name.as_str())))
            .map(|(id, name)| Choice { label: name.to_string(), value: ChoiceValue::Id(id.to_string()), selected: id == current })
            .collect();
        let (shown, full) = fit_name(&model::device_label(&current, devices));
        let kind = CellKind::Dropdown { pick: Pick::Audio(role), options };
        Cell { tooltip: full, ..Cell::new(format!("{shown}{DROPDOWN}"), Tone::Text, kind) }
    }

    /// A microphone is only ever switched on from its own device, as it could be listened in on.
    fn microphone(&self, device: Device, status: &DeviceStatus) -> Cell {
        let cell = self.audio_device(device, DeviceRole::Input, &status.audio.inputs, &status.audio.input);
        if self.input.snapshot.is_here(device) {
            cell
        } else {
            cell.read_only(format!("Only {} can choose its microphone.", self.input.snapshot.title(device)))
        }
    }

    fn bitrate(&self, device: Device, status: &DeviceStatus) -> Cell {
        let (value, saving) = self.shown(device, status, Control::AudioBitrate);
        let current = value as u32;
        let options = BITRATES
            .iter()
            .map(|&kbps| Choice { label: model::bitrate_label(kbps), value: ChoiceValue::Kbps(kbps), selected: kbps == current })
            .collect();
        let (shown, full) = fit_name(&model::bitrate_label(current));
        let kind = CellKind::Dropdown { pick: Pick::Bitrate, options };
        Cell { saving, tooltip: full, ..Cell::new(format!("{shown}{DROPDOWN}"), Tone::Text, kind) }
    }

    /// Plays a chime on the other device, as heard through this one's audio.
    fn test_sound(&self, device: Device) -> Cell {
        let other = self.input.snapshot.name(device.other());
        let cell = Cell { tooltip: Some(format!("Play a test sound on {other}.")), ..action("Play", Tone::Text, ActionKind::TestSound) };
        if self.input.on.test_sound {
            cell
        } else {
            cell.read_only("A test sound can be played while the devices are linked.")
        }
    }

    /// Each device's audio lines, in the order its panel showed them.
    fn audio_notes(&self) -> Vec<Note> {
        let mut notes = Vec::new();
        for device in self.order {
            let Some(status) = self.status(device) else { continue };
            let audio = &status.audio;
            // These say something isn't working as chosen.
            for text in &audio.notes {
                notes.push(self.note(device, text, Tone::Warn));
            }
            if audio.rx_other_version {
                let text = "Audio from the other device is being ignored. Update both devices to the same version.";
                notes.push(self.note(device, text, Tone::Warn));
            }
            if audio.play_only_on_other.is_none() {
                notes.push(self.note(device, "Update mesh-kvm on it before its audio can be changed here.", Tone::Dim));
                continue;
            }
            if !audio.hint.is_empty() {
                notes.push(self.note(device, &audio.hint, Tone::Dim));
            }
            if audio.tx_enabled {
                notes.push(self.note(device, &model::sending_quality(audio, &self.input.snapshot.name(device.other())), Tone::Dim));
            }
            if let Some(quality) = model::receiving_quality(audio) {
                notes.push(self.note(device, &quality, Tone::Dim));
            }
        }
        notes
    }

    /// Only for the device the app is on: the locks are its own, and don't travel.
    fn games(&self) -> GroupRows {
        let here = self.order[0];
        let config = self.input.layout.map(|l| &l.config).filter(|_| self.status(here).is_some());
        let lock = |label: &str, setting, on: fn(&model::LayoutConfig) -> bool| Row {
            label: label.to_string(),
            cells: [config.map_or_else(Cell::blank, |c| on_off(on(c), setting)), Cell::blank()],
            error: None,
        };
        let rows = vec![
            lock("Hold the mouse while a game runs", Setting::AutoGameLock, |c| c.auto_game_lock),
            lock("Lock the mouse here now", Setting::ManualGameLock, |c| c.manual_game_lock),
        ];
        let telemetry = self.input.snapshot.layout.as_ref().map(|l| &l.telemetry);
        let notes = telemetry.and_then(model::game_lock_hint).map(|hint| self.note(here, &hint, Tone::Dim)).into_iter().collect();
        GroupRows { group: Group::Games, rows, notes }
    }

    fn startup(&self) -> GroupRows {
        let rows = vec![self.control_row("Start when this device boots", Control::Autostart, |d, s| self.toggle(d, s, Control::Autostart))];
        GroupRows { group: Group::Startup, rows, notes: Vec::new() }
    }
}

fn on_off(on: bool, setting: Setting) -> Cell {
    if on {
        Cell::new("ON".into(), Tone::Accent, CellKind::Toggle(setting))
    } else {
        Cell::new("OFF".into(), Tone::Dim, CellKind::Toggle(setting))
    }
}

fn action(value: &str, tone: Tone, kind: ActionKind) -> Cell {
    Cell::new(value.into(), tone, CellKind::Action(kind))
}

fn level(percent: u32) -> Cell {
    let percent = percent.min(100);
    Cell::new(format!("{percent}%"), Tone::Text, CellKind::Level(percent))
}

/// A size as the table shows it: `2560×1440`.
fn compact(size: &str) -> String {
    size.replace(' ', "")
}

/// The resolution and refresh rate dropdowns of a monitor whose modes are known.
fn mode_menus(display: &DisplayInfo, current: &DisplayMode) -> (Cell, Cell) {
    let size = (current.width, current.height);
    let sizes = model::resolutions(display)
        .into_iter()
        .filter_map(|option| {
            let mode = model::mode_for_size(display, option)?;
            Some(Choice { label: compact(&model::size_label(option)), value: ChoiceValue::Mode(mode.clone()), selected: option == size })
        })
        .collect();
    let pick = Pick::Mode(display.id.clone());
    let rates = model::rates(display, size);
    let rate_choices = rates
        .iter()
        .map(|mode| Choice { label: model::rate_label(mode, &rates), value: ChoiceValue::Mode((*mode).clone()), selected: mode.same_as(current) })
        .collect();
    let size_text = format!("{}{DROPDOWN}", compact(&model::size_label(size)));
    let rate_text = format!("{}{DROPDOWN}", model::rate_label(current, &rates));
    (
        Cell::new(size_text, Tone::Text, CellKind::Dropdown { pick: pick.clone(), options: sizes }),
        Cell::new(rate_text, Tone::Text, CellKind::Dropdown { pick, options: rate_choices }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fixtures::*;
    use crate::app::model::{Device::*, Sharing};

    fn input(snapshot: &Snapshot) -> RowsInput<'_> {
        RowsInput {
            snapshot,
            layout: snapshot.layout.as_ref(),
            edits: Box::leak(Box::default()),
            picks: Box::leak(Box::default()),
            on: Box::leak(Box::new(model::enabled(snapshot))),
            row_error: &|_| None,
            ui_scale: 1.0,
            main_changing: false,
            main_error: None,
            unpairing: false,
            now: Instant::now(),
        }
    }

    fn labels(groups: &[GroupRows], group: Group) -> Vec<String> {
        groups.iter().find(|g| g.group == group).map(|g| g.rows.iter().map(|r| r.label.clone()).collect()).unwrap_or_default()
    }

    fn row<'a>(groups: &'a [GroupRows], label: &str) -> &'a Row {
        groups.iter().flat_map(|g| &g.rows).find(|r| r.label == label).unwrap_or_else(|| panic!("no row {label}"))
    }

    fn notes(groups: &[GroupRows], group: Group) -> Vec<Note> {
        groups.iter().find(|g| g.group == group).map(|g| g.notes.clone()).unwrap_or_default()
    }

    fn values(row: &Row) -> [&str; 2] {
        [row.cells[0].value.as_str(), row.cells[1].value.as_str()]
    }

    #[test]
    fn every_device_setting_has_a_row_in_the_boards_order() {
        let g = groups(&input(&linked()));
        assert_eq!(g.iter().map(|g| g.group).collect::<Vec<_>>(), ORDER[..6]);
        assert_eq!(labels(&g, Group::Input), ["Share mouse", "Share keyboard", "Pointer speed on the other device"]);
        assert_eq!(labels(&g, Group::Clipboard), ["Send clipboard", "Receive clipboard"]);
        assert_eq!(labels(&g, Group::Audio), [
            "Output to send", "Microphone or line-in", "Play only on the other device", "Send volume", "Audio quality",
            "Level sent", "Play audio from the other device", "Play received audio on", "Playback volume",
            "Level received", "Test sound",
        ]);
        assert_eq!(labels(&g, Group::Games), ["Hold the mouse while a game runs", "Lock the mouse here now"]);
        assert_eq!(labels(&g, Group::Startup), ["Start when this device boots"]);
    }

    #[test]
    fn this_device_comes_first() {
        let mut s = linked();
        s.local.as_mut().unwrap().pointer_speed = 0.5;
        assert_eq!(values(row(&groups(&input(&s)), "Pointer speed on the other device")), ["0.50\u{d7}", "1.00\u{d7}"]);
        let swapped = Snapshot { local: s.peer.clone(), peer: s.local.clone(), ..s };
        assert_eq!(values(row(&groups(&input(&swapped)), "Pointer speed on the other device")), ["1.00\u{d7}", "0.50\u{d7}"]);
    }

    #[test]
    fn toggles_read_on_and_off_in_their_colours() {
        let mut s = linked();
        s.peer.as_mut().unwrap().sharing = Some(Sharing { mouse: true, keyboard: false });
        let g = groups(&input(&s));
        let keyboard = row(&g, "Share keyboard");
        assert_eq!((keyboard.cells[0].value.as_str(), keyboard.cells[0].tone), ("ON", Tone::Accent));
        assert_eq!((keyboard.cells[1].value.as_str(), keyboard.cells[1].tone), ("OFF", Tone::Dim));
        assert_eq!(keyboard.cells[1].kind, CellKind::Toggle(Setting::Device(Control::KeyboardSharing)));
    }

    #[test]
    fn numbers_and_dropdowns_read_as_on_the_board() {
        let mut s = linked();
        let local = s.local.as_mut().unwrap();
        local.pointer_speed = 0.35;
        local.audio.bitrate = 128;
        let g = groups(&input(&s));
        let speed = row(&g, "Pointer speed on the other device");
        assert_eq!(speed.cells[0].value, "0.35\u{d7}");
        let CellKind::Number { setting, range, step, .. } = &speed.cells[0].kind else { panic!("not a number") };
        assert_eq!((*setting, range.clone(), *step), (Setting::Device(Control::PointerSpeed), MIN_POINTER_SPEED..=MAX_POINTER_SPEED, 0.05));
        assert_eq!(row(&g, "Send volume").cells[0].value, "100%");
        let quality = row(&g, "Audio quality");
        assert_eq!(quality.cells[0].value, "128 kbit/s \u{25be}");
        let CellKind::Dropdown { pick, options } = &quality.cells[0].kind else { panic!("not a dropdown") };
        assert_eq!(*pick, Pick::Bitrate);
        assert_eq!(options.iter().map(|o| o.value.clone()).collect::<Vec<_>>(), BITRATES.map(ChoiceValue::Kbps));
        assert_eq!(options.iter().filter(|o| o.selected).map(|o| o.label.as_str()).collect::<Vec<_>>(), ["128 kbit/s"]);
        assert_eq!(row(&g, "Output to send").cells[0].value, "Speakers \u{25be}");
    }

    #[test]
    fn an_audio_dropdown_offers_the_fixed_choices_then_the_devices() {
        let g = groups(&input(&linked()));
        let CellKind::Dropdown { pick, options } = &row(&g, "Play received audio on").cells[0].kind else { panic!("not a dropdown") };
        assert_eq!(*pick, Pick::Audio(DeviceRole::Play));
        let labels: Vec<_> = options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["System default", "Speakers"]);
        assert_eq!(options[1].value, ChoiceValue::Id("speakers".into()));
    }

    #[test]
    fn a_long_audio_device_name_is_cut_with_the_whole_name_in_a_tooltip() {
        let mut s = linked();
        let name = "Headphones (High Definition Audio Device, rear panel)";
        s.local.as_mut().unwrap().audio.outputs[0].name = name.into();
        let g = groups(&input(&s));
        let cell = &row(&g, "Output to send").cells[0];
        assert_eq!(cell.value, format!("{} \u{25be}", fit_name(name).0));
        assert_eq!(cell.tooltip.as_deref(), Some(name));
    }

    #[test]
    fn the_other_devices_microphone_is_read_only_and_says_why() {
        let g = groups(&input(&linked()));
        let mic = row(&g, "Microphone or line-in");
        assert_eq!(mic.cells[0].read_only, None);
        assert_eq!(mic.cells[1].read_only.as_deref(), Some("Only windows-host can choose its microphone."));
        assert_eq!(mic.cells[1].tone, Tone::Dim);
    }

    #[test]
    fn a_device_that_is_not_connected_has_a_blank_column() {
        for group in groups(&input(&unpaired())).iter().filter(|g| g.group != Group::BothDevices && g.group != Group::Updates) {
            for r in &group.rows {
                assert!(matches!(r.cells[1].kind, CellKind::Blank), "{}", r.label);
            }
        }
    }

    #[test]
    fn game_locks_apply_only_on_this_device() {
        let g = groups(&input(&linked()));
        let hold = row(&g, "Hold the mouse while a game runs");
        assert_eq!((hold.cells[0].value.as_str(), &hold.cells[0].kind), ("ON", &CellKind::Toggle(Setting::AutoGameLock)));
        let lock = row(&g, "Lock the mouse here now");
        assert_eq!((lock.cells[0].value.as_str(), &lock.cells[0].kind), ("OFF", &CellKind::Toggle(Setting::ManualGameLock)));
        assert_eq!(hold.cells[1], Cell::blank());
        assert_eq!(lock.cells[1], Cell::blank());
    }

    #[test]
    fn an_older_service_cannot_be_changed_and_says_why() {
        let mut s = linked();
        s.peer.as_mut().unwrap().sharing = None;
        let g = groups(&input(&s));
        let why = "windows-host is running an older mesh-kvm. Update it to change this.";
        for label in ["Share mouse", "Share keyboard", "Pointer speed on the other device", "Send clipboard", "Receive clipboard"] {
            let r = row(&g, label);
            assert_eq!(r.cells[1].read_only.as_deref(), Some(why), "{label}");
            assert_eq!(r.cells[0].read_only, None, "{label}");
        }
        assert_eq!(row(&g, "Start when this device boots").cells[1].read_only, None);
    }

    #[test]
    fn a_device_with_no_audio_status_has_blank_audio_cells_and_says_so() {
        let mut s = linked();
        s.peer.as_mut().unwrap().audio.play_only_on_other = None;
        let g = groups(&input(&s));
        for r in &g.iter().find(|g| g.group == Group::Audio).unwrap().rows {
            if r.label != "Test sound" {
                assert_eq!(r.cells[1], Cell::blank(), "{}", r.label);
            }
        }
        let note = Note { text: "windows-host: Update mesh-kvm on it before its audio can be changed here.".into(), tone: Tone::Dim };
        assert!(notes(&g, Group::Audio).contains(&note));
    }

    #[test]
    fn the_test_sound_plays_while_linked() {
        let g = groups(&input(&linked()));
        let test = row(&g, "Test sound");
        assert_eq!(test.cells[0].kind, CellKind::Action(ActionKind::TestSound));
        assert_eq!(test.cells[0].tooltip.as_deref(), Some("Play a test sound on windows-host."));
        assert_eq!(test.cells[1].tooltip.as_deref(), Some("Play a test sound on debian-host."));
        let g = groups(&input(&link_down(linked())));
        assert!(row(&g, "Test sound").cells[0].read_only.is_some());
    }

    #[test]
    fn levels_are_meters() {
        let g = groups(&input(&linked()));
        assert_eq!(row(&g, "Level sent").cells[0].kind, CellKind::Level(60));
        assert_eq!(row(&g, "Level received").cells[0].kind, CellKind::Level(0));
    }

    fn one_screen_each() -> Snapshot {
        let mut s = linked();
        s.layout.as_mut().unwrap().config.devices[0].monitors.truncate(1);
        s.local_displays = vec![display("DP-3")];
        s.peer_displays = vec![display("Display 1")];
        s
    }

    #[test]
    fn one_screen_each_reads_resolution_and_refresh_rate() {
        let g = groups(&input(&one_screen_each()));
        assert_eq!(labels(&g, Group::Screens), ["Resolution", "Refresh rate"]);
        let size = row(&g, "Resolution");
        assert_eq!(values(size), ["2560\u{d7}1440 \u{25be}", "2560\u{d7}1440 \u{25be}"]);
        assert_eq!(size.cells[0].tooltip.as_deref(), Some("DP-3 model"));
        assert_eq!(size.cells[1].tooltip.as_deref(), Some("Display 1 model"));
        assert_eq!(size.cells[0].monitor, Some((Debian, "DP-3 (240Hz Native)".to_string())));
        let CellKind::Dropdown { pick, options } = &size.cells[0].kind else { panic!("not a dropdown") };
        assert_eq!(*pick, Pick::Mode("DP-3".into()));
        assert_eq!(options.iter().map(|o| (o.label.as_str(), o.selected)).collect::<Vec<_>>(), [("2560\u{d7}1440", true), ("1920\u{d7}1080", false)]);
        assert_eq!(options[1].value, ChoiceValue::Mode(mode(1920, 1080, 60_000)));
        let rate = row(&g, "Refresh rate");
        assert_eq!(rate.cells[0].value, "144 Hz \u{25be}");
        assert_eq!(rate.cells[0].tooltip.as_deref(), Some("DP-3 model"));
    }

    #[test]
    fn several_screens_are_numbered_and_a_missing_one_is_blank() {
        let mut s = linked();
        s.local_displays = vec![display("DP-3"), display("DP-2")];
        s.peer_displays = vec![display("Display 1")];
        let g = groups(&input(&s));
        assert_eq!(labels(&g, Group::Screens), ["Screen 1 resolution", "Screen 1 refresh rate", "Screen 2 resolution", "Screen 2 refresh rate"]);
        assert_eq!(row(&g, "Screen 2 resolution").cells[0].tooltip.as_deref(), Some("DP-2 model"));
        assert_eq!(row(&g, "Screen 2 resolution").cells[1], Cell::blank());
        assert_eq!(row(&g, "Screen 2 refresh rate").cells[1], Cell::blank());
    }

    #[test]
    fn a_screen_that_cannot_change_shows_its_mode_read_only() {
        let g = groups(&input(&linked()));
        let size = &row(&g, "Screen 1 resolution").cells[0];
        assert_eq!((size.value.as_str(), &size.kind, size.read_only.as_deref()), ("1920\u{d7}1080", &CellKind::Plain, Some(CANT_CHANGE)));
        assert_eq!(size.tooltip.as_deref(), Some("DP-3"));
        let rate = &row(&g, "Screen 1 refresh rate").cells[0];
        assert_eq!((rate.value.as_str(), &rate.kind, rate.read_only.as_deref()), ("240 Hz", &CellKind::Plain, Some(CANT_CHANGE)));
        let mut s = one_screen_each();
        s.local_displays[0].can_change = false;
        let g = groups(&input(&s));
        let size = &row(&g, "Resolution").cells[0];
        assert_eq!((size.value.as_str(), &size.kind), ("2560\u{d7}1440", &CellKind::Plain));
    }

    #[test]
    fn screens_wait_while_a_screen_change_is_being_answered() {
        let mut s = one_screen_each();
        s.local.as_mut().unwrap().display_change.change = Some(waiting_change());
        let g = groups(&input(&s));
        for label in ["Resolution", "Refresh rate"] {
            for cell in &row(&g, label).cells {
                assert!(matches!(cell.kind, CellKind::Dropdown { .. }), "{label}");
                assert_eq!(cell.read_only.as_deref(), Some("Wait for the screen change to finish."), "{label}");
            }
        }
    }

    #[test]
    fn no_layout_has_no_screen_rows() {
        let mut s = linked();
        s.layout = None;
        assert!(labels(&groups(&input(&s)), Group::Screens).is_empty());
    }

    #[test]
    fn notes_name_their_device_and_stand_out_when_something_is_wrong() {
        let mut s = linked();
        let peer = s.peer.as_mut().unwrap();
        peer.clipboard.note = "Couldn't read the clipboard".into();
        peer.clipboard.note_failed = true;
        s.local.as_mut().unwrap().audio.tx_enabled = true;
        s.local.as_mut().unwrap().audio.notes = vec!["The output to send is missing.".into()];
        let g = groups(&input(&s));
        assert_eq!(notes(&g, Group::Clipboard), [Note { text: "windows-host: Couldn't read the clipboard".into(), tone: Tone::Warn }]);
        assert_eq!(notes(&g, Group::Audio), [
            Note { text: "debian-host: The output to send is missing.".into(), tone: Tone::Warn },
            Note { text: "debian-host: Not reaching windows-host yet.".into(), tone: Tone::Dim },
        ]);
    }

    #[test]
    fn audio_notes_follow_the_panels_order() {
        let mut s = audio_live(linked(), 1.0, 40);
        let local = s.local.as_mut().unwrap();
        local.audio.rx_other_version = true;
        local.audio.hint = "Pick an output to send.".into();
        let texts: Vec<_> = notes(&groups(&input(&s)), Group::Audio).into_iter().map(|n| (n.text, n.tone)).collect();
        assert_eq!(texts, [
            ("debian-host: Audio from the other device is being ignored. Update both devices to the same version.".to_string(), Tone::Warn),
            ("debian-host: Pick an output to send.".to_string(), Tone::Dim),
            ("debian-host: Received: 1.0% lost, 0.0% couldn't be repaired, 40 ms held back to smooth it out.".to_string(), Tone::Dim),
        ]);
    }

    #[test]
    fn the_game_lock_hint_is_a_note_for_this_device() {
        let mut s = linked();
        s.layout.as_mut().unwrap().telemetry.game_captive_active = true;
        assert_eq!(notes(&groups(&input(&s)), Group::Games), [Note { text: "debian-host: The mouse is being held on this screen.".into(), tone: Tone::Dim }]);
    }

    #[test]
    fn a_value_being_saved_is_marked_and_a_refusal_sits_under_its_row() {
        let mut s = linked();
        s.peer.as_mut().unwrap().sharing = Some(Sharing { mouse: true, keyboard: false });
        let base = input(&s);
        let mut edits = Edits::default();
        edits.set((Windows, Control::KeyboardSharing), 1.0, base.now);
        let refused = "windows-host didn't accept that change. Try again, or restart mesh-kvm on it.";
        let row_error = |key: (Device, Control)| (key == (Windows, Control::KeyboardSharing)).then(|| refused.to_string());
        let g = groups(&RowsInput { edits: &edits, row_error: &row_error, ..base });
        let keyboard = row(&g, "Share keyboard");
        assert!(keyboard.cells[1].saving);
        assert!(!keyboard.cells[0].saving);
        assert_eq!(keyboard.cells[1].value, "ON");
        assert_eq!(keyboard.error.as_deref(), Some(refused));
        assert_eq!(row(&g, "Share mouse").error, None);
    }
}
