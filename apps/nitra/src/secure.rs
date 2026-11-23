//! Signed signaling over a channel: everything sent is sealed for the peer
//! with this profile's identity, everything received is verified (signature,
//! recipient, trust, expiry, replay) before the app looks at it.

use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use identity::{Identity, PublicKey};
use signaling::Message;
use signaling::local::LocalChannel;
use signaling::signed::{self, Envelope, Opened, Rejected, Trust, Verifier};

/// Shared by every channel of the process: one replay cache for all, so a
/// message copied onto another connection is refused too.
#[derive(Clone)]
pub struct Secure {
    identity: Arc<Identity>,
    verifier: Arc<Mutex<Verifier>>,
}

#[derive(Debug)]
pub enum RecvError {
    Io(io::Error),
    Rejected(Rejected),
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "signaling: {e}"),
            Self::Rejected(r) => write!(f, "signaling message refused: {r}"),
        }
    }
}

impl std::error::Error for RecvError {}

impl Secure {
    pub fn new(identity: Arc<Identity>, trust: Trust) -> Self {
        let verifier = Verifier::new(*identity.public_key(), trust);
        Self {
            identity,
            verifier: Arc::new(Mutex::new(verifier)),
        }
    }

    /// Sends `message` signed for `to`; returns its nonce.
    pub fn send(
        &self,
        channel: &mut LocalChannel,
        to: &PublicKey,
        message: Message,
        reply_to: Option<&str>,
    ) -> io::Result<String> {
        let (envelope, nonce) = signed::seal(&self.identity, to, message, reply_to, signed::now())?;
        channel.send(&envelope)?;
        Ok(nonce)
    }

    /// The next message, verified.
    pub fn recv(&self, channel: &mut LocalChannel) -> Result<Opened, RecvError> {
        let envelope: Envelope = channel.recv().map_err(RecvError::Io)?;
        self.verifier
            .lock()
            .unwrap()
            .open(&envelope, signed::now())
            .map_err(RecvError::Rejected)
    }
}
