//! The group a UMP names.

use tutti_ump::{MidiGroup, Ump};

/// The group a packet is on (the low nibble of its first byte), for the
/// reassemblers and the MPE ingest, which key on it whatever the message type.
pub(crate) fn group_of(ump: &Ump) -> MidiGroup {
    MidiGroup::masked(((ump.0[0] >> 24) & 0x0F) as u8)
}
