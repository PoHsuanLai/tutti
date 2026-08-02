//! Channel Voice constructors — Note On/Off, CC, pitch bend, pressure, program
//! change, and the MIDI 2.0 per-note messages (per-note pitch bend, per-note
//! controllers, per-note management). All build MIDI 2.0 Channel Voice 2 (UMP
//! type 0x4, two words) via the typed `midi2` messages.
//!
//! Out-of-range field values are masked to their valid MIDI width so the emitted
//! UMP is always well-formed. The masking is now split by who owns the field:
//!
//! - **Group and channel** arrive as [`MidiGroup`] and [`MidiChannel`], which
//!   mask on construction. They cannot be out of range here, so there is
//!   nothing left to assert about them — and, more to the point, they cannot be
//!   *transposed* either. That was the failure a `debug_assert` could never
//!   catch: both fields are 4 bits, so a swap of two in-range values passes
//!   every width check and emits a well-formed packet on the wrong cable
//!   addressing the wrong voice. It is now a compile error.
//! - **Note, CC, and program** are still 7-bit `u8`s and are still masked
//!   (`& 0x7F`) with a `debug_assert!` in front. Masking turns an overflow into
//!   a *plausible wrong* value (note 200 → 72) rather than an error, so the
//!   assert makes an accidental octave-overflow fail loudly in tests and debug
//!   builds while release builds keep the safe masking.

use midi2::prelude::*;
use tutti_types::{MidiChannel, MidiGroup};

use super::MidiEvent;

/// Debug-assert that the 7-bit note number is in range, so a caller's
/// out-of-range value is caught in tests rather than silently masked to a
/// plausible-but-wrong value (note 200 → 72). No-op in release, where the
/// `& 0x7F` at the call site keeps the emitted UMP valid.
///
/// Group and channel used to be checked here too. They no longer can be:
/// [`MidiGroup`] and [`MidiChannel`] mask on construction, so by the time one
/// reaches a constructor it is in range by type. That is a strict improvement
/// even though it removes two assertions — the assertions could only catch an
/// out-of-*range* group or channel, never a group and channel *transposed*,
/// which is the error that actually happens and which both fields being 4 bits
/// made invisible to any width check.
#[inline]
fn debug_assert_note(note: u8) {
    debug_assert!(note < 128, "MIDI note {note} out of range (0..128)");
}

impl MidiEvent {
    /// A MIDI 2.0 Channel Voice **Note On**. `velocity` is the full **16-bit**
    /// MIDI-2 value (`0x8000` ≈ mezzo-forte, `0xFFFF` = max) — *not* a 7-bit
    /// 0–127. If you have a 7-bit velocity from a MIDI-1 source, use
    /// [`note_on_7bit`](Self::note_on_7bit) (or widen via
    /// [`convert::midi1_velocity_to_midi2`](crate::convert::midi1_velocity_to_midi2))
    /// so `100` doesn't become a near-silent `100/65535`.
    ///
    /// This builds the *event*. To **deliver** a note to a running unit without
    /// hand-building one, use `tutti_midi_runtime::MidiSender::note_on` /
    /// `MidiBus::note_on`, which take a 7-bit velocity and push for you.
    #[inline]
    pub fn note_on(group: MidiGroup, channel: MidiChannel, note: u8, velocity: u16) -> Self {
        use midi2::channel_voice2::NoteOn;
        debug_assert_note(note);
        let mut m = NoteOn::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        Self::from_ump(0, m.data())
    }

    /// Note-on from a 7-bit MIDI 1.0 velocity (0-127), widened to the 16-bit
    /// MIDI 2.0 field with the spec Min-Center-Max scaler
    /// ([`convert::midi1_velocity_to_midi2`](crate::convert::midi1_velocity_to_midi2)).
    ///
    /// This is the one MIDI-1→2 upconvert used across the engine's edges, so a
    /// note delivered via this helper carries the same native 16-bit value as one
    /// promoted from the wire — 0 → 0, 64 → 0x8000, 127 → 65535. (It previously
    /// used a lossy `<< 9` that mapped 127 → 65024; unified here to the spec
    /// scaler so the default delivery path isn't the lossy one.)
    #[inline]
    pub fn note_on_7bit(group: MidiGroup, channel: MidiChannel, note: u8, velocity_u7: u8) -> Self {
        Self::note_on(
            group,
            channel,
            note,
            crate::convert::midi1_velocity_to_midi2(velocity_u7),
        )
    }

    #[inline]
    pub fn note_off(group: MidiGroup, channel: MidiChannel, note: u8, velocity: u16) -> Self {
        use midi2::channel_voice2::NoteOff;
        debug_assert_note(note);
        let mut m = NoteOff::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn cc(group: MidiGroup, channel: MidiChannel, cc: u8, value: u32) -> Self {
        use midi2::channel_voice2::ControlChange;
        debug_assert!(cc < 128, "MIDI CC {cc} out of range (0..128)");
        let mut m = ControlChange::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_control(u7::new(cc & 0x7F));
        m.set_control_change_data(value);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn pitch_bend(group: MidiGroup, channel: MidiChannel, bend: u32) -> Self {
        use midi2::channel_voice2::ChannelPitchBend;
        let mut m = ChannelPitchBend::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_pitch_bend_data(bend);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn per_note_pitch_bend(
        group: MidiGroup,
        channel: MidiChannel,
        note: u8,
        bend: u32,
    ) -> Self {
        use midi2::channel_voice2::PerNotePitchBend;
        debug_assert_note(note);
        let mut m = PerNotePitchBend::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_pitch_bend_data(bend);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn program_change(
        group: MidiGroup,
        channel: MidiChannel,
        program: u8,
        bank: Option<u16>,
    ) -> Self {
        use midi2::channel_voice2::ProgramChange;
        debug_assert!(
            program < 128,
            "MIDI program {program} out of range (0..128)"
        );
        let mut m = ProgramChange::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_program(u7::new(program & 0x7F));
        m.set_bank(bank.map(|b| u14::new(b & 0x3FFF)));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn channel_pressure(group: MidiGroup, channel: MidiChannel, pressure: u32) -> Self {
        use midi2::channel_voice2::ChannelPressure;
        let mut m = ChannelPressure::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_channel_pressure_data(pressure);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn poly_pressure(group: MidiGroup, channel: MidiChannel, note: u8, pressure: u32) -> Self {
        use midi2::channel_voice2::KeyPressure;
        debug_assert_note(note);
        let mut m = KeyPressure::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_key_pressure_data(pressure);
        Self::from_ump(0, m.data())
    }

    /// Per-Note Controller: `registered=true` for Registered (opcode 0x0),
    /// `registered=false` for Assignable (opcode 0x1).
    ///
    /// `index` accepts the full 0-255 range; midi2's semantic `Controller`
    /// enum only covers a spec-mandated subset, so the UMP word is written
    /// directly.
    #[inline]
    pub fn per_note_controller(
        group: MidiGroup,
        channel: MidiChannel,
        note: u8,
        index: u8,
        value: u32,
        registered: bool,
    ) -> Self {
        debug_assert_note(note);
        // `index` is a full 8-bit assignable index — no mask, no assert.
        // UMP type 0x4, status nibble 0x0 (registered) or 0x1 (assignable).
        let opcode: u32 = if registered { 0x0 } else { 0x1 };
        let w0 = (0x4u32 << 28)
            | ((group.get() as u32) << 24)
            | (opcode << 20)
            | ((channel.get() as u32) << 16)
            | (((note & 0x7F) as u32) << 8)
            | (index as u32);
        Self::from_ump(0, &[w0, value])
    }

    /// Per-Note Management (M2-104 §7.4.5). `detach` = D (detach per-note
    /// controllers from prior notes on this note number); `reset` = S (reset
    /// per-note controllers to defaults). `D=0, S=0` has no defined function.
    #[inline]
    pub fn per_note_management(
        group: MidiGroup,
        channel: MidiChannel,
        note: u8,
        detach: bool,
        reset: bool,
    ) -> Self {
        use midi2::channel_voice2::PerNoteManagement;
        debug_assert_note(note);
        let mut m = PerNoteManagement::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_detach(detach);
        m.set_reset(reset);
        Self::from_ump(0, m.data())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi2::{channel_voice2, UmpMessage};

    #[test]
    fn note_on_decodes_via_midi2() {
        let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(5), 60, 0x8000);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        match msg {
            UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::NoteOn(m)) => {
                assert_eq!(u8::from(m.channel()), 5);
                assert_eq!(u8::from(m.note_number()), 60);
                assert_eq!(m.velocity(), 0x8000);
            }
            _ => panic!("expected CV2 NoteOn, got {msg:?}"),
        }
    }

    #[test]
    fn cc_decodes_via_midi2() {
        let ev = MidiEvent::cc(MidiGroup::FIRST, MidiChannel::new(2), 74, 0xDEAD_BEEF);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::ControlChange(m)) = msg else {
            panic!("expected CV2 ControlChange");
        };
        assert_eq!(u8::from(m.channel()), 2);
        assert_eq!(u8::from(m.control()), 74);
        assert_eq!(m.control_change_data(), 0xDEAD_BEEF);
    }

    #[test]
    fn per_note_management_decodes_via_midi2() {
        let ev =
            MidiEvent::per_note_management(MidiGroup::FIRST, MidiChannel::new(4), 60, true, false);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::PerNoteManagement(m)) = msg
        else {
            panic!("expected CV2 PerNoteManagement");
        };
        assert_eq!(u8::from(m.channel()), 4);
        assert_eq!(u8::from(m.note_number()), 60);
        assert!(m.detach());
        assert!(!m.reset());
    }
}
