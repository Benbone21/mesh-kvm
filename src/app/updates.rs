//! What the app says about updates: what the status means for the banner, and noticing when the
//! service has moved on to a new version. No egui types in here.

use super::model::{Device, Snapshot};
use serde::Deserialize;

/// Where a version's release notes are. Made here, so nothing a device reports becomes a link.
pub fn notes_url(version: &str) -> String {
    format!("https://github.com/Benbone21/mesh-kvm/releases/tag/v{version}")
}

/// `update` in `/api/local`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct UpdateStatus {
    pub current: String,
    pub available: Option<String>,
    pub ready: bool,
    pub state: String,
    pub note: Option<String>,
    pub progress: Option<u8>,
    /// "peer" when the failure was the other device's.
    pub failed_on: Option<String>,
    /// "allowed", "needs_password", "denied" or "unknown".
    pub preflight: Option<String>,
    /// The command that installs the ready download by hand.
    pub by_hand: Option<String>,
}

impl UpdateStatus {
    /// True while this device is part of an update that has started.
    fn is_updating(&self) -> bool {
        matches!(self.state.as_str(), "waiting_for_trial" | "updating_peer" | "waiting_for_peer" | "installing")
    }

    fn is_ready_with(&self, version: &str) -> bool {
        self.ready && self.available.as_deref() == Some(version)
    }

    /// True when this device has said it won't let an install go ahead.
    fn is_blocked(&self) -> bool {
        matches!(self.preflight.as_deref(), Some("needs_password" | "denied"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateButton {
    Both,
    ThisDevice,
}

impl UpdateButton {
    pub fn label(self) -> &'static str {
        match self {
            Self::Both => "Update both devices",
            Self::ThisDevice => "Update this device",
        }
    }

    /// The action for this device's service. It names nothing: only what's checked can install.
    pub fn action(self) -> serde_json::Value {
        match self {
            Self::Both => serde_json::json!({ "action": "update_both" }),
            Self::ThisDevice => serde_json::json!({ "action": "install_update" }),
        }
    }
}

/// The line at the top of the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub text: String,
    /// A smaller line of what to expect.
    pub detail: Option<String>,
    pub button: Option<UpdateButton>,
    /// The version whose release notes to link to.
    pub notes: Option<String>,
    pub failed: bool,
    /// An update is under way, so losing sight of the service is no surprise.
    pub busy: bool,
}

impl Banner {
    fn step(text: impl Into<String>) -> Self {
        let detail = Some("Sharing drops for a few seconds.".to_string());
        Self { text: text.into(), detail, button: None, notes: None, failed: false, busy: true }
    }

    fn ready(version: &str, detail: String, button: Option<UpdateButton>) -> Self {
        Self { text: format!("Version {version} is ready"), detail: Some(detail), button, notes: Some(version.to_string()), failed: false, busy: false }
    }
}

const SHARING_DROPS: &str = "Sharing drops for a few seconds.";

/// What to show for this device's update, if anything.
pub fn update_banner(snapshot: &Snapshot) -> Option<Banner> {
    let local = snapshot.local.as_ref()?.update.as_ref()?;
    let [_, other] = snapshot.in_order();
    let name = snapshot.name(other);
    let peer = snapshot.peer.as_ref().and_then(|p| p.update.as_ref());
    match local.state.as_str() {
        "waiting_for_trial" => return Some(Banner::step("Waiting for the screen change to finish")),
        "updating_peer" => return Some(Banner::step(format!("Updating {name}"))),
        "waiting_for_peer" => return Some(Banner::step("Waiting for it to come back")),
        "installing" => return Some(Banner::step("Updating this device")),
        // The other device got there in the end, so what's left is for this one to follow.
        "failed" if failed_peer_has_arrived(snapshot, local) => {}
        "failed" => {
            let text = local.note.clone().unwrap_or_else(|| "The update didn't work.".to_string());
            let button = local.available.as_deref().filter(|_| local.ready).and_then(|v| ready_banner(snapshot, v, &name).button);
            return Some(Banner { text, detail: None, button, notes: None, failed: true, busy: false });
        }
        "view_only" => {
            let version = local.available.clone()?;
            return Some(Banner { text: format!("Version {version} is available"), detail: None, button: None, notes: Some(version), failed: false, busy: false });
        }
        _ => {}
    }
    if peer.is_some_and(UpdateStatus::is_updating) {
        return Some(Banner::step(format!("Updating {name}")));
    }
    if local.state == "downloading" {
        // Only worth a line when the other device is waiting for this one.
        let version = local.available.as_deref()?;
        peer.filter(|p| p.is_ready_with(version))?;
        let text = format!("Getting version {version} on this device: {}%", local.progress.unwrap_or(0));
        return Some(Banner { text, detail: None, button: None, notes: Some(version.to_string()), failed: false, busy: false });
    }
    let version = local.available.as_deref().filter(|_| local.ready)?;
    if local.is_blocked() {
        let why = match local.preflight.as_deref() {
            Some("denied") => "This device doesn't allow mesh-kvm to install updates.",
            _ => "This device wants a password before installing, and mesh-kvm can't ask for one.",
        };
        let detail = local.by_hand.as_ref().map_or_else(|| why.to_string(), |command| format!("{why} To update by hand, run: {command}"));
        return Some(Banner::ready(version, detail, None));
    }
    Some(ready_banner(snapshot, version, &name))
}

/// The line on a Windows copy that isn't the installed one, such as one run from Downloads with
/// nothing installed. It doesn't install itself or update, so it says how to install.
const NOT_INSTALLED: &str = "This copy of mesh-kvm isn't installed, so it won't start at sign-in or update itself.";
const HOW_TO_INSTALL: &str = "To install mesh-kvm, run mesh-kvm-setup.exe.";

/// Adds how to install to the line, on a copy that isn't installed. An update under way, a failure
/// or a button keeps the line to itself.
pub fn with_install_hint(banner: Option<Banner>, not_installed: bool) -> Option<Banner> {
    if !not_installed {
        return banner;
    }
    match banner {
        Some(banner) if banner.busy || banner.failed || banner.button.is_some() => Some(banner),
        Some(banner) => Some(Banner { detail: Some(HOW_TO_INSTALL.to_string()), ..banner }),
        None => Some(Banner { text: NOT_INSTALLED.to_string(), detail: Some(HOW_TO_INSTALL.to_string()), button: None, notes: None, failed: false, busy: false }),
    }
}

fn failed_peer_has_arrived(snapshot: &Snapshot, local: &UpdateStatus) -> bool {
    let arrived = snapshot.peer.as_ref().is_some_and(|p| Some(p.version.as_str()) == local.available.as_deref());
    local.failed_on.as_deref() == Some("peer") && local.ready && arrived
}

/// Offering a version this device has ready, which depends on the other device.
fn ready_banner(snapshot: &Snapshot, version: &str, name: &str) -> Banner {
    let apart = "The two won't work together until both match.";
    let Some(peer) = snapshot.peer.as_ref() else {
        return Banner::ready(version, format!("{name} is offline. {apart}"), Some(UpdateButton::ThisDevice));
    };
    let Some(update) = peer.update.as_ref() else {
        return Banner::ready(version, format!("{name} can't update itself. {apart}"), Some(UpdateButton::ThisDevice));
    };
    if update.is_ready_with(version) && update.is_blocked() {
        return Banner::ready(version, format!("{name} can't install it by itself. {apart}"), Some(UpdateButton::ThisDevice));
    }
    if update.is_ready_with(version) {
        return Banner::ready(version, SHARING_DROPS.to_string(), Some(UpdateButton::Both));
    }
    if peer.version == version {
        return Banner::ready(version, SHARING_DROPS.to_string(), Some(UpdateButton::ThisDevice));
    }
    if update.state == "downloading" {
        return Banner::ready(version, format!("Waiting for {name} to get it: {}%", update.progress.unwrap_or(0)), None);
    }
    Banner::ready(version, format!("{name} doesn't have it yet. {apart}"), Some(UpdateButton::ThisDevice))
}

/// A device that should look for an update now, as the other one has found it. The two check at
/// their own times, up to 6 hours apart, and the one that's behind would hold up the button.
pub fn device_to_nudge(snapshot: &Snapshot) -> Option<(Device, String)> {
    let [here, other] = snapshot.in_order();
    let local = snapshot.local.as_ref()?.update.as_ref()?;
    let peer = snapshot.peer.as_ref()?.update.as_ref()?;
    let behind = |this: &UpdateStatus, that: &UpdateStatus| that.ready && this.state == "idle" && this.available.is_none();
    if behind(peer, local) {
        return Some((other, local.available.clone()?));
    }
    if behind(local, peer) {
        return Some((here, peer.available.clone()?));
    }
    None
}

/// Notices the service moving to another version under a running app or tray, which then has to
/// start again from the new exe. A service that was never on this version, such as with a copy run
/// from somewhere else, is left alone, or that copy would start itself again for ever.
#[derive(Debug, Default)]
pub struct VersionWatch {
    seen_same: bool,
}

impl VersionWatch {
    /// True once, when it's time to start again.
    pub fn moved_on(&mut self, own: &str, service: Option<&str>) -> bool {
        let Some(service) = service else { return false };
        if service == own {
            self.seen_same = true;
            return false;
        }
        std::mem::take(&mut self.seen_same)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model::DeviceStatus;

    fn status(state: &str, available: Option<&str>, ready: bool) -> UpdateStatus {
        UpdateStatus {
            current: "0.7.0".into(),
            available: available.map(str::to_string),
            ready,
            state: state.into(),
            ..Default::default()
        }
    }

    fn device(node: &str, name: &str, update: Option<UpdateStatus>) -> DeviceStatus {
        DeviceStatus { node: node.into(), device_name: name.into(), version: "0.7.0".into(), update, ..Default::default() }
    }

    fn snapshot(local: UpdateStatus, peer: Option<Option<UpdateStatus>>) -> Snapshot {
        Snapshot {
            local: Some(device("debian", "Desk", Some(local))),
            peer: peer.map(|update| device("windows", "SACBOOKPRO", update)),
            ..Default::default()
        }
    }

    fn ready() -> UpdateStatus {
        status("ready", Some("0.7.1"), true)
    }

    #[test]
    fn a_copy_that_is_not_installed_says_how_to_install() {
        let banner = with_install_hint(None, true).unwrap();
        assert!(banner.text.contains("isn't installed"), "{banner:?}");
        assert!(banner.detail.unwrap().contains("mesh-kvm-setup.exe"));
        assert_eq!(with_install_hint(None, false), None);
    }

    #[test]
    fn a_copy_that_is_not_installed_still_says_what_version_is_out() {
        let available = update_banner(&snapshot(status("view_only", Some("0.7.1"), false), Some(Some(ready()))));
        let banner = with_install_hint(available, true).unwrap();
        assert_eq!(banner.text, "Version 0.7.1 is available");
        assert!(banner.detail.unwrap().contains("mesh-kvm-setup.exe"));
    }

    #[test]
    fn nothing_new_shows_nothing() {
        assert_eq!(update_banner(&snapshot(status("idle", None, false), Some(Some(status("idle", None, false))))), None);
        assert_eq!(update_banner(&Snapshot::default()), None, "no service");
    }

    #[test]
    fn with_the_other_device_ready_the_button_updates_both() {
        let banner = update_banner(&snapshot(ready(), Some(Some(ready())))).unwrap();
        assert_eq!(banner.text, "Version 0.7.1 is ready");
        assert_eq!(banner.button, Some(UpdateButton::Both));
        assert_eq!(banner.button.unwrap().label(), "Update both devices");
        assert_eq!(banner.notes.as_deref(), Some("0.7.1"));
        assert!(banner.detail.unwrap().contains("Sharing drops"), "it says so first");
    }

    #[test]
    fn with_the_other_device_offline_the_button_updates_this_one_and_says_what_that_means() {
        let banner = update_banner(&snapshot(ready(), None)).unwrap();
        assert_eq!(banner.button, Some(UpdateButton::ThisDevice));
        assert_eq!(banner.button.unwrap().label(), "Update this device");
        assert!(banner.detail.unwrap().contains("won't work together until both match"));
    }

    #[test]
    fn with_the_other_device_still_downloading_this_one_waits_and_shows_how_far() {
        let mut peer = status("downloading", Some("0.7.1"), false);
        peer.progress = Some(45);
        let banner = update_banner(&snapshot(ready(), Some(Some(peer)))).unwrap();
        assert_eq!(banner.button, None);
        assert!(banner.detail.unwrap().contains("SACBOOKPRO to get it: 45%"));
    }

    #[test]
    fn with_this_device_still_downloading_it_shows_how_far_once_the_other_is_waiting() {
        let mut here = status("downloading", Some("0.7.1"), false);
        here.progress = Some(12);
        let banner = update_banner(&snapshot(here.clone(), Some(Some(ready())))).unwrap();
        assert!(banner.text.contains("12%"));
        assert_eq!(banner.button, None);
        assert_eq!(update_banner(&snapshot(here, Some(Some(status("idle", None, false))))), None, "nothing is offered yet");
    }

    #[test]
    fn an_older_other_device_or_one_without_it_falls_back_to_this_device() {
        for peer in [None, Some(status("idle", None, false)), Some(status("view_only", Some("0.7.1"), false)), Some(status("ready", Some("0.7.0"), true))] {
            let banner = update_banner(&snapshot(ready(), Some(peer.clone()))).unwrap();
            assert_eq!(banner.button, Some(UpdateButton::ThisDevice), "{peer:?}");
            assert!(banner.detail.unwrap().contains("won't work together"), "{peer:?}");
        }
    }

    #[test]
    fn with_the_other_device_already_on_the_version_only_this_one_is_left() {
        let mut snapshot = snapshot(ready(), Some(Some(status("idle", None, false))));
        snapshot.peer.as_mut().unwrap().version = "0.7.1".into();
        let banner = update_banner(&snapshot).unwrap();
        assert_eq!(banner.button, Some(UpdateButton::ThisDevice));
        assert!(!banner.detail.unwrap().contains("won't work together"));
    }

    #[test]
    fn each_step_of_an_update_has_its_own_line_and_no_button() {
        for (state, text) in [
            ("updating_peer", "Updating SACBOOKPRO"),
            ("waiting_for_peer", "Waiting for it to come back"),
            ("installing", "Updating this device"),
            ("waiting_for_trial", "Waiting for the screen change to finish"),
        ] {
            let banner = update_banner(&snapshot(status(state, Some("0.7.1"), true), Some(Some(ready())))).unwrap();
            assert_eq!(banner.text, text);
            assert_eq!(banner.button, None);
            assert!(banner.busy);
            assert!(banner.detail.unwrap().contains("Sharing drops"));
        }
    }

    #[test]
    fn the_other_device_leading_an_update_shows_here_too() {
        let banner = update_banner(&snapshot(status("idle", None, false), Some(Some(status("installing", Some("0.7.1"), true))))).unwrap();
        assert_eq!(banner.text, "Updating SACBOOKPRO");
    }

    #[test]
    fn a_copy_that_can_not_install_shows_the_version_only() {
        let banner = update_banner(&snapshot(status("view_only", Some("0.7.1"), false), Some(Some(ready())))).unwrap();
        assert_eq!(banner.text, "Version 0.7.1 is available");
        assert_eq!((banner.button, banner.detail), (None, None));
    }

    #[test]
    fn a_failure_shows_its_note_and_can_be_tried_again() {
        let mut here = status("failed", Some("0.7.1"), true);
        here.note = Some("SACBOOKPRO didn't come back on version 0.7.1 within 5 minutes.".into());
        let banner = update_banner(&snapshot(here.clone(), Some(Some(ready())))).unwrap();
        assert!(banner.failed);
        assert!(banner.text.contains("SACBOOKPRO"));
        assert_eq!(banner.button, Some(UpdateButton::Both));
        here.ready = false;
        assert_eq!(update_banner(&snapshot(here, None)).unwrap().button, None, "nothing left to try");
    }

    #[test]
    fn a_failure_that_was_the_other_devices_clears_once_it_turns_up_on_the_version() {
        let mut here = status("failed", Some("0.7.1"), true);
        here.note = Some("SACBOOKPRO didn't come back on version 0.7.1 within 5 minutes.".into());
        here.failed_on = Some("peer".into());
        let mut late = snapshot(here.clone(), Some(Some(status("idle", None, false))));
        late.peer.as_mut().unwrap().version = "0.7.1".into();
        let banner = update_banner(&late).unwrap();
        assert!(!banner.failed);
        assert_eq!(banner.button, Some(UpdateButton::ThisDevice));
        assert!(update_banner(&snapshot(here.clone(), Some(Some(ready())))).unwrap().failed, "still on the old version");
        here.failed_on = None;
        late.local.as_mut().unwrap().update = Some(here);
        assert!(update_banner(&late).unwrap().failed, "this device's own failure stays");
    }

    #[test]
    fn a_device_that_would_not_install_says_so_with_the_command_and_no_button() {
        for (answer, says) in [("needs_password", "wants a password"), ("denied", "doesn't allow")] {
            let mut here = ready();
            here.preflight = Some(answer.into());
            here.by_hand = Some("sudo apt-get install /x.deb".into());
            let banner = update_banner(&snapshot(here, Some(Some(ready())))).unwrap();
            assert_eq!(banner.button, None, "{answer}");
            let detail = banner.detail.unwrap();
            assert!(detail.contains(says) && detail.ends_with("sudo apt-get install /x.deb"), "{detail}");
        }
    }

    #[test]
    fn a_preflight_that_found_nothing_out_stops_nothing() {
        let mut here = ready();
        here.preflight = Some("unknown".into());
        assert_eq!(update_banner(&snapshot(here, Some(Some(ready())))).unwrap().button, Some(UpdateButton::Both));
    }

    #[test]
    fn an_other_device_that_would_not_install_leaves_this_device_only() {
        let mut there = ready();
        there.preflight = Some("denied".into());
        let banner = update_banner(&snapshot(ready(), Some(Some(there)))).unwrap();
        assert_eq!(banner.button, Some(UpdateButton::ThisDevice));
        assert!(banner.detail.unwrap().contains("can't install it by itself"));
    }

    #[test]
    fn a_status_from_an_older_service_shows_nothing() {
        let older = Snapshot { local: Some(device("debian", "Desk", None)), ..Default::default() };
        assert_eq!(update_banner(&older), None);
    }

    #[test]
    fn the_action_names_no_file_or_version() {
        for button in [UpdateButton::Both, UpdateButton::ThisDevice] {
            assert_eq!(button.action().as_object().unwrap().len(), 1, "{:?}", button.action());
        }
    }

    #[test]
    fn the_link_is_built_here() {
        assert_eq!(notes_url("0.7.1"), "https://github.com/Benbone21/mesh-kvm/releases/tag/v0.7.1");
    }

    #[test]
    fn the_device_that_has_not_looked_yet_is_told_to() {
        let idle = status("idle", None, false);
        let behind = snapshot(ready(), Some(Some(idle.clone())));
        assert_eq!(device_to_nudge(&behind), Some((Device::Windows, "0.7.1".to_string())));
        let here_behind = snapshot(idle.clone(), Some(Some(ready())));
        assert_eq!(device_to_nudge(&here_behind), Some((Device::Debian, "0.7.1".to_string())));
        assert_eq!(device_to_nudge(&snapshot(ready(), Some(Some(ready())))), None);
        assert_eq!(device_to_nudge(&snapshot(ready(), None)), None);
        assert_eq!(device_to_nudge(&snapshot(ready(), Some(Some(status("downloading", Some("0.7.1"), false))))), None);
    }

    #[test]
    fn a_new_version_under_a_running_app_counts_once() {
        let mut watch = VersionWatch::default();
        assert!(!watch.moved_on("0.7.0", None));
        assert!(!watch.moved_on("0.7.0", Some("0.7.0")));
        assert!(!watch.moved_on("0.7.0", None), "the service restarting");
        assert!(watch.moved_on("0.7.0", Some("0.7.1")));
        assert!(!watch.moved_on("0.7.0", Some("0.7.1")), "once");
    }

    #[test]
    fn a_copy_that_never_matched_its_service_stays_put() {
        let mut watch = VersionWatch::default();
        assert!(!watch.moved_on("0.8.0", Some("0.7.0")));
        assert!(!watch.moved_on("0.8.0", Some("0.7.0")));
    }
}
