pub mod control;
pub mod datagram;
pub mod keys;
pub mod pairing;
pub mod stream;
pub mod window;

use datagram::{Channel, Label, Role};
use keys::{Loaded, Pairing};
use std::sync::{Arc, OnceLock};

/// What the running service has to work with, from `pairing.json`.
pub struct Services {
    /// None when the file can't be used.
    pub pairing: Option<Arc<Pairing>>,
    /// Why the file can't be used, for the app.
    pub problem: Option<String>,
    pub input: Arc<Channel>,
    pub audio_out: Arc<Channel>,
    pub audio_in: Arc<Channel>,
}

impl Services {
    fn from_loaded(loaded: Loaded) -> Services {
        let (pairing, problem) = match loaded {
            Loaded::Ready(pairing) => (Some(Arc::new(pairing)), None),
            Loaded::Damaged(problem) => (None, Some(problem)),
        };
        // A key nobody knows, and no peer: the channels stay as they would unpaired.
        let channels_from = pairing.clone().unwrap_or_else(|| Arc::new(Pairing::fresh()));
        let channel = |label, role| Arc::new(Channel::new(&channels_from, label, role));
        Services {
            input: channel(Label::Input, Role::Both),
            audio_out: channel(Label::Audio, Role::Initiator),
            audio_in: channel(Label::Audio, Role::Responder),
            pairing,
            problem,
        }
    }
}

/// Built the first time it's asked for, so the service asks as it starts.
pub fn services() -> &'static Services {
    static SERVICES: OnceLock<Services> = OnceLock::new();
    SERVICES.get_or_init(|| Services::from_loaded(Pairing::load_from(&keys::pairing_path())))
}

#[cfg(test)]
mod tests {
    use super::datagram::LinkSecurity;
    use super::keys::PeerKey;
    use super::*;

    fn channels(services: &Services) -> [&Channel; 3] {
        [&services.input, &services.audio_out, &services.audio_in]
    }

    #[test]
    fn a_damaged_file_gives_no_peer_and_keeps_the_reason() {
        let services = Services::from_loaded(Loaded::Damaged("not usable".to_string()));
        assert!(services.pairing.is_none());
        assert_eq!(services.problem.as_deref(), Some("not usable"));
        assert!(channels(&services).iter().all(|c| c.security() == LinkSecurity::Unpaired));
    }

    #[test]
    fn a_pairing_with_no_peer_leaves_every_channel_unpaired() {
        let services = Services::from_loaded(Loaded::Ready(Pairing::fresh()));
        assert!(services.pairing.is_some() && services.problem.is_none());
        assert!(channels(&services).iter().all(|c| c.security() == LinkSecurity::Unpaired));
    }

    #[test]
    fn a_peer_starts_every_channel_connecting() {
        let mut pairing = Pairing::fresh();
        pairing.peer = Some(PeerKey { public: [7; 32], name: "Peer".to_string(), paired_at: 1 });
        let services = Services::from_loaded(Loaded::Ready(pairing));
        assert!(channels(&services).iter().all(|c| c.security() == LinkSecurity::Connecting));
    }
}
