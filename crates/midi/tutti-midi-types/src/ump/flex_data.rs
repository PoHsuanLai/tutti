//! Flex Data (UMP Message Type 0xD).
//!
//! Flex Data messages carry musical metadata that MIDI 1.0 kept in SMF "meta
//! events" — tempo, time signature, key signature, metronome, chord names, and
//! text/lyrics (M2-104 §7.5). They are group-scoped (no channel).
//!
//! Three shapes live here, all following the [`super`] constructor idiom:
//! - **Fixed-size setup/performance** — tempo, time signature, metronome, key
//!   signature, chord name: inherent `MidiEvent::flex_set_*` methods over a
//!   `[u32; 4]` midi2 builder, with a matching decoder.
//! - **Variable-length text/metadata** — the ~19 UTF-8 messages (project /
//!   composition / clip name, lyrics, copyright, performer names, …): the free
//!   function [`push_flex_text`] (a text may span several 128-bit packets, so it
//!   appends one or more [`MidiEvent`]s to an `&mut Vec`, mirroring
//!   [`super::endpoint_name`]) plus the [`flex_text`] decoder.
//!
//! The midi2 field enums (`Tonic`, chord/key `SharpsFlats`, `ChordType`,
//! `Alteration`) are re-exported here so consumers name them as
//! `tutti_midi_types::ump::Tonic` without importing `midi2` — the same courtesy
//! [`NoteAttribute`](crate::NoteAttribute) extends for note attributes.

use midi2::prelude::*;

use super::MidiEvent;

/// The tonic (root pitch class) of a key signature or chord (M2-104 §7.5.9/§7.5.10).
/// Re-export of midi2's `flex_data::Tonic` so consumers avoid a `midi2` import.
pub type Tonic = midi2::flex_data::Tonic;

/// Key-signature accidental count: `Flats(n)` / `Sharps(n)` / `NonStandard`
/// (M2-104 §7.5.9). Re-export of midi2's `SetKeySignature`-flavored `SharpsFlats`
/// (distinct from the chord-name flavor, [`ChordSharpsFlats`]).
pub type KeySharpsFlats = midi2::flex_data::SetKeySignatureSharpsFlats;

/// Per-note accidental of a chord tonic/bass: `DoubleSharp` … `DoubleFlat`
/// (M2-104 §7.5.10). Re-export of midi2's `SetChordName`-flavored `SharpsFlats`.
pub type ChordSharpsFlats = midi2::flex_data::SetChordNameSharpsFlats;

/// A chord quality (Major, Minor7th, Dominant9th, …) (M2-104 §7.5.10).
/// Re-export of midi2's `flex_data::ChordType`.
pub type ChordType = midi2::flex_data::ChordType;

/// A chord alteration (`Add` / `Subtract` / `Raise` / `Lower` a scale degree)
/// (M2-104 §7.5.10). Re-export of midi2's `flex_data::Alteration`.
pub type Alteration = midi2::flex_data::Alteration;

/// A fully-specified chord for a **Set Chord Name** message (M2-104 §7.5.10): a
/// tonic (pitch class + accidental + quality with up to four alterations) and an
/// optional bass note (its own pitch class + accidental + quality + up to two
/// alterations). `bass == None` writes the "same as tonic / no separate bass"
/// encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChordName {
    pub tonic: Tonic,
    pub tonic_sharps_flats: ChordSharpsFlats,
    pub chord_type: ChordType,
    pub alterations: [Option<Alteration>; 4],
    pub bass: Option<ChordBass>,
}

/// The bass half of a [`ChordName`] (the "/G" in "Cmaj7/G").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChordBass {
    pub note: Tonic,
    pub sharps_flats: ChordSharpsFlats,
    pub chord_type: ChordType,
    pub alterations: [Option<Alteration>; 2],
}

/// The three bar-accent positions of a Flex Data **Set Metronome** message: each
/// marks a subdivision (in clicks) that receives an accent, `0` meaning "none".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BarAccents {
    pub primary: u8,
    pub secondary: u8,
    pub tertiary: u8,
}

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
    /// click; `accents` marks which bar subdivisions are accented.
    #[inline]
    pub fn flex_set_metronome(group: u8, clocks_per_click: u8, accents: BarAccents) -> Self {
        use midi2::flex_data::SetMetronome;
        let mut m = SetMetronome::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_number_of_clocks_per_primary_click(clocks_per_click);
        m.set_bar_accent1(accents.primary);
        m.set_bar_accent2(accents.secondary);
        m.set_bar_accent3(accents.tertiary);
        Self::from_ump(0, m.data())
    }

    /// Flex Data **Set Key Signature** (M2-104 §7.5.9). `tonic` is the key's
    /// root pitch class; `sharps_flats` its accidental count. Group-scoped.
    #[inline]
    pub fn flex_set_key_signature(group: u8, tonic: Tonic, sharps_flats: KeySharpsFlats) -> Self {
        use midi2::flex_data::SetKeySignature;
        let mut m = SetKeySignature::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_tonic(tonic);
        m.set_sharps_flats(sharps_flats);
        Self::from_ump(0, m.data())
    }

    /// Flex Data **Set Chord Name** (M2-104 §7.5.10) from a [`ChordName`]. Writes
    /// the tonic (pitch class + accidental + quality + up to four alterations) and
    /// — when `chord.bass` is `Some` — the bass note likewise. Group-scoped.
    #[inline]
    pub fn flex_set_chord_name(group: u8, chord: &ChordName) -> Self {
        use midi2::flex_data::SetChordName;
        let mut m = SetChordName::<[u32; 4]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_tonic_sharps_flats(chord.tonic_sharps_flats);
        m.set_tonic(chord.tonic);
        m.set_chord_type(chord.chord_type);
        m.set_chord_alteration1(chord.alterations[0]);
        m.set_chord_alteration2(chord.alterations[1]);
        m.set_chord_alteration3(chord.alterations[2]);
        m.set_chord_alteration4(chord.alterations[3]);
        if let Some(bass) = chord.bass {
            m.set_bass_sharps_flats(bass.sharps_flats);
            m.set_bass_note(bass.note);
            m.set_bass_chord_type(bass.chord_type);
            m.set_bass_alteration1(bass.alterations[0]);
            m.set_bass_alteration2(bass.alterations[1]);
        }
        Self::from_ump(0, m.data())
    }
}

/// Recover the [`KeySharpsFlats`] tonic pair from a Flex Data Set Key Signature
/// [`MidiEvent`], or `None` if `event` isn't one. Inverse of
/// [`MidiEvent::flex_set_key_signature`].
pub fn flex_key_signature(event: &MidiEvent) -> Option<(Tonic, KeySharpsFlats)> {
    use midi2::flex_data::FlexData;
    use midi2::UmpMessage;
    let UmpMessage::FlexData(FlexData::SetKeySignature(m)) =
        UmpMessage::try_from(event.data_words()).ok()?
    else {
        return None;
    };
    Some((m.tonic(), m.sharps_flats()))
}

/// Recover a [`ChordName`] from a Flex Data Set Chord Name [`MidiEvent`], or
/// `None` if `event` isn't one. Inverse of [`MidiEvent::flex_set_chord_name`].
/// The bass half is reported only when the message carries a bass chord type
/// other than the "no bass" sentinel (`ChordType::ClearChord`).
pub fn flex_chord_name(event: &MidiEvent) -> Option<ChordName> {
    use midi2::flex_data::{ChordType, FlexData};
    use midi2::UmpMessage;
    let UmpMessage::FlexData(FlexData::SetChordName(m)) =
        UmpMessage::try_from(event.data_words()).ok()?
    else {
        return None;
    };
    let bass = if m.bass_chord_type() == ChordType::ClearChord {
        None
    } else {
        Some(ChordBass {
            note: m.bass_note(),
            sharps_flats: m.bass_sharps_flats(),
            chord_type: m.bass_chord_type(),
            alterations: [m.bass_alteration1(), m.bass_alteration2()],
        })
    };
    Some(ChordName {
        tonic: m.tonic(),
        tonic_sharps_flats: m.tonic_sharps_flats(),
        chord_type: m.chord_type(),
        alterations: [
            m.chord_alteration1(),
            m.chord_alteration2(),
            m.chord_alteration3(),
            m.chord_alteration4(),
        ],
        bass,
    })
}

/// Which Flex Data text/metadata message a UTF-8 string is (M2-104 §7.5.4–7.5.8).
/// Selects the wire message [`push_flex_text`] emits and [`flex_text`] recovers.
///
/// The two banks: `Metadata*` variants are Metadata-Text-bank messages carried by
/// a project/clip (names, credits, dates); `Performance*`/`Lyrics*` variants are
/// Performance-Text-bank messages timed within the stream (lyrics, ruby).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FlexTextKind {
    /// Free metadata text with no specific status (Metadata bank, status 0x0).
    UnknownMetadata,
    ProjectName,
    CompositionName,
    MidiClipName,
    CopyrightNotice,
    ComposerName,
    LyricistName,
    ArrangerName,
    PublisherName,
    PrimaryPerformerName,
    AccompanyingPerformerName,
    RecordingDate,
    RecordingLocation,
    /// Free performance text with no specific status (Performance bank, status 0x0).
    UnknownPerformance,
    Lyrics,
    LyricsLanguage,
    Ruby,
    RubyLanguage,
}

/// Emit a Flex Data text/metadata message of `kind` carrying `text`, appending
/// one or more [`MidiEvent`]s (one per 128-bit packet) to `out`. UTF-8 text
/// longer than one packet is fragmented by midi2's Format field, mirroring
/// [`super::endpoint_name`]. Inverse: [`flex_text`] on each reassembled message.
pub fn push_flex_text(kind: FlexTextKind, text: &str, group: u8, out: &mut Vec<MidiEvent>) {
    use midi2::Data;

    /// One arm per text message type: build the midi2 message into a growable
    /// buffer, set group + text, then split its words into 4-word packets. Most
    /// messages carry the string in a `text` field; `ComposerName` alone names it
    /// `name`, so its setter is passed explicitly.
    macro_rules! emit {
        ($ty:ty) => {
            emit!($ty, set_text)
        };
        ($ty:ty, $setter:ident) => {{
            let mut m = <$ty>::new();
            m.set_group(u4::new(group & 0x0F));
            m.$setter(text);
            for packet in m.data().chunks(4) {
                out.push(MidiEvent::from_ump(0, packet));
            }
        }};
    }
    use midi2::flex_data::*;
    match kind {
        FlexTextKind::UnknownMetadata => emit!(UnknownMetadataText::<Vec<u32>>),
        FlexTextKind::ProjectName => emit!(ProjectName::<Vec<u32>>),
        FlexTextKind::CompositionName => emit!(CompositionName::<Vec<u32>>),
        FlexTextKind::MidiClipName => emit!(MidiClipName::<Vec<u32>>),
        FlexTextKind::CopyrightNotice => emit!(CopyrightNotice::<Vec<u32>>),
        FlexTextKind::ComposerName => emit!(ComposerName::<Vec<u32>>, set_name),
        FlexTextKind::LyricistName => emit!(LyricistName::<Vec<u32>>),
        FlexTextKind::ArrangerName => emit!(ArrangerName::<Vec<u32>>),
        FlexTextKind::PublisherName => emit!(PublisherName::<Vec<u32>>),
        FlexTextKind::PrimaryPerformerName => emit!(PrimaryPerformerName::<Vec<u32>>),
        FlexTextKind::AccompanyingPerformerName => emit!(AccompanyingPerformerName::<Vec<u32>>),
        FlexTextKind::RecordingDate => emit!(RecordingDate::<Vec<u32>>),
        FlexTextKind::RecordingLocation => emit!(RecordingLocation::<Vec<u32>>),
        FlexTextKind::UnknownPerformance => emit!(UnknownPerformanceText::<Vec<u32>>),
        FlexTextKind::Lyrics => emit!(Lyrics::<Vec<u32>>),
        FlexTextKind::LyricsLanguage => emit!(LyricsLanguage::<Vec<u32>>),
        FlexTextKind::Ruby => emit!(Ruby::<Vec<u32>>),
        FlexTextKind::RubyLanguage => emit!(RubyLanguage::<Vec<u32>>),
    }
}

/// Recover `(kind, text)` from a single-packet Flex Data text/metadata
/// [`MidiEvent`], or `None` if `event` isn't one.
///
/// A text longer than one packet holds is fragmented by [`push_flex_text`], and
/// a single packet of such a run does not decode on its own — use
/// [`FlexTextReassembler`] to read those back.
pub fn flex_text(event: &MidiEvent) -> Option<(FlexTextKind, String)> {
    flex_text_from_words(event.data_words())
}

/// Reassembles multi-packet Flex Data text runs.
///
/// [`push_flex_text`] fragments a long text across packets, but a lone packet of
/// that run carries no complete message — so a project name or lyric longer than
/// one packet could be *written* by tutti and not read back by tutti. This is
/// the missing half.
///
/// Feed every inbound event to [`push`](Self::push); it returns
/// `(kind, text)` the moment a run completes, and `None` while one is in flight
/// or the event isn't Flex Data. Single-packet texts complete immediately, so a
/// caller can route all Flex traffic through this rather than special-casing.
#[derive(Clone, Debug, Default)]
pub struct FlexTextReassembler {
    /// Accumulated words of the run in flight. Flex Data is one message type, so
    /// unlike SysEx there is no group/stream key to track.
    words: Vec<u32>,
}

impl FlexTextReassembler {
    /// A reassembler with no run in flight.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one inbound event, returning the completed `(kind, text)` if this
    /// event finishes a run.
    pub fn push(&mut self, event: &MidiEvent) -> Option<(FlexTextKind, String)> {
        if event.message_type() != super::UmpMessageType::FlexData {
            return None;
        }
        self.words.extend_from_slice(event.data_words());
        // midi2 owns the Format-field fragmentation rules, so decoding the
        // accumulated words is what tells us the run is complete.
        match flex_text_from_words(&self.words) {
            Some(done) => {
                self.words.clear();
                Some(done)
            }
            None => None,
        }
    }

    /// Drop any partial run — e.g. after a stream reset.
    pub fn reset(&mut self) {
        self.words.clear();
    }

    /// Whether a multi-packet run is currently in flight.
    pub fn is_in_flight(&self) -> bool {
        !self.words.is_empty()
    }
}

/// Decode a Flex Data text message from `words` — one packet's worth, or a
/// reassembled multi-packet run.
fn flex_text_from_words(words: &[u32]) -> Option<(FlexTextKind, String)> {
    use midi2::flex_data::FlexData;
    use midi2::UmpMessage;
    let UmpMessage::FlexData(fd) = UmpMessage::try_from(words).ok()? else {
        return None;
    };
    // Map each FlexData text variant to (kind, its String). Non-text variants
    // (tempo/timesig/metronome/key/chord) fall through to None.
    Some(match fd {
        FlexData::UnknownMetadataText(m) => (FlexTextKind::UnknownMetadata, m.text()),
        FlexData::ProjectName(m) => (FlexTextKind::ProjectName, m.text()),
        FlexData::CompositionName(m) => (FlexTextKind::CompositionName, m.text()),
        FlexData::MidiClipName(m) => (FlexTextKind::MidiClipName, m.text()),
        FlexData::CopyrightNotice(m) => (FlexTextKind::CopyrightNotice, m.text()),
        FlexData::ComposerName(m) => (FlexTextKind::ComposerName, m.name()),
        FlexData::LyricistName(m) => (FlexTextKind::LyricistName, m.text()),
        FlexData::ArrangerName(m) => (FlexTextKind::ArrangerName, m.text()),
        FlexData::PublisherName(m) => (FlexTextKind::PublisherName, m.text()),
        FlexData::PrimaryPerformerName(m) => (FlexTextKind::PrimaryPerformerName, m.text()),
        FlexData::AccompanyingPerformerName(m) => {
            (FlexTextKind::AccompanyingPerformerName, m.text())
        }
        FlexData::RecordingDate(m) => (FlexTextKind::RecordingDate, m.text()),
        FlexData::RecordingLocation(m) => (FlexTextKind::RecordingLocation, m.text()),
        FlexData::UnknownPerformanceText(m) => (FlexTextKind::UnknownPerformance, m.text()),
        FlexData::Lyrics(m) => (FlexTextKind::Lyrics, m.text()),
        FlexData::LyricsLanguage(m) => (FlexTextKind::LyricsLanguage, m.text()),
        FlexData::Ruby(m) => (FlexTextKind::Ruby, m.text()),
        FlexData::RubyLanguage(m) => (FlexTextKind::RubyLanguage, m.text()),
        _ => return None,
    })
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

/// Recover `(numerator, denominator)` from a Flex Data Set Time Signature
/// [`MidiEvent`], or `None` if `event` isn't one. Inverse of
/// [`MidiEvent::flex_set_time_signature`].
pub fn flex_time_signature(event: &MidiEvent) -> Option<(u8, u8)> {
    use midi2::flex_data::FlexData;
    use midi2::UmpMessage;
    let UmpMessage::FlexData(FlexData::SetTimeSignature(m)) =
        UmpMessage::try_from(event.data_words()).ok()?
    else {
        return None;
    };
    Some((m.numerator(), m.denominator()))
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

    #[test]
    fn flex_set_key_signature_round_trips() {
        let ev = MidiEvent::flex_set_key_signature(0, Tonic::D, KeySharpsFlats::Sharps(u3::new(2)));
        let (tonic, sf) = flex_key_signature(&ev).expect("is a key signature");
        assert_eq!(tonic, Tonic::D);
        assert_eq!(sf, KeySharpsFlats::Sharps(u3::new(2)));
        // A non-key-signature event decodes to None.
        assert!(flex_key_signature(&MidiEvent::flex_set_tempo(0, 120.0)).is_none());
    }

    #[test]
    fn flex_set_chord_name_round_trips_with_bass() {
        // Cmaj7 / G — a tonic quality plus a distinct bass note.
        let chord = ChordName {
            tonic: Tonic::C,
            tonic_sharps_flats: ChordSharpsFlats::Natural,
            chord_type: ChordType::Major7th,
            alterations: [Some(Alteration::Add(u4::new(9))), None, None, None],
            bass: Some(ChordBass {
                note: Tonic::G,
                sharps_flats: ChordSharpsFlats::Natural,
                chord_type: ChordType::Major,
                alterations: [None, None],
            }),
        };
        let ev = MidiEvent::flex_set_chord_name(0, &chord);
        let back = flex_chord_name(&ev).expect("is a chord name");
        assert_eq!(back, chord);
    }

    #[test]
    fn flex_set_chord_name_round_trips_without_bass() {
        let chord = ChordName {
            tonic: Tonic::A,
            tonic_sharps_flats: ChordSharpsFlats::Natural,
            chord_type: ChordType::Minor,
            alterations: [None; 4],
            bass: None,
        };
        let ev = MidiEvent::flex_set_chord_name(0, &chord);
        let back = flex_chord_name(&ev).expect("is a chord name");
        assert_eq!(back.bass, None, "no-bass encoding round-trips to None");
        assert_eq!(back.tonic, Tonic::A);
        assert_eq!(back.chord_type, ChordType::Minor);
    }

    #[test]
    fn flex_text_single_packet_round_trips_kind_and_string() {
        // Short strings fit one 128-bit packet (≤ 12 UTF-8 bytes).
        for (kind, text) in [
            (FlexTextKind::ProjectName, "My Song"),
            (FlexTextKind::MidiClipName, "Verse 1"),
            (FlexTextKind::Lyrics, "la la"),
            (FlexTextKind::ComposerName, "Ada"),
        ] {
            let mut out = Vec::new();
            push_flex_text(kind, text, 0, &mut out);
            assert_eq!(out.len(), 1, "{text:?} should be one packet");
            let (k, s) = flex_text(&out[0]).expect("decodes as text");
            assert_eq!(k, kind);
            assert_eq!(s, text);
        }
    }

    #[test]
    fn flex_text_rejects_non_text() {
        assert!(flex_text(&MidiEvent::flex_set_tempo(0, 120.0)).is_none());
        assert!(flex_text(&MidiEvent::note_on(0, 0, 60, 0x8000)).is_none());
    }

    #[test]
    fn flex_set_metronome_decodes_via_midi2() {
        use midi2::flex_data::FlexData;
        let accents = BarAccents {
            primary: 24,
            secondary: 12,
            tertiary: 6,
        };
        let ev = MidiEvent::flex_set_metronome(0, 24, accents);
        match midi2::UmpMessage::try_from(ev.data_words()).unwrap() {
            midi2::UmpMessage::FlexData(FlexData::SetMetronome(m)) => {
                assert_eq!(m.number_of_clocks_per_primary_click(), 24);
                assert_eq!(m.bar_accent1(), 24);
                assert_eq!(m.bar_accent2(), 12);
                assert_eq!(m.bar_accent3(), 6);
            }
            other => panic!("expected SetMetronome, got {other:?}"),
        }
    }

    #[test]
    fn multi_packet_text_reassembles() {
        // The write-but-can't-read asymmetry: push_flex_text fragments a long
        // text, but a lone packet of that run carries no complete message, so
        // flex_text alone could not read back what tutti had written.
        let long = "A project name comfortably longer than a single Flex Data packet can hold";
        let mut out = Vec::new();
        push_flex_text(FlexTextKind::ProjectName, long, 0, &mut out);
        assert!(out.len() > 1, "text spans packets");
        assert!(
            flex_text(&out[0]).is_none(),
            "a single packet of a run does not decode on its own"
        );

        let mut r = FlexTextReassembler::new();
        let mut got = None;
        for (i, ev) in out.iter().enumerate() {
            let done = r.push(ev);
            if i + 1 < out.len() {
                assert!(done.is_none(), "run still in flight at packet {i}");
                assert!(r.is_in_flight());
            } else {
                got = done;
            }
        }
        let (kind, text) = got.expect("run completes on the last packet");
        assert_eq!(kind, FlexTextKind::ProjectName);
        assert_eq!(text, long);
        assert!(!r.is_in_flight(), "buffer cleared after completion");
    }

    #[test]
    fn reassembler_passes_single_packet_text_straight_through() {
        // So a caller can route all Flex traffic through the reassembler rather
        // than special-casing short texts.
        let mut out = Vec::new();
        push_flex_text(FlexTextKind::MidiClipName, "Verse", 0, &mut out);
        assert_eq!(out.len(), 1);

        let mut r = FlexTextReassembler::new();
        assert_eq!(
            r.push(&out[0]),
            Some((FlexTextKind::MidiClipName, "Verse".to_string()))
        );
        assert!(!r.is_in_flight());

        // Non-text Flex Data and non-Flex events are ignored.
        assert!(r.push(&MidiEvent::flex_set_tempo(0, 120.0)).is_none());
        assert!(r.push(&MidiEvent::note_on(0, 0, 60, 0x8000)).is_none());
        r.reset();
        assert!(!r.is_in_flight());
    }
}
