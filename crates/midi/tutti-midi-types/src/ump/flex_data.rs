//! Flex Data (UMP Message Type 0xD).
//!
//! Flex Data messages carry musical metadata that MIDI 1.0 kept in SMF "meta
//! events" — tempo, time signature, key signature, metronome, chord names, and
//! text/lyrics (M2-104 §7.5). They are group-scoped (no channel). The
//! constructors below cover the fixed-size subset a DAW clip needs; the
//! variable-length text/chord messages are not yet exposed.

use midi2::prelude::*;

use super::MidiEvent;

/// 10-nanosecond units in one minute — the numerator relating BPM to the Flex
/// Data Set Tempo wire field (60 s = 6e9 × 10 ns). `bpm × ten_ns_per_qn = 6e9`.
const TEN_NS_UNITS_PER_MINUTE: f64 = 600_000_000.0;

/// Beats-per-minute → the Set Tempo wire field (10-ns units per quarter note).
/// `0` (an invalid rate) maps to `0`, which the inverse reports as "no tempo".
#[inline]
pub const fn bpm_to_ten_ns_per_quarter(bpm: f64) -> u32 {
    if bpm > 0.0 {
        (TEN_NS_UNITS_PER_MINUTE / bpm) as u32
    } else {
        0
    }
}

/// The Set Tempo wire field (10-ns units per quarter note) → beats-per-minute.
/// `0` (no valid tempo) yields `None`. Inverse of [`bpm_to_ten_ns_per_quarter`].
#[inline]
pub const fn ten_ns_per_quarter_to_bpm(ten_ns_per_qn: u32) -> Option<f64> {
    if ten_ns_per_qn != 0 {
        Some(TEN_NS_UNITS_PER_MINUTE / ten_ns_per_qn as f64)
    } else {
        None
    }
}

impl MidiEvent {
    /// Flex Data **Set Tempo** from beats-per-minute. See
    /// [`bpm_to_ten_ns_per_quarter`] for the BPM ↔ wire-field conversion.
    #[inline]
    pub fn flex_set_tempo(group: u8, bpm: f64) -> Self {
        use midi2::flex_data::SetTempo;
        let mut m = SetTempo::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_number_of_10_nanosecond_units_per_quarter_note(bpm_to_ten_ns_per_quarter(bpm));
        Self::from_ump(0, m.data())
    }

    /// Flex Data **Set Time Signature**. `numerator`/`denominator` are the beats
    /// per bar and the beat unit; `num_32nd_notes` is the number of 1/32 notes
    /// per quarter note (usually 8).
    #[inline]
    pub fn flex_set_time_signature(
        group: u8,
        numerator: u8,
        denominator: u8,
        num_32nd_notes: u8,
    ) -> Self {
        use midi2::flex_data::SetTimeSignature;
        let mut m = SetTimeSignature::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_numerator(numerator);
        m.set_denominator(denominator);
        m.set_number_of_32nd_notes(num_32nd_notes);
        Self::from_ump(0, m.data())
    }

    /// Flex Data **Set Metronome**. `clocks_per_click` = MIDI clocks per primary
    /// click; the three bar accents mark which subdivisions are accented.
    #[inline]
    pub fn flex_set_metronome(
        group: u8,
        clocks_per_click: u8,
        bar_accent1: u8,
        bar_accent2: u8,
        bar_accent3: u8,
    ) -> Self {
        use midi2::flex_data::SetMetronome;
        let mut m = SetMetronome::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_number_of_clocks_per_primary_click(clocks_per_click);
        m.set_bar_accent1(bar_accent1);
        m.set_bar_accent2(bar_accent2);
        m.set_bar_accent3(bar_accent3);
        Self::from_ump(0, m.data())
    }
}

/// Recover BPM from a Flex Data Set Tempo [`MidiEvent`], or `None` if `event`
/// isn't one. Inverse of [`MidiEvent::flex_set_tempo`].
pub fn flex_tempo_bpm(event: &MidiEvent) -> Option<f64> {
    use midi2::flex_data::FlexData;
    use midi2::UmpMessage;
    let UmpMessage::FlexData(FlexData::SetTempo(m)) =
        UmpMessage::try_from(event.data_words()).ok()?
    else {
        return None;
    };
    ten_ns_per_quarter_to_bpm(m.number_of_10_nanosecond_units_per_quarter_note())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_conversion_kernels_round_trip() {
        for bpm in [60.0, 120.0, 140.0, 174.0] {
            let field = bpm_to_ten_ns_per_quarter(bpm);
            let back = ten_ns_per_quarter_to_bpm(field).expect("nonzero");
            assert!((back - bpm).abs() < 0.05, "bpm {bpm} → {back}");
        }
        // An invalid rate maps to the "no tempo" sentinel, both directions.
        assert_eq!(bpm_to_ten_ns_per_quarter(0.0), 0);
        assert_eq!(ten_ns_per_quarter_to_bpm(0), None);
    }

    #[test]
    fn flex_set_tempo_round_trips_bpm() {
        for bpm in [60.0, 120.0, 140.0, 174.0] {
            let ev = MidiEvent::flex_set_tempo(0, bpm);
            let decoded = flex_tempo_bpm(&ev).expect("is a Set Tempo");
            // 10ns-per-qn is integer-quantized, so allow a tiny epsilon.
            assert!((decoded - bpm).abs() < 0.05, "bpm {bpm} → {decoded}");
        }
    }

    #[test]
    fn flex_set_time_signature_decodes_via_midi2() {
        use midi2::flex_data::FlexData;
        let ev = MidiEvent::flex_set_time_signature(0, 7, 8, 8);
        match midi2::UmpMessage::try_from(ev.data_words()).unwrap() {
            midi2::UmpMessage::FlexData(FlexData::SetTimeSignature(m)) => {
                assert_eq!(m.numerator(), 7);
                assert_eq!(m.denominator(), 8);
                assert_eq!(m.number_of_32nd_notes(), 8);
            }
            other => panic!("expected SetTimeSignature, got {other:?}"),
        }
    }

    #[test]
    fn flex_tempo_bpm_rejects_non_tempo() {
        assert!(flex_tempo_bpm(&MidiEvent::note_on(0, 0, 60, 0x8000)).is_none());
    }
}
