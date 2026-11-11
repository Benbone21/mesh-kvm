//! Which devices there are, which are chosen, and which are really in use.
//!
//! The lists are refreshed in the background by each platform, so reading them costs nothing: the
//! app asks for status five times a second.

use super::mix::Activity;
use serde::Serialize;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Nothing chosen. Saved settings hold this, the default, or a device's id.
pub const NONE: &str = "";
/// The system's default device, whichever that is at the time.
pub const DEFAULT: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    /// Survives replugging and restarts: `node.name` on Linux, the endpoint id on Windows.
    pub id: String,
    pub name: String,
}

/// The devices of this machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lists {
    pub outputs: Vec<Device>,
    pub default_output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub output: String,
    pub play: String,
}

/// What the choices came to, once the devices present are taken into account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InUse {
    /// The real id of the output being captured.
    pub output: Option<String>,
    /// The real id of the output received audio plays on.
    pub play: Option<String>,
    /// Another output to mute with the one sent, where capture isn't tied to one output.
    pub also_mute: Option<String>,
    /// Whether what this device plays stays out of what it sends.
    pub echo_safe: bool,
    /// Something worth knowing about what's sent, with nothing wrong.
    pub hint: String,
    pub notes: Vec<String>,
}

static CHOICE: Mutex<Choice> =
    Mutex::new(Choice { output: String::new(), play: String::new() });
static LISTS: Mutex<Option<Lists>> = Mutex::new(None);
static IN_USE: Mutex<Option<InUse>> = Mutex::new(None);

pub fn choice() -> Choice {
    CHOICE.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

pub fn set_choice(choice: Choice) {
    *CHOICE.lock().unwrap_or_else(PoisonError::into_inner) = choice;
}

/// True when something is chosen to send.
pub fn sending() -> bool {
    CHOICE.lock().unwrap_or_else(PoisonError::into_inner).output != NONE
}

pub fn lists() -> Lists {
    LISTS.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_default()
}

pub fn set_lists(lists: Lists) {
    *LISTS.lock().unwrap_or_else(PoisonError::into_inner) = Some(lists);
}

pub fn in_use() -> InUse {
    IN_USE.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_default()
}

pub fn set_in_use(in_use: InUse) {
    *IN_USE.lock().unwrap_or_else(PoisonError::into_inner) = Some(in_use);
}

/// What one choice came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Off,
    /// The system default: nothing needs naming, and following it is up to the system.
    Default,
    Device(String),
    /// What was chosen isn't here, so the default stands in.
    Missing,
}

/// Works out one choice against the devices present.
pub fn resolve(chosen: &str, devices: &[Device]) -> Resolved {
    match chosen {
        NONE => Resolved::Off,
        DEFAULT => Resolved::Default,
        id if devices.iter().any(|device| device.id == id) => Resolved::Device(id.to_string()),
        _ => Resolved::Missing,
    }
}

impl Resolved {
    /// The real id, given the system default.
    pub fn id(&self, default: Option<&String>) -> Option<String> {
        match self {
            Resolved::Off => None,
            Resolved::Device(id) => Some(id.clone()),
            Resolved::Default | Resolved::Missing => default.cloned(),
        }
    }

    /// The name to ask for, with `None` meaning the system's choice.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub fn target(&self) -> Option<String> {
        match self {
            Resolved::Device(id) => Some(id.clone()),
            _ => None,
        }
    }

    pub fn missing_note(&self, what: &str) -> Option<String> {
        (*self == Resolved::Missing).then(|| format!("The chosen {what} isn't connected, so the system default is being used."))
    }
}

/// How long a capture gets to start delivering.
const CAPTURE_GRACE: Duration = Duration::from_secs(4);

/// How a capture that's wanted is getting on.
#[derive(Debug, Clone, Copy)]
pub struct CaptureHealth {
    /// It failed to open or stopped by itself a moment ago.
    pub failed: bool,
    /// How long it has been up, and been expected to deliver.
    pub expected_for: Option<Duration>,
    pub activity: Activity,
}

impl CaptureHealth {
    /// A note for the app if there's something wrong, with `help` saying what to try.
    pub fn note(&self, what: &str, help: &str, now: Instant) -> Option<String> {
        let finish = |text: String| Some(if help.is_empty() { text } else { format!("{text} {help}") });
        if self.failed {
            return finish(format!("The {what} couldn't be opened."));
        }
        let up_for = self.expected_for?;
        let longer_than = |at: Option<Instant>, limit: Duration| {
            up_for >= limit && at.is_none_or(|at| now.duration_since(at) >= limit)
        };
        if longer_than(self.activity.data, CAPTURE_GRACE) {
            return finish(format!("The {what} isn't delivering any audio."));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed() -> Vec<Device> {
        vec![Device { id: "usb".into(), name: "USB headset".into() }]
    }

    fn health(failed: bool, up_for: Option<u64>, data: Option<Instant>, sound: Option<Instant>) -> CaptureHealth {
        CaptureHealth { failed, expected_for: up_for.map(Duration::from_secs), activity: Activity { data, sound } }
    }

    #[test]
    fn a_capture_that_failed_or_delivers_nothing_is_called_out() {
        let t = Instant::now();
        let later = t + Duration::from_secs(60);
        let note = |h: CaptureHealth| h.note("output to send", "Try this.", later);
        assert_eq!(note(health(true, None, None, None)).unwrap(), "The output to send couldn't be opened. Try this.");
        assert_eq!(note(health(false, Some(60), None, None)).unwrap(), "The output to send isn't delivering any audio. Try this.");
        assert_eq!(note(health(false, Some(1), None, None)), None, "it has only just started");
        assert_eq!(note(health(false, Some(60), Some(later), Some(later))), None, "all is well");
    }

    #[test]
    fn an_output_delivering_only_silence_is_not_called_out() {
        let t = Instant::now();
        let later = t + Duration::from_secs(60);
        assert_eq!(health(false, Some(60), Some(later), None).note("output", "", later), None);
    }

    #[test]
    fn nothing_from_a_capture_that_isn_t_expected_to_deliver_says_nothing() {
        let t = Instant::now();
        assert_eq!(health(false, None, None, None).note("output", "", t), None);
    }

    #[test]
    fn a_choice_resolves_against_what_is_plugged_in() {
        assert_eq!(resolve(NONE, &listed()), Resolved::Off);
        assert_eq!(resolve(DEFAULT, &listed()), Resolved::Default);
        assert_eq!(resolve("usb", &listed()), Resolved::Device("usb".into()));
        assert_eq!(resolve("usb", &[]), Resolved::Missing, "unplugged");
    }

    #[test]
    fn a_missing_device_stands_down_to_the_default_with_a_note_and_comes_back() {
        let default = "speakers".to_string();
        let gone = resolve("usb", &[]);
        assert_eq!(gone.target(), None);
        assert_eq!(gone.id(Some(&default)), Some(default.clone()));
        assert_eq!(
            gone.missing_note("output to send").unwrap(),
            "The chosen output to send isn't connected, so the system default is being used."
        );
        let back = resolve("usb", &listed());
        assert_eq!(back.target().as_deref(), Some("usb"));
        assert_eq!(back.id(Some(&default)).as_deref(), Some("usb"));
        assert_eq!(back.missing_note("output to send"), None);
        assert_eq!(resolve(NONE, &[]).id(Some(&default)), None);
    }
}
