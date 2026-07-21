//! Channel Voice constructors — Note On/Off, CC, pitch bend, pressure, program
//! change, and the MIDI 2.0 per-note messages (per-note pitch bend, per-note
//! controllers, per-note management). All build MIDI 2.0 Channel Voice 2 (UMP
//! type 0x4, two words) via the typed `midi2` messages.
//!
//! Out-of-range field values are masked to their valid MIDI width so the emitted
//! UMP is always well-formed (`note & 0x7F`, `channel & 0x0F`, `group & 0x0F`).
//! Because masking turns an overflow into a *plausible wrong* value (e.g. note
//! 200 → 72) rather than an error, each constructor also `debug_assert!`s the
//! field is in range: an accidental octave-overflow or a swapped channel/note
//! argument then fails loudly in tests and debug builds, while release builds
//! keep the safe masking.

use midi2::prelude::*;

use super::MidiEvent;

/// Debug-assert that MIDI field widths hold, so a caller's out-of-range value (or
/// a transposed argument) is caught in tests rather than silently masked to a
/// plausible-but-wrong value. No-op in release, where masking keeps UMP valid.
#[inline]
fn debug_assert_fields(group: u8, channel: u8, note: Option<u8>) {
    debug_assert!(group < 16, "MIDI group {group} out of range (0..16)");
    debug_assert!(channel < 16, "MIDI channel {channel} out of range (0..16)");
    if let Some(note) = note {
        debug_assert!(note < 128, "MIDI note {note} out of range (0..128)");
    }
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
    pub fn note_on(group: u8, channel: u8, note: u8, velocity: u16) -> Self {
        use midi2::channel_voice2::NoteOn;
        debug_assert_fields(group, channel, Some(note));
        let mut m = NoteOn::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        Self::from_ump(0, m.data())
    }

    /// Note-on from a 7-bit MIDI 1.0 velocity (0-127), widened to the 16-bit
    /// MIDI 2.0 field by left-shifting 9 bits.
    ///
    /// This is the shift-widen many callers were open-coding as
    /// `note_on(g, c, n, (vel as u16) << 9)`. Note it is *not* the spec
    /// Min-Center-Max upconvert (`convert::midi1_velocity_to_midi2`): `<< 9`
    /// maps 127 → 65024, not 65535. Preserved here verbatim so the helper is a
    /// drop-in for the existing call sites; reach for `note_on` +
    /// `midi1_velocity_to_midi2` when exact full-range fidelity matters.
    #[inline]
    pub fn note_on_7bit(group: u8, channel: u8, note: u8, velocity_u7: u8) -> Self {
        Self::note_on(group, channel, note, (velocity_u7 as u16) << 9)
    }

    #[inline]
    pub fn note_off(group: u8, channel: u8, note: u8, velocity: u16) -> Self {
        use midi2::channel_voice2::NoteOff;
        debug_assert_fields(group, channel, Some(note));
        let mut m = NoteOff::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn cc(group: u8, channel: u8, cc: u8, value: u32) -> Self {
        use midi2::channel_voice2::ControlChange;
        debug_assert_fields(group, channel, None);
        debug_assert!(cc < 128, "MIDI CC {cc} out of range (0..128)");
        let mut m = ControlChange::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_control(u7::new(cc & 0x7F));
        m.set_control_change_data(value);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn pitch_bend(group: u8, channel: u8, bend: u32) -> Self {
        use midi2::channel_voice2::ChannelPitchBend;
        debug_assert_fields(group, channel, None);
        let mut m = ChannelPitchBend::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_pitch_bend_data(bend);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn per_note_pitch_bend(group: u8, channel: u8, note: u8, bend: u32) -> Self {
        use midi2::channel_voice2::PerNotePitchBend;
        debug_assert_fields(group, channel, Some(note));
        let mut m = PerNotePitchBend::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_pitch_bend_data(bend);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn program_change(group: u8, channel: u8, program: u8, bank: Option<u16>) -> Self {
        use midi2::channel_voice2::ProgramChange;
        debug_assert_fields(group, channel, None);
        debug_assert!(program < 128, "MIDI program {program} out of range (0..128)");
        let mut m = ProgramChange::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_program(u7::new(program & 0x7F));
        m.set_bank(bank.map(|b| u14::new(b & 0x3FFF)));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn channel_pressure(group: u8, channel: u8, pressure: u32) -> Self {
        use midi2::channel_voice2::ChannelPressure;
        debug_assert_fields(group, channel, None);
        let mut m = ChannelPressure::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_channel_pressure_data(pressure);
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn poly_pressure(group: u8, channel: u8, note: u8, pressure: u32) -> Self {
        use midi2::channel_voice2::KeyPressure;
        debug_assert_fields(group, channel, Some(note));
        let mut m = KeyPressure::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
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
        group: u8,
        channel: u8,
        note: u8,
        index: u8,
        value: u32,
        registered: bool,
    ) -> Self {
        debug_assert_fields(group, channel, Some(note));
        // `index` is a full 8-bit assignable index — no mask, no assert.
        // UMP type 0x4, status nibble 0x0 (registered) or 0x1 (assignable).
        let opcode: u32 = if registered { 0x0 } else { 0x1 };
        let w0 = (0x4u32 << 28)
            | (((group & 0x0F) as u32) << 24)
            | (opcode << 20)
            | (((channel & 0x0F) as u32) << 16)
            | (((note & 0x7F) as u32) << 8)
            | (index as u32);
        Self::from_ump(0, &[w0, value])
    }

    /// Per-Note Management (M2-104 §7.4.5). `detach` = D (detach per-note
    /// controllers from prior notes on this note number); `reset` = S (reset
    /// per-note controllers to defaults). `D=0, S=0` has no defined function.
    #[inline]
    pub fn per_note_management(
        group: u8,
        channel: u8,
        note: u8,
        detach: bool,
        reset: bool,
    ) -> Self {
        use midi2::channel_voice2::PerNoteManagement;
        debug_assert_fields(group, channel, Some(note));
        let mut m = PerNoteManagement::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
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
        let ev = MidiEvent::note_on(0, 5, 60, 0x8000);
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
        let ev = MidiEvent::cc(0, 2, 74, 0xDEAD_BEEF);
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
        let ev = MidiEvent::per_note_management(0, 4, 60, true, false);
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
