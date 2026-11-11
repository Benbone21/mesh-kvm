//! Everything the updater says to the other device. `peer_call` is the only way out of here, so
//! that changing how devices reach each other is a change to that one function.

use super::both::{Asked, PeerLook};
use super::trust::Version;
use serde_json::{json, Value};

/// One call to the other device's API, straight over the control connection this service keeps.
/// It's tried even while the input link is said to be down, unlike the app's calls through the
/// proxy: an update can change how the input link works, and the pair must still reach each other
/// to come back into step.
fn peer_call(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    crate::secure::control::peer_call_any_link(method, path, body)
}

/// `peer_call`, for what can wait on a slow link. The swap to another way of reaching the other
/// device goes here as well.
fn peer_call_patiently(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    crate::secure::control::peer_call_any_link_patiently(method, path, body)
}

/// Reaching the other device.
pub trait PeerLink: Send + Sync {
    /// `None` when it can't be reached.
    fn look(&self) -> Option<PeerLook>;
    /// Asks it to install its checked download.
    fn install(&self) -> Asked;
}

pub struct PeerApi;

impl PeerLink for PeerApi {
    fn look(&self) -> Option<PeerLook> {
        match peer_call("GET", "/api/local", "") {
            Ok((200, body)) => Some(read_peer(&serde_json::from_str(&body).ok()?)),
            _ => None,
        }
    }

    fn install(&self) -> Asked {
        let body = json!({ "action": "install_update" }).to_string();
        read_answer(peer_call_patiently("POST", "/api/local/action", &body))
    }
}

/// What came back from asking. Only the other device's own word counts as a refusal: a timeout or
/// the proxy giving up says nothing about whether it heard.
fn read_answer(answer: Result<(u16, String), String>) -> Asked {
    let Ok((status, reply)) = answer else { return Asked::Unclear };
    let reply: Option<Value> = serde_json::from_str(&reply).ok();
    match (status, reply) {
        (200, Some(reply)) if reply["ok"] == true => Asked::Accepted,
        (200, Some(reply)) if reply["ok"] == false => {
            Asked::Refused(reply["error"].as_str().unwrap_or("It didn't accept the request.").to_string())
        }
        (403, _) => Asked::Refused("It doesn't accept changes from this device.".to_string()),
        _ => Asked::Unclear,
    }
}

/// What matters here in the other device's `/api/local`.
fn read_peer(status: &Value) -> PeerLook {
    let text = |key: &str| status[key].as_str().unwrap_or("");
    let name = [text("device_name"), text("hostname")].into_iter().find(|n| !n.is_empty()).unwrap_or("The other device");
    let update = &status["update"];
    let failed = (update["state"] == "failed").then(|| update["note"].as_str().unwrap_or("").to_string());
    let updating = ["waiting_for_trial", "updating_peer", "waiting_for_peer", "installing"].iter().any(|s| update["state"] == *s);
    PeerLook { name: name.to_string(), version: Version::parse(text("version")), failed, proving: update["proving"] == true, updating }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_reports_its_name_version_and_failure() {
        let look = read_peer(&json!({ "device_name": "SACBOOKPRO", "version": "0.7.0", "update": { "state": "failed", "note": "No." } }));
        assert_eq!(look, PeerLook { name: "SACBOOKPRO".into(), version: Version::parse("0.7.0"), failed: Some("No.".into()), ..Default::default() });
        let look = read_peer(&json!({ "hostname": "laptop", "version": "0.6.1" }));
        assert_eq!(look, PeerLook { name: "laptop".into(), version: Version::parse("0.6.1"), ..Default::default() });
    }

    #[test]
    fn a_peer_says_when_it_is_proving_or_busy() {
        assert!(read_peer(&json!({ "update": { "state": "idle", "proving": true } })).proving);
        assert!(read_peer(&json!({ "update": { "state": "installing" } })).updating);
        let quiet = read_peer(&json!({ "update": { "state": "ready" } }));
        assert!(!quiet.proving && !quiet.updating);
    }

    #[test]
    fn only_the_peers_own_word_is_a_refusal() {
        assert_eq!(read_answer(Ok((200, r#"{"ok":true}"#.into()))), Asked::Accepted);
        assert_eq!(read_answer(Ok((200, r#"{"ok":false,"error":"No."}"#.into()))), Asked::Refused("No.".into()));
        assert!(matches!(read_answer(Ok((403, String::new()))), Asked::Refused(_)));
        assert_eq!(read_answer(Err("timed out".into())), Asked::Unclear);
        assert_eq!(read_answer(Ok((502, r#"{"error":"peer unreachable"}"#.into()))), Asked::Unclear);
        assert_eq!(read_answer(Ok((200, String::new()))), Asked::Unclear, "cut off on the way back");
    }
}
