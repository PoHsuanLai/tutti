//! The MIDI Protocol a UMP stream carries.

/// The MIDI protocol carried on a UMP stream. Wire values per M2-104 §7.1.6.2.
///
/// The Stream Configuration messages are `tutti_ump::Stream` and the
/// endpoint's own state is `tutti-midi-device`'s `Endpoint`; this plain enum
/// stays for `tutti-midi-hardware`'s `UmpCapability`, until that crate takes
/// `tutti_ump::Protocol` with the inbound path of design 017 stage 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Protocol {
    /// MIDI 1.0 Protocol in UMP: 7-bit values in UMP packets. Negotiate this
    /// only for a peer that cannot speak MIDI 2.0: every value crossing the
    /// boundary narrows.
    Midi1 = 1,
    /// The default: this engine is MIDI-2-native.
    #[default]
    Midi2 = 2,
}
