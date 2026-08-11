//! System Real-Time / System Common constructors (M2-104 §7.6 — UMP Message
//! Type 0x1, one 32-bit word): timing clock, transport start/stop/continue,
//! MIDI Time Code quarter-frame, song position/select, active sensing, reset,
//! and tune request. Decode the other direction with [`MidiEvent::message`],
//! which surfaces these as [`MidiMessage`](crate::MidiMessage) System variants.

use midi2::prelude::*;
use tutti_types::MidiGroup;

use super::MidiEvent;

impl MidiEvent {
    /// System Real-Time **Timing Clock** (status 0xF8).
    ///
    /// Sent 24 times per quarter note, so the receiver infers tempo from the
    /// arrival rate rather than from any field — the message carries no data.
    #[inline]
    pub fn timing_clock(group: MidiGroup) -> Self {
        use midi2::system_common::TimingClock;
        let mut m = TimingClock::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        Self::from_ump(0, m.data())
    }

    /// System Real-Time **Start** (status 0xFA): rewind to zero and play.
    ///
    /// Distinct from [`continue_msg`](Self::continue_msg), which resumes in
    /// place — sending the wrong one relocates the transport silently.
    #[inline]
    pub fn start(group: MidiGroup) -> Self {
        use midi2::system_common::Start;
        let mut m = Start::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        Self::from_ump(0, m.data())
    }

    /// System Real-Time **Continue** (status 0xFB): play from the current
    /// position, leaving it where [`stop`](Self::stop) left it.
    ///
    /// Named `continue_msg` because `continue` is a Rust keyword.
    #[inline]
    pub fn continue_msg(group: MidiGroup) -> Self {
        use midi2::system_common::Continue;
        let mut m = Continue::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        Self::from_ump(0, m.data())
    }

    /// System Real-Time **Stop** (status 0xFC): halt, keeping the position.
    ///
    /// Does not silence sounding notes — that is
    /// [`crate::cc::ALL_NOTES_OFF`]'s job.
    #[inline]
    pub fn stop(group: MidiGroup) -> Self {
        use midi2::system_common::Stop;
        let mut m = Stop::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        Self::from_ump(0, m.data())
    }

    /// System Real-Time **Active Sensing** (status 0xFE): a keepalive.
    ///
    /// Once a device has seen one, silence for ~300 ms means the cable is gone
    /// and it should release its notes. A sender that emits this even once has
    /// opted into emitting it continuously.
    #[inline]
    pub fn active_sensing(group: MidiGroup) -> Self {
        use midi2::system_common::ActiveSensing;
        let mut m = ActiveSensing::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        Self::from_ump(0, m.data())
    }

    /// System Real-Time **Reset** (status 0xFF): return the device to power-up
    /// state.
    ///
    /// The bluntest message in MIDI — it drops notes, controllers and program
    /// selection at once.
    #[inline]
    pub fn system_reset(group: MidiGroup) -> Self {
        use midi2::system_common::Reset;
        let mut m = Reset::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        Self::from_ump(0, m.data())
    }

    /// System Common **MIDI Time Code quarter-frame** (status 0xF1).
    ///
    /// `data` is the 7-bit byte: piece number in bits 4..6, nibble in bits 0..3.
    /// One message carries a *quarter* of a timecode, so eight in order make one
    /// position — [`MtcDecoder`](crate::sync::MtcDecoder) does that reassembly.
    /// Masked to 7 bits, so a high bit is dropped rather than corrupting the
    /// piece number.
    #[inline]
    pub fn mtc_quarter_frame(group: MidiGroup, data: u8) -> Self {
        use midi2::system_common::TimeCode;
        let mut m = TimeCode::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        m.set_time_code(u7::new(data & 0x7F));
        Self::from_ump(0, m.data())
    }

    /// System Common **Song Position Pointer** (status 0xF2).
    ///
    /// `position` is in **sixteenth notes** since song start — six timing clocks
    /// each — not beats and not bars. Masked to the wire's 14 bits, so the
    /// addressable range ends at 16383 sixteenths.
    #[inline]
    pub fn song_position(group: MidiGroup, position: u16) -> Self {
        use midi2::system_common::SongPositionPointer;
        let mut m = SongPositionPointer::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        m.set_position(u14::new(position & 0x3FFF));
        Self::from_ump(0, m.data())
    }

    /// System Common **Song Select** (status 0xF3): choose sequence `song`,
    /// masked to 7 bits.
    #[inline]
    pub fn song_select(group: MidiGroup, song: u8) -> Self {
        use midi2::system_common::SongSelect;
        let mut m = SongSelect::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        m.set_song(u7::new(song & 0x7F));
        Self::from_ump(0, m.data())
    }

    /// System Common **Tune Request** (status 0xF6): ask analogue oscillators to
    /// retune.
    ///
    /// Meaningless to a digital instrument, which ignores it.
    #[inline]
    pub fn tune_request(group: MidiGroup) -> Self {
        use midi2::system_common::TuneRequest;
        let mut m = TuneRequest::<[u32; 1]>::new();
        m.set_group(u4::new(group.get()));
        Self::from_ump(0, m.data())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi2::{system_common, UmpMessage};

    #[test]
    fn timing_clock_is_one_word() {
        let ev = MidiEvent::timing_clock(MidiGroup::FIRST);
        assert_eq!(ev.data_words().len(), 1);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        assert!(matches!(
            msg,
            UmpMessage::SystemCommon(system_common::SystemCommon::TimingClock(_))
        ));
    }

    #[test]
    fn song_position_round_trips_14bit() {
        let ev = MidiEvent::song_position(MidiGroup::FIRST, 12345);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        let UmpMessage::SystemCommon(system_common::SystemCommon::SongPositionPointer(m)) = msg
        else {
            panic!("expected SongPositionPointer");
        };
        assert_eq!(u16::from(m.position()), 12345);
    }
}
