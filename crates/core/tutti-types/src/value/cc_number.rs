//! [`CCNumber`] — which Control Change controller a message addresses.
//!
//! An *address*, not a measure — so like [`MidiChannel`](super::MidiChannel) it
//! is an integer newtype rather than one of the float-backed units in
//! [`super::units`], and it deliberately does not implement
//! [`Unit`](super::Unit): "controller 74" is not an automatable DSP parameter
//! and must not be reachable through [`Param`](super::Param). (The *value* a CC
//! carries is automatable; the *index* selecting it is not.)
//!
//! # Why a newtype and not `u8`
//!
//! A CC number and a MIDI channel are both small integers that arrive together
//! and mask silently — `handle_cc(cc_num: u8, value: f32, channel: u8)` is a
//! real signature in `tutti-synth`, with two interchangeable `u8`s. Swapping
//! them does not crash: CC 1 on channel 3 becomes CC 3 on channel 1, so the mod
//! wheel silently drives brightness, or a sustain pedal lands on a channel
//! nobody is playing. Both failures are audible-but-plausible, which is the
//! worst kind.
//!
//! `tutti-midi-types` used to carry a `pub type CCNumber = u8` alias next to
//! the `pub type MidiChannel = u8` one, which prevented none of that — an alias
//! is the same type as what it aliases. This is the real one, and that alias
//! now re-exports it, exactly as [`MidiChannel`](super::MidiChannel) already
//! did. It lives here rather than in `tutti-midi-types` because a document has
//! to persist a CC number (a CC automation lane is keyed by one), and that
//! crate carries no serde.
//!
//! # Why 7-bit, in a MIDI 2.0 engine
//!
//! MIDI 2.0 widened the CC *value* to 32 bits but left the *index* at 7 —
//! `MidiMessage::ControlChange { index: u8, value: u32 }` (M2-104 §7.4.6). So
//! `0..=127` remains correct here even though nothing else about a Control
//! Change message is 7-bit any more, and the range is a property of the format
//! rather than a legacy carry-over.

/// One of the 128 MIDI Control Change controllers.
///
/// Always in `0..=127`: [`new`](Self::new) masks rather than rejects, matching
/// what [`MidiEvent::cc`] already does with `cc & 0x7F`.
///
/// [`MidiEvent::cc`]: https://docs.rs/tutti-midi-types
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
// `transparent` for the same reason `MidiChannel` has it: a `CCNumber` is `74`
// on the wire, not `[74]`.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct CCNumber(u8);

impl CCNumber {
    /// The first controller number. Also [`BANK_SELECT`](Self::BANK_SELECT).
    pub const FIRST: CCNumber = CCNumber(0);
    /// The last controller number. (Poly Mode On, in the channel-mode block.)
    pub const LAST: CCNumber = CCNumber(127);
    /// How many Control Change controllers the format defines.
    pub const COUNT: u8 = 128;

    // --- The named roster ---------------------------------------------------
    //
    // These live on the type rather than as loose module constants so that
    // `MOD_WHEEL` cannot be handed to something expecting a channel: that is
    // the whole payoff of the newtype. `tutti_midi_types::cc` re-exports each
    // one under its bare name, so `cc::MOD_WHEEL` still resolves.
    //
    // Numbers are from the MIDI 1.0 Control Change table (and carried forward
    // unchanged by M2-104 §7.4.6).

    // Continuous controllers (MSB).
    /// CC 0.
    pub const BANK_SELECT: CCNumber = CCNumber(0);
    /// CC 1.
    pub const MOD_WHEEL: CCNumber = CCNumber(1);
    /// CC 2.
    pub const BREATH: CCNumber = CCNumber(2);
    /// CC 4.
    pub const FOOT: CCNumber = CCNumber(4);
    /// CC 5.
    pub const PORTAMENTO_TIME: CCNumber = CCNumber(5);
    /// CC 6 — the MSB of an (N)RPN value.
    pub const DATA_ENTRY: CCNumber = CCNumber(6);
    /// CC 7.
    pub const VOLUME: CCNumber = CCNumber(7);
    /// CC 8.
    pub const BALANCE: CCNumber = CCNumber(8);
    /// CC 10.
    pub const PAN: CCNumber = CCNumber(10);
    /// CC 11.
    pub const EXPRESSION: CCNumber = CCNumber(11);

    /// CC 38 — Data Entry LSB, the low 7 bits of an (N)RPN value
    /// (MSB is [`DATA_ENTRY`](Self::DATA_ENTRY)).
    pub const DATA_ENTRY_LSB: CCNumber = CCNumber(38);

    // Sound controllers.
    /// CC 71 — Sound Controller 2 (filter resonance).
    pub const RESONANCE: CCNumber = CCNumber(71);
    /// CC 72 — Sound Controller 3 (release time).
    pub const RELEASE_TIME: CCNumber = CCNumber(72);
    /// CC 73 — Sound Controller 4 (attack time).
    pub const ATTACK_TIME: CCNumber = CCNumber(73);
    /// CC 74 — Sound Controller 5 (brightness). Under MPE this is the *slide*
    /// dimension, which is why it is routed per-note rather than per-channel.
    pub const BRIGHTNESS: CCNumber = CCNumber(74);

    // Switches.
    /// CC 64 — sustain (damper) pedal.
    pub const SUSTAIN: CCNumber = CCNumber(64);
    /// CC 65.
    pub const PORTAMENTO_SWITCH: CCNumber = CCNumber(65);
    /// CC 66.
    pub const SOSTENUTO: CCNumber = CCNumber(66);
    /// CC 67.
    pub const SOFT_PEDAL: CCNumber = CCNumber(67);
    /// CC 68.
    pub const LEGATO: CCNumber = CCNumber(68);

    // Channel mode.
    /// CC 120.
    pub const ALL_SOUND_OFF: CCNumber = CCNumber(120);
    /// CC 121 — Reset All Controllers.
    pub const RESET_ALL: CCNumber = CCNumber(121);
    /// CC 123.
    pub const ALL_NOTES_OFF: CCNumber = CCNumber(123);

    // RPN / NRPN parameter select.
    /// CC 98.
    pub const NRPN_LSB: CCNumber = CCNumber(98);
    /// CC 99.
    pub const NRPN_MSB: CCNumber = CCNumber(99);
    /// CC 100.
    pub const RPN_LSB: CCNumber = CCNumber(100);
    /// CC 101.
    pub const RPN_MSB: CCNumber = CCNumber(101);

    /// Wrap a raw controller number, masking into `0..=127`.
    ///
    /// Masks rather than returning an error because that is what the wire does:
    /// the UMP control field is 7 bits, so a larger value cannot be
    /// represented, and `channel_voice.rs` already `debug_assert`s then masks.
    /// Rejecting here would make this type stricter than the format it
    /// addresses.
    #[inline]
    pub const fn new(raw: u8) -> CCNumber {
        CCNumber(raw & 0x7F)
    }

    /// The raw 0-based controller number, for a UMP constructor or a wire byte.
    #[inline]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<CCNumber> for u8 {
    #[inline]
    fn from(n: CCNumber) -> u8 {
        n.0
    }
}

// Deliberately no `From<u8> for CCNumber`, for the same reason `MidiChannel`
// refuses one: the conversion masks, and a lossy conversion inside a trait that
// promises not to lose data is exactly what `ChannelLayout` refused a reverse
// `From` for. `CCNumber::new` names the masking.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cc_number_is_masked_into_range() {
        assert_eq!(CCNumber::new(0).get(), 0);
        assert_eq!(CCNumber::new(127).get(), 127);
        // 128 wraps to 0 — the same thing `MidiEvent::cc` does, rather than a
        // silently different answer one layer up.
        assert_eq!(CCNumber::new(128).get(), 0);
        assert_eq!(CCNumber::new(255).get(), 127);
    }

    #[test]
    fn the_bounds_are_the_seven_bit_range() {
        assert_eq!(CCNumber::FIRST.get(), 0);
        assert_eq!(CCNumber::LAST.get(), 127);
        assert_eq!(CCNumber::COUNT, 128);
    }

    #[test]
    fn the_default_is_the_first_controller() {
        assert_eq!(CCNumber::default(), CCNumber::FIRST);
    }

    /// The named roster against the MIDI 1.0 Control Change table. A wrong
    /// number here is silent at runtime — the synth just responds to the wrong
    /// knob — so the table is asserted rather than trusted.
    #[test]
    fn the_named_controllers_have_their_spec_numbers() {
        assert_eq!(CCNumber::BANK_SELECT.get(), 0);
        assert_eq!(CCNumber::MOD_WHEEL.get(), 1);
        assert_eq!(CCNumber::BREATH.get(), 2);
        assert_eq!(CCNumber::FOOT.get(), 4);
        assert_eq!(CCNumber::PORTAMENTO_TIME.get(), 5);
        assert_eq!(CCNumber::DATA_ENTRY.get(), 6);
        assert_eq!(CCNumber::VOLUME.get(), 7);
        assert_eq!(CCNumber::BALANCE.get(), 8);
        assert_eq!(CCNumber::PAN.get(), 10);
        assert_eq!(CCNumber::EXPRESSION.get(), 11);
        assert_eq!(CCNumber::DATA_ENTRY_LSB.get(), 38);

        assert_eq!(CCNumber::SUSTAIN.get(), 64);
        assert_eq!(CCNumber::PORTAMENTO_SWITCH.get(), 65);
        assert_eq!(CCNumber::SOSTENUTO.get(), 66);
        assert_eq!(CCNumber::SOFT_PEDAL.get(), 67);
        assert_eq!(CCNumber::LEGATO.get(), 68);

        assert_eq!(CCNumber::RESONANCE.get(), 71);
        assert_eq!(CCNumber::RELEASE_TIME.get(), 72);
        assert_eq!(CCNumber::ATTACK_TIME.get(), 73);
        assert_eq!(CCNumber::BRIGHTNESS.get(), 74);

        assert_eq!(CCNumber::NRPN_LSB.get(), 98);
        assert_eq!(CCNumber::NRPN_MSB.get(), 99);
        assert_eq!(CCNumber::RPN_LSB.get(), 100);
        assert_eq!(CCNumber::RPN_MSB.get(), 101);

        assert_eq!(CCNumber::ALL_SOUND_OFF.get(), 120);
        assert_eq!(CCNumber::RESET_ALL.get(), 121);
        assert_eq!(CCNumber::ALL_NOTES_OFF.get(), 123);
    }

    /// The MSB/LSB pairs are adjacent in the way the (N)RPN decoder assumes:
    /// LSB is the lower number of each pair. Getting this backwards silently
    /// swaps bank and index in every RPN run.
    #[test]
    fn the_rpn_select_pairs_are_lsb_then_msb() {
        assert_eq!(CCNumber::NRPN_LSB.get() + 1, CCNumber::NRPN_MSB.get());
        assert_eq!(CCNumber::RPN_LSB.get() + 1, CCNumber::RPN_MSB.get());
        // Data Entry MSB (6) and its LSB (38) are 32 apart, the standard
        // MSB→LSB offset for the CC 0-31 continuous-controller block.
        assert_eq!(
            CCNumber::DATA_ENTRY.get() + 32,
            CCNumber::DATA_ENTRY_LSB.get()
        );
    }
}
