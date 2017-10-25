//! The context structs hold state used in signaling.

use keystore::{PublicKey};

use super::cookie::{CookiePair};
use super::csn::{CombinedSequencePair};
use super::state::{ServerHandshakeState};
use super::types::{Identity, Address};


pub trait PeerContext {
    fn identity(&self) -> Identity;
    fn permanent_key(&self) -> Option<&PublicKey>;
    fn session_key(&self) -> Option<&PublicKey>;
    fn csn_pair(&self) -> &CombinedSequencePair;
    fn csn_pair_mut(&mut self) -> &mut CombinedSequencePair;
    fn cookie_pair(&self) -> &CookiePair;
    fn cookie_pair_mut(&mut self) -> &mut CookiePair;
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerContext {
    pub(crate) handshake_state: ServerHandshakeState,
    pub(crate) permanent_key: Option<PublicKey>,
    pub(crate) session_key: Option<PublicKey>,
    pub(crate) csn_pair: CombinedSequencePair,
    pub(crate) cookie_pair: CookiePair,
}

impl ServerContext {
    pub fn new() -> Self {
        ServerContext {
            handshake_state: ServerHandshakeState::New,
            permanent_key: None,
            session_key: None,
            csn_pair: CombinedSequencePair::new(),
            cookie_pair: CookiePair::new(),
        }
    }
}

impl PeerContext for ServerContext {
    fn identity(&self) -> Identity {
        Identity::Server
    }

    fn permanent_key(&self) -> Option<&PublicKey> {
        self.permanent_key.as_ref()
    }

    fn session_key(&self) -> Option<&PublicKey> {
        self.session_key.as_ref()
    }

    fn csn_pair(&self) -> &CombinedSequencePair {
        &self.csn_pair
    }

    fn csn_pair_mut(&mut self) -> &mut CombinedSequencePair {
        &mut self.csn_pair
    }

    fn cookie_pair(&self) -> &CookiePair {
        &self.cookie_pair
    }

    fn cookie_pair_mut(&mut self) -> &mut CookiePair {
        &mut self.cookie_pair
    }
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponderContext {
    pub(crate) address: Address,
    pub(crate) permanent_key: Option<PublicKey>,
    pub(crate) session_key: Option<PublicKey>,
    pub(crate) csn_pair: CombinedSequencePair,
    pub(crate) cookie_pair: CookiePair,
}

impl ResponderContext {
    pub fn new(address: Address) -> Self {
        ResponderContext {
            address: address,
            permanent_key: None,
            session_key: None,
            csn_pair: CombinedSequencePair::new(),
            cookie_pair: CookiePair::new(),
        }
    }
}

impl PeerContext for ResponderContext {
    fn identity(&self) -> Identity {
        Identity::Responder(self.address.0)
    }

    fn permanent_key(&self) -> Option<&PublicKey> {
        self.permanent_key.as_ref()
    }

    fn session_key(&self) -> Option<&PublicKey> {
        self.session_key.as_ref()
    }

    fn csn_pair(&self) -> &CombinedSequencePair {
        &self.csn_pair
    }

    fn csn_pair_mut(&mut self) -> &mut CombinedSequencePair {
        &mut self.csn_pair
    }

    fn cookie_pair(&self) -> &CookiePair {
        &self.cookie_pair
    }

    fn cookie_pair_mut(&mut self) -> &mut CookiePair {
        &mut self.cookie_pair
    }
}