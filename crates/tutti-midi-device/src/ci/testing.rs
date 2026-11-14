//! Fixtures the unit tests of the CI modules share.

use tutti_ump::{MidiChannel, U7};

use super::{
    Ack, CiAction, CiDeviceId, CiHeader, CiMessage, Decoded, Muid, Nak, ProcessAction,
    ProfileAction, Unread,
};

/// The source of the messages a test decodes.
pub(super) const SRC: u32 = 0x0123_4567;
/// The destination of the messages a test decodes.
pub(super) const DST: u32 = 0x0007_7777;
/// The device under test, in a test with a peer.
pub(super) const OURS: u32 = 0x0100_0001;
/// The peer of the device under test.
pub(super) const PEER: u32 = 0x0200_0002;
/// A third device, neither of the two.
pub(super) const OTHER: u32 = 0x0300_0003;

/// A header from `source`, on `address`.
pub(super) fn header_from(source: u32, address: CiDeviceId) -> CiHeader {
    CiHeader {
        device_id: address,
        ..CiHeader::new(Muid::of(source))
    }
}

/// A header from [`SRC`], on `address`.
pub(super) fn header_on(address: CiDeviceId) -> CiHeader {
    header_from(SRC, address)
}

/// A header from [`SRC`] to the Function Block.
pub(super) fn header() -> CiHeader {
    CiHeader::new(Muid::of(SRC))
}

/// The Destination bytes of [`DST`].
pub(super) fn to_dst() -> [u8; 4] {
    Muid::of(DST).to_bytes()
}

/// `7E <id> 0D <sub> 02 src dst`, the preamble of a Version 2 message from
/// [`SRC`].
pub(super) fn preamble(device_id: u8, sub: u8, destination: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![0x7E, device_id, 0x0D, sub, 0x02];
    bytes.extend(Muid::of(SRC).to_bytes());
    bytes.extend(destination);
    bytes
}

/// The Channel `n`.
pub(super) fn channel(n: u8) -> CiDeviceId {
    CiDeviceId::Channel(MidiChannel::of(n))
}

/// The message `bytes` decode to.
pub(super) fn decoded(bytes: &[u8]) -> CiMessage {
    match CiMessage::decode(bytes) {
        Decoded::Message(message) => message,
        other => panic!("expected a message, got {other:?}"),
    }
}

/// The [`Unread`] `bytes` decode to.
pub(super) fn unread(bytes: &[u8]) -> Unread {
    match CiMessage::decode(bytes) {
        Decoded::Unread(unread) => unread,
        other => panic!("expected an Unread, got {other:?}"),
    }
}

/// A NAK from [`PEER`] to [`OURS`] of `original`, on `address`, with
/// `details`.
pub(super) fn nak_with(address: CiDeviceId, original: u8, status: U7, details: [u8; 5]) -> Decoded {
    Decoded::Message(CiMessage::Nak {
        header: header_from(PEER, address),
        to: Muid::of(OURS),
        nak: Nak::new(U7::of(original), status).with_details(details.map(U7::of)),
    })
}

/// A NAK from [`PEER`] to [`OURS`] of `original`, on `address`.
pub(super) fn nak(address: CiDeviceId, original: u8, status: U7) -> Decoded {
    nak_with(address, original, status, [0; 5])
}

/// A Timeout Wait ACK from [`PEER`] to [`OURS`] of `original`, on `address`,
/// asking `tenths` of a second more, with `details`.
pub(super) fn wait_with(
    address: CiDeviceId,
    original: u8,
    tenths: u8,
    details: [u8; 5],
) -> Decoded {
    Decoded::Message(CiMessage::Ack {
        header: header_from(PEER, address),
        to: Muid::of(OURS),
        ack: Ack::new(U7::of(original), Ack::STATUS_TIMEOUT_WAIT)
            .with_status_data(U7::of(tenths))
            .with_details(details.map(U7::of)),
    })
}

/// A Timeout Wait ACK from [`PEER`] to [`OURS`] of `original`, on `address`,
/// asking `tenths` of a second more.
pub(super) fn wait(address: CiDeviceId, original: u8, tenths: u8) -> Decoded {
    wait_with(address, original, tenths, [0; 5])
}

/// An action that may carry a message to send.
pub(super) trait Action {
    fn message(&self) -> Option<&CiMessage>;
}

macro_rules! action {
    ($($action:ident),*) => {$(
        impl Action for $action {
            fn message(&self) -> Option<&CiMessage> {
                match self {
                    Self::Send(message) => Some(message),
                    _ => None,
                }
            }
        }
    )*};
}
action!(CiAction, ProcessAction, ProfileAction);

/// The messages among `actions`, in order.
pub(super) fn sent<A: Action>(actions: &[A]) -> Vec<&CiMessage> {
    actions.iter().filter_map(Action::message).collect()
}
