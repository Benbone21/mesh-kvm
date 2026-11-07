//! Everything the updater says to the other device. `peer_call` is the only way out of here, so
//! that changing how devices reach each other is a change to that one function.

use super::both::PeerLook;
use super::trust::Version;
use serde_json::{json, Value};

/// One call to the other device's API, through this service's own proxy, which knows the way.
/// Calling from here counts as the app on this device, as the proxy requires for a change.
fn peer_call(method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    crate::webui::http_call("127.0.0.1", method, &proxied(path), body)
}

fn proxied(path: &str) -> String {
    format!("/api/peer{path}")
}

/// Reaching the other device.
pub trait PeerLink: Send + Sync {
    /// `None` when it can't be reached.
    fn look(&self) -> Option<PeerLook>;
    /// Asks it to install its checked download.
    fn install(&self) -> Result<(), String>;
}

pub struct PeerApi;

impl PeerLink for PeerApi {
    fn look(&self) -> Option<PeerLook> {
        match peer_call("GET", "/api/local", "") {
            Ok((200, body)) => Some(read_peer(&serde_json::from_str(&body).ok()?)),
            _ => None,
        }
    }

    fn install(&self) -> Result<(), String> {
        let body = json!({ "action": "install_update" }).to_string();
        let (status, reply) = peer_call("POST", "/api/local/action", &body)?;
        let reply: Value = serde_json::from_str(&reply).unwrap_or_default();
        if status == 200 && reply["ok"] == true {
            return Ok(());
        }
        Err(reply["error"].as_str().unwrap_or("It didn't accept the request.").to_string())
    }
}

/// What matters here in the other device's `/api/local`.
fn read_peer(status: &Value) -> PeerLook {
    let text = |key: &str| status[key].as_str().unwrap_or("");
    let name = [text("device_name"), text("hostname")].into_iter().find(|n| !n.is_empty()).unwrap_or("The other device");
    let failed = (status["update"]["state"] == "failed").then(|| status["update"]["note"].as_str().unwrap_or("").to_string());
    PeerLook { name: name.to_string(), version: Version::parse(text("version")), failed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_goes_through_the_proxy_on_this_device() {
        assert_eq!(proxied("/api/local/action"), "/api/peer/api/local/action");
    }

    #[test]
    fn a_peer_reports_its_name_version_and_failure() {
        let look = read_peer(&json!({ "device_name": "SACBOOKPRO", "version": "0.7.0", "update": { "state": "failed", "note": "No." } }));
        assert_eq!(look, PeerLook { name: "SACBOOKPRO".into(), version: Version::parse("0.7.0"), failed: Some("No.".into()) });
        let look = read_peer(&json!({ "hostname": "laptop", "version": "0.6.1" }));
        assert_eq!(look, PeerLook { name: "laptop".into(), version: Version::parse("0.6.1"), failed: None });
    }
}
