use super::*;
use tutti_ump::MidiChannel;
use tutti_ump::MidiGroup;

#[test]
fn tempo_conversion_kernels_round_trip() {
    for bpm in [60.0, 120.0, 140.0, 174.0] {
        let field = bpm_to_ten_ns_per_quarter(Bpm::of(bpm));
        let back = ten_ns_per_quarter_to_bpm(field).expect("nonzero");
        assert!((back.get() - bpm).abs() < 0.05, "bpm {bpm} → {back}");
    }
    // The "no tempo" sentinel, both directions: a `Bpm` is never zero, so the
    // zero field is only a tempo above 6e9 BPM, which the field cannot hold.
    assert_eq!(bpm_to_ten_ns_per_quarter(Bpm::of(1e10)), 0);
    assert_eq!(ten_ns_per_quarter_to_bpm(0), None);
}

// Fixed values: a round trip cancels the constant, so these pin it.
// Mutation: TEN_NS_UNITS_PER_MINUTE = 6e8 fails the first two.
#[test]
fn tempo_wire_field_is_ten_ns_per_quarter() {
    assert_eq!(bpm_to_ten_ns_per_quarter(Bpm::of(120.0)), 50_000_000);
    assert_eq!(bpm_to_ten_ns_per_quarter(Bpm::of(60.0)), 100_000_000);
    let ev = MidiEvent::flex_set_tempo(MidiGroup::FIRST, Bpm::of(120.0));
    assert_eq!(ev.data_words(), &[0xD010_0000, 0x02FA_F080, 0, 0]);
}

#[test]
fn external_set_tempo_packet_decodes_to_120_bpm() {
    let ev = MidiEvent::from_ump(0, &[0xD010_0000, 0x02FA_F080, 0, 0]);
    assert_eq!(flex_tempo_bpm(&ev), Some(Bpm::of(120.0)));
    assert_eq!(ten_ns_per_quarter_to_bpm(100_000_000), Some(Bpm::of(60.0)));
}

#[test]
fn flex_set_tempo_round_trips_bpm() {
    for bpm in [60.0, 120.0, 140.0, 174.0] {
        let ev = MidiEvent::flex_set_tempo(MidiGroup::FIRST, Bpm::of(bpm));
        let decoded = flex_tempo_bpm(&ev).expect("is a Set Tempo");
        // 10ns-per-qn is integer-quantized, so allow a tiny epsilon.
        assert!((decoded.get() - bpm).abs() < 0.05, "bpm {bpm} → {decoded}");
    }
}

#[test]
fn flex_set_time_signature_decodes_via_midi2() {
    use midi2::flex_data::FlexData;
    let ev = MidiEvent::flex_set_time_signature(MidiGroup::FIRST, 7, 8, 8);
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
    assert!(
        flex_tempo_bpm(&MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000
        ))
        .is_none()
    );
}

#[test]
fn flex_set_key_signature_round_trips() {
    let ev = MidiEvent::flex_set_key_signature(
        MidiGroup::FIRST,
        Tonic::D,
        KeySharpsFlats::Sharps(u3::new(2)),
    );
    let (tonic, sf) = flex_key_signature(&ev).expect("is a key signature");
    assert_eq!(tonic, Tonic::D);
    assert_eq!(sf, KeySharpsFlats::Sharps(u3::new(2)));
    // A non-key-signature event decodes to None.
    assert!(
        flex_key_signature(&MidiEvent::flex_set_tempo(MidiGroup::FIRST, Bpm::of(120.0))).is_none()
    );
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
    let ev = MidiEvent::flex_set_chord_name(MidiGroup::FIRST, &chord);
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
    let ev = MidiEvent::flex_set_chord_name(MidiGroup::FIRST, &chord);
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
        push_flex_text(kind, text, MidiGroup::FIRST, &mut out);
        assert_eq!(out.len(), 1, "{text:?} should be one packet");
        let (k, s) = flex_text(&out[0]).expect("decodes as text");
        assert_eq!(k, kind);
        assert_eq!(s, text);
    }
}

#[test]
fn flex_text_rejects_non_text() {
    assert!(flex_text(&MidiEvent::flex_set_tempo(MidiGroup::FIRST, Bpm::of(120.0))).is_none());
    assert!(
        flex_text(&MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000
        ))
        .is_none()
    );
}

#[test]
fn flex_set_metronome_decodes_via_midi2() {
    use midi2::flex_data::FlexData;
    let accents = BarAccents {
        primary: 24,
        secondary: 12,
        tertiary: 6,
    };
    let ev = MidiEvent::flex_set_metronome(MidiGroup::FIRST, 24, accents);
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
    push_flex_text(FlexTextKind::ProjectName, long, MidiGroup::FIRST, &mut out);
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
    push_flex_text(
        FlexTextKind::MidiClipName,
        "Verse",
        MidiGroup::FIRST,
        &mut out,
    );
    assert_eq!(out.len(), 1);

    let mut r = FlexTextReassembler::new();
    assert_eq!(
        r.push(&out[0]),
        Some((FlexTextKind::MidiClipName, "Verse".to_string()))
    );
    assert!(!r.is_in_flight());

    // Non-text Flex Data and non-Flex events are ignored.
    assert!(
        r.push(&MidiEvent::flex_set_tempo(MidiGroup::FIRST, Bpm::of(120.0)))
            .is_none()
    );
    assert!(
        r.push(&MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000
        ))
        .is_none()
    );
    r.reset();
    assert!(!r.is_in_flight());
}
