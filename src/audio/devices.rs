//! Which devices there are, which are chosen, and which are really in use.
//!
//! The lists are refreshed in the background by each platform, so reading them costs nothing: the
//! app asks for status five times a second.

use serde::Serialize;
use std::sync::{Mutex, PoisonError};

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
    pub inputs: Vec<Device>,
    pub outputs: Vec<Device>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub input: String,
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
    /// Whether what this device plays stays out of what it sends.
    pub echo_safe: bool,
    pub notes: Vec<String>,
}

static CHOICE: Mutex<Choice> =
    Mutex::new(Choice { input: String::new(), output: String::new(), play: String::new() });
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
    let choice = CHOICE.lock().unwrap_or_else(PoisonError::into_inner);
    choice.input != NONE || choice.output != NONE
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

#[cfg(test)]
mod tests {
    use super::*;

    fn listed() -> Vec<Device> {
        vec![Device { id: "usb".into(), name: "USB headset".into() }]
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
            gone.missing_note("microphone").unwrap(),
            "The chosen microphone isn't connected, so the system default is being used."
        );
        let back = resolve("usb", &listed());
        assert_eq!(back.target().as_deref(), Some("usb"));
        assert_eq!(back.id(Some(&default)).as_deref(), Some("usb"));
        assert_eq!(back.missing_note("microphone"), None);
        assert_eq!(resolve(NONE, &[]).id(Some(&default)), None);
    }
}
