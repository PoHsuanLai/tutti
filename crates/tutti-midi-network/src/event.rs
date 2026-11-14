//! What a session machine reports to its application.

use std::net::SocketAddr;

use tutti_ump::Ump;

use crate::{ByeReason, EndpointIdentity, Header, NakReason, SequenceNumber};

/// The six states a Client or Host is in with respect to one peer (M2-124
/// section 6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// Neither has invited the other.
    Idle,
    /// An Invitation is out and unanswered, or answered Pending.
    PendingInvitation,
    /// A Host has asked for authentication and holds the nonce.
    AuthenticationRequired,
    /// UMP Data Commands flow.
    Established,
    /// A Session Reset is out and unanswered.
    PendingSessionReset,
    /// A Bye is out and unanswered.
    PendingBye,
}

/// Which end ended a Session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Initiator {
    /// This endpoint sent the Bye, or gave up.
    Local,
    /// The peer sent the Bye.
    Remote,
}

/// Why a Client could not answer a challenge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthFailure {
    /// The Host asked for a method the Client holds no credentials for.
    NoCredentials,
    /// The Host did not accept the digest.
    DigestWrong,
    /// The Host does not know the user name.
    UserNotFound,
}

/// What happened in a session, for the application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A Session is Established with a peer.
    Established {
        /// The peer's address.
        peer: SocketAddr,
        /// Who the peer says it is.
        remote: EndpointIdentity,
    },
    /// An Invitation waits for the application: accept it with
    /// [`Host::accept`](crate::Host::accept) or refuse it with
    /// [`Host::decline`](crate::Host::decline).
    Confirm {
        /// The peer's address.
        peer: SocketAddr,
        /// Who the peer says it is.
        remote: EndpointIdentity,
    },
    /// A UMP arrived, in the order it was sent.
    Ump {
        /// The peer's address.
        peer: SocketAddr,
        /// The packet.
        packet: Ump,
    },
    /// UMP Data Commands never arrived and cannot be recovered: the
    /// application may release notes that were left sounding.
    DataLost {
        /// The peer's address.
        peer: SocketAddr,
        /// The first sequence number that was lost.
        first: SequenceNumber,
        /// How many consecutive numbers.
        count: u16,
    },
    /// Both ends reset the sequence numbers and flushed their buffers; the
    /// application may release notes that were left sounding.
    Reset {
        /// The peer's address.
        peer: SocketAddr,
    },
    /// The Session or the pending Invitation ended.
    Closed {
        /// The peer's address.
        peer: SocketAddr,
        /// Which end ended it.
        initiator: Initiator,
        /// The reason given.
        reason: ByeReason,
    },
    /// A Client's authentication did not succeed.
    AuthenticationFailed {
        /// The peer's address.
        peer: SocketAddr,
        /// Why.
        cause: AuthFailure,
    },
    /// The peer refused a command with a NAK.
    Nak {
        /// The peer's address.
        peer: SocketAddr,
        /// Why.
        reason: NakReason,
        /// The header of the command that was refused.
        refused: Header,
    },
}

/// A datagram to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transmit {
    /// Where to send it.
    pub to: SocketAddr,
    /// The UDP payload.
    pub payload: Vec<u8>,
}

/// Why UMP cannot be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum SendError {
    /// No Session is Established with the peer.
    #[error("no Session is established with the peer (state {state:?})")]
    NotEstablished {
        /// The state the peer is in.
        state: SessionState,
    },
}
