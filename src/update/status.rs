//! How the update is going, as `/api/local` reports it and the app shows it.

use super::trust::Version;
use serde_json::{json, Value};
use std::time::Duration;

/// With no good check for this long, Settings says so. A failed check is otherwise quiet.
pub const STALE_AFTER: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const STALE_NOTE: &str = "mesh-kvm hasn't been able to check for updates for 3 days.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    #[default]
    Idle,
    Downloading,
    /// A screen mode trial is running, and the update starts when it ends.
    WaitingForTrial,
    UpdatingPeer,
    WaitingForPeer,
    Installing,
    /// The last update didn't happen. `note` says why.
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// The newest version with a good signature, downloaded or not.
    pub available: Option<Version>,
    /// That version is downloaded and checked.
    pub ready: bool,
    pub stage: Stage,
    /// How much of the download has arrived, in percent.
    pub progress: Option<u8>,
    pub note: Option<String>,
    /// Too long has gone by with no good check.
    pub stale: bool,
}

impl Status {
    fn state(&self, can_install: bool) -> &'static str {
        match self.stage {
            Stage::Downloading => "downloading",
            Stage::WaitingForTrial => "waiting_for_trial",
            Stage::UpdatingPeer => "updating_peer",
            Stage::WaitingForPeer => "waiting_for_peer",
            Stage::Installing => "installing",
            Stage::Failed => "failed",
            Stage::Idle if self.available.is_some() && !can_install => "view_only",
            Stage::Idle if self.ready => "ready",
            Stage::Idle => "idle",
        }
    }

    /// `update` in `/api/local`.
    pub fn json(&self, current: Version, can_install: bool) -> Value {
        let note = self.note.as_deref().or(self.stale.then_some(STALE_NOTE));
        json!({
            "current": current.to_string(),
            "available": self.available.map(|v| v.to_string()),
            "ready": self.ready && can_install,
            "state": self.state(can_install),
            "note": note,
            "progress": self.progress,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn nothing_new_says_so_with_the_version_running() {
        let json = Status::default().json(v("0.7.0"), true);
        assert_eq!(json, json!({ "current": "0.7.0", "available": null, "ready": false, "state": "idle", "note": null, "progress": null }));
    }

    #[test]
    fn a_checked_download_is_ready_only_on_the_installed_copy() {
        let status = Status { available: Some(v("0.7.1")), ready: true, ..Default::default() };
        let installed = status.json(v("0.7.0"), true);
        assert_eq!((&installed["state"], &installed["ready"], &installed["available"]), (&json!("ready"), &json!(true), &json!("0.7.1")));
        let elsewhere = status.json(v("0.7.0"), false);
        assert_eq!((&elsewhere["state"], &elsewhere["ready"]), (&json!("view_only"), &json!(false)));
    }

    #[test]
    fn a_note_of_its_own_wins_over_the_late_check() {
        let late = Status { stale: true, ..Default::default() };
        assert_eq!(late.json(v("0.7.0"), true)["note"], json!(STALE_NOTE));
        let both = Status { stale: true, note: Some("Wrong hash.".into()), ..Default::default() };
        assert_eq!(both.json(v("0.7.0"), true)["note"], json!("Wrong hash."));
    }

    #[test]
    fn every_stage_of_an_update_has_its_own_state() {
        for (stage, state) in [
            (Stage::Downloading, "downloading"),
            (Stage::WaitingForTrial, "waiting_for_trial"),
            (Stage::UpdatingPeer, "updating_peer"),
            (Stage::WaitingForPeer, "waiting_for_peer"),
            (Stage::Installing, "installing"),
            (Stage::Failed, "failed"),
        ] {
            assert_eq!(Status { stage, ..Default::default() }.json(v("0.7.0"), true)["state"], json!(state));
        }
    }
}
