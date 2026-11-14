//! The two ends of an Initiator's connection.

use super::{CiDeviceId, CiHeader, CiMessage, Destination, Muid};

/// This device and the peer an Initiator talks to.
#[derive(Clone, Copy, Debug)]
pub(super) struct Link {
    pub(super) own: Muid,
    pub(super) peer: Muid,
}

impl Link {
    /// Whether `message` is from the peer.
    pub(super) fn is_from_peer(&self, message: &CiMessage) -> bool {
        message.header().source == self.peer
    }

    /// Whether `message` is addressed to this device alone.
    pub(super) fn is_to_us(&self, message: &CiMessage) -> bool {
        message.destination() == Destination::Muid(self.own)
    }

    /// The message `build` makes from a header of this device on `address` and
    /// the peer's MUID.
    pub(super) fn message(
        &self,
        address: CiDeviceId,
        build: impl FnOnce(CiHeader, Muid) -> CiMessage,
    ) -> CiMessage {
        build(
            CiHeader {
                device_id: address,
                ..CiHeader::new(self.own)
            },
            self.peer,
        )
    }
}
