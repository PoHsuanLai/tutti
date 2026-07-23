//! System Real-Time / System Common constructors (M2-104 §7.6 — UMP Message
//! Type 0x1, one 32-bit word): timing clock, transport start/stop/continue,
//! MIDI Time Code quarter-frame, song position/select, active sensing, reset,
//! and tune request. Decode the other direction with [`MidiEvent::message`],
//! which surfaces these as [`MidiMessage`](crate::MidiMessage) System variants.

use midi2::prelude::*;

use super::MidiEvent;

impl MidiEvent {
    #[inline]
    pub fn timing_clock(group: u8) -> Self {
        use midi2::system_common::TimingClock;
        let mut m = TimingClock::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn start(group: u8) -> Self {
        use midi2::system_common::Start;
        let mut m = Start::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn continue_msg(group: u8) -> Self {
        use midi2::system_common::Continue;
        let mut m = Continue::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn stop(group: u8) -> Self {
        use midi2::system_common::Stop;
        let mut m = Stop::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn active_sensing(group: u8) -> Self {
        use midi2::system_common::ActiveSensing;
        let mut m = ActiveSensing::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn system_reset(group: u8) -> Self {
        use midi2::system_common::Reset;
        let mut m = Reset::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn mtc_quarter_frame(group: u8, data: u8) -> Self {
        use midi2::system_common::TimeCode;
        let mut m = TimeCode::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_time_code(u7::new(data & 0x7F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn song_position(group: u8, position: u16) -> Self {
        use midi2::system_common::SongPositionPointer;
        let mut m = SongPositionPointer::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_position(u14::new(position & 0x3FFF));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn song_select(group: u8, song: u8) -> Self {
        use midi2::system_common::SongSelect;
        let mut m = SongSelect::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_song(u7::new(song & 0x7F));
        Self::from_ump(0, m.data())
    }

    #[inline]
    pub fn tune_request(group: u8) -> Self {
        use midi2::system_common::TuneRequest;
        let mut m = TuneRequest::<[u32; 1]>::new();
        m.set_group(u4::new(group & 0x0F));
        Self::from_ump(0, m.data())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi2::{system_common, UmpMessage};

    #[test]
    fn timing_clock_is_one_word() {
        let ev = MidiEvent::timing_clock(0);
        assert_eq!(ev.data_words().len(), 1);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        assert!(matches!(
            msg,
            UmpMessage::SystemCommon(system_common::SystemCommon::TimingClock(_))
        ));
    }

    #[test]
    fn song_position_round_trips_14bit() {
        let ev = MidiEvent::song_position(0, 12345);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        let UmpMessage::SystemCommon(system_common::SystemCommon::SongPositionPointer(m)) = msg
        else {
            panic!("expected SongPositionPointer");
        };
        assert_eq!(u16::from(m.position()), 12345);
    }
}
