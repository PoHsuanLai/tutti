//! data: part of the VST3 event vocabulary.

use super::*;

/// Generic raw-bytes event — used by VST3 for CC, pitch bend, program change,
/// channel pressure, and SysEx.
#[derive(Debug, Clone, Copy)]
pub struct DataEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// Valid byte count in `bytes`.
    pub size: u32,
    /// Data subtype (e.g. `DataEvent::DataTypes::kMidiSysEx`).
    pub event_type: u32,
    /// Inline payload. Only the first `size` bytes are valid.
    pub bytes: [u8; 16],
}

/// A complete SysEx message, too long for [`DataEvent`]'s inline bytes, whose
/// bytes live in the owning event list's buffer, which [`bytes`](Self::bytes)
/// indexes.
///
/// On an input list the list reassembled it from SysEx7 packets
/// (`0xF0 … 0xF7`), and the plugin receives it as a `kDataEvent` with the
/// `kMidiSysEx` subtype. On an output list it is a message the plugin
/// `addEvent`ed, copied whole.
#[derive(Debug, Clone, Copy)]
pub struct SysExEvent {
    /// Bus, frame offset (its `END` packet's) and the `kDataEvent` type.
    pub header: EventHeader,
    /// Where the message lies in the owning event list's SysEx buffer.
    pub bytes: SysExRef,
}
