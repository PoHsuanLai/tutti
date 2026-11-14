//! What names a byte port: its identity, and the id it is opened by.

use crate::capability::{EndpointInfo, UmpCapability};
use tutti_midi_device::{EndpointId, Identity};

/// Which way a port goes. Inputs and outputs are numbered separately by such an
/// API, so a position alone names neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::backend) enum Side {
    /// A port that sends MIDI to this engine.
    Input,
    /// A port this engine sends MIDI to.
    Output,
}

/// What a driver reports of one port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::backend) struct PortInfo {
    /// The product name.
    pub name: String,
    /// The manufacturer id.
    pub manufacturer: u16,
    /// The product id.
    pub product: u16,
}

/// The parts of an [`EndpointId`] a byte port mints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::backend) struct Slot {
    /// The port's direction.
    pub side: Side,
    /// A digest of the port's [`PortInfo`] when the id was minted, 31 bits.
    pub fingerprint: u32,
    /// The port's position in the OS's list when the id was minted.
    pub index: u32,
}

const OUTPUT_BIT: u64 = 1 << 63;
const FINGERPRINT_BITS: u32 = 31;
const FINGERPRINT_SHIFT: u32 = 32;

/// A 31-bit FNV-1a digest of what the driver reports: the part of an id that
/// says which device was at a position.
pub(in crate::backend) fn fingerprint(info: &PortInfo) -> u32 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let bytes = info
        .name
        .bytes()
        .chain(info.manufacturer.to_le_bytes())
        .chain(info.product.to_le_bytes());
    let hash = bytes.fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME));
    ((hash >> 32) ^ hash) as u32 & ((1 << FINGERPRINT_BITS) - 1)
}

/// The id of the port at `index` in `side`'s list. Carries the position and a
/// digest of the port found there, so an id that outlives its device (the list
/// shifts when one is unplugged) names a position that [`Slot::matches`]
/// refuses, not whatever is there now.
pub(in crate::backend) fn endpoint_id(side: Side, index: u32, info: &PortInfo) -> EndpointId {
    let side = match side {
        Side::Input => 0,
        Side::Output => OUTPUT_BIT,
    };
    EndpointId::from_raw(
        side | u64::from(fingerprint(info)) << FINGERPRINT_SHIFT | u64::from(index),
    )
}

/// The parts of `id`. Every `u64` is some port's id, so this does not fail; a
/// port that is not there is the open's refusal.
pub(in crate::backend) fn slot_of(id: EndpointId) -> Slot {
    let raw = id.raw();
    Slot {
        side: if raw & OUTPUT_BIT == 0 {
            Side::Input
        } else {
            Side::Output
        },
        fingerprint: (raw >> FINGERPRINT_SHIFT) as u32 & ((1 << FINGERPRINT_BITS) - 1),
        index: raw as u32,
    }
}

impl Slot {
    /// Whether `info`, read from the position now, is the port the id was made
    /// for.
    pub(in crate::backend) fn matches(self, info: &PortInfo) -> bool {
        self.fingerprint == fingerprint(info)
    }
}

/// The endpoint of the port at `index` on `side`: a MIDI 1.0 port with no
/// Function Blocks, named as the driver names it.
pub(in crate::backend) fn endpoint(side: Side, index: u32, info: PortInfo) -> EndpointInfo {
    let identity = match side {
        Side::Input => Identity::winmm_input(info.name.as_str(), info.manufacturer, info.product),
        Side::Output => Identity::winmm_output(info.name.as_str(), info.manufacturer, info.product),
    };
    EndpointInfo {
        id: endpoint_id(side, index, &info),
        identity,
        name: info.name,
        capability: UmpCapability::midi1(),
    }
}

/// The endpoints of `side`'s ports, each with its position in the OS's list
/// (a position the OS would not describe is left out of `ports`, and keeps its
/// number from the ones after it).
pub(in crate::backend) fn endpoints(
    side: Side,
    ports: impl IntoIterator<Item = (u32, PortInfo)>,
) -> Vec<EndpointInfo> {
    ports
        .into_iter()
        .map(|(index, info)| endpoint(side, index, info))
        .collect()
}
