//! A Flex Data broadcast request reaches the outbound mailbox as real UMP.
//!
//! `BroadcastFlexMetadata` is fire-and-forget: the app raises it, one system
//! turns it into packets, and the hardware-out pump sends them. Nothing in that
//! chain returns a value, so the only way to know an arm works is to read the
//! mailbox and decode what landed there.
//!
//! Each assertion decodes the *wire bytes* rather than trusting the variant it
//! sent — a system arm that built the wrong message, or stamped the wrong group,
//! would otherwise pass. Tempo and time signature are the ones that matter most:
//! they are also transport state, and a broadcast that silently encoded 0 BPM
//! would tell downstream gear the session had stopped.

#![cfg(feature = "midi")]

use bevy_app::prelude::*;

use bevy_tutti::midi::test_support::drain_midi_out;
use bevy_tutti::midi::{MidiOutRes, BroadcastFlexMetadata, MidiMetadataPlugin};
use bevy_tutti::AudioEngineState;
use tutti_midi_types::midi2::ux::u3;
use tutti_midi_types::ump::{
    flex_chord_name, flex_key_signature, flex_tempo_bpm, flex_text, BarAccents, ChordName,
    ChordSharpsFlats, ChordType, FlexTextKind, KeySharpsFlats, MidiEvent, Tonic,
};

/// An app with just the metadata plugin and the mailbox it writes into.
///
/// `flex_metadata_broadcast_system` is gated on `engine_ready`, so the state
/// resource has to claim a running engine or nothing runs and every assertion
/// here fails on an empty mailbox.
fn app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioEngineState::Running);
    app.init_resource::<MidiOutRes>();
    app.add_plugins(MidiMetadataPlugin);
    app
}

/// Raise one request, run a frame, and return what reached the mailbox.
fn broadcast(app: &mut App, request: BroadcastFlexMetadata) -> Vec<MidiEvent> {
    app.world_mut().write_message(request);
    app.update();
    let out = app.world().resource::<MidiOutRes>();
    drain_midi_out(out)
}

/// Tempo survives the BPM → 10-ns-per-quarter encoding and comes back.
#[test]
fn a_tempo_broadcast_carries_the_bpm_it_was_given() {
    let mut app = app();
    let packets = broadcast(&mut app, BroadcastFlexMetadata::Tempo(140.0));

    assert_eq!(packets.len(), 1, "one tempo message, one packet");
    let bpm = flex_tempo_bpm(&packets[0]).expect("the packet decodes as Set Tempo");
    assert!(
        (bpm - 140.0).abs() < 0.05,
        "140 BPM went out as {bpm} — the wire field is a reciprocal, so an \
         encoding slip inverts rather than offsets"
    );
}

/// The reciprocal encoding means a slow tempo and a fast one must not collapse
/// onto the same wire field.
#[test]
fn distinct_tempos_stay_distinct_on_the_wire() {
    let mut app = app();
    let slow = broadcast(&mut app, BroadcastFlexMetadata::Tempo(60.0));
    let fast = broadcast(&mut app, BroadcastFlexMetadata::Tempo(174.0));

    assert!((flex_tempo_bpm(&slow[0]).unwrap() - 60.0).abs() < 0.05);
    assert!((flex_tempo_bpm(&fast[0]).unwrap() - 174.0).abs() < 0.05);
    assert_ne!(
        slow[0].data_words(),
        fast[0].data_words(),
        "two tempos must not encode identically"
    );
}

/// A time signature reaches the wire with its three fields intact.
///
/// Decoded through `midi2` directly: there is no upstream `flex_time_signature`
/// reader, and the upstream test for the constructor does the same.
#[test]
fn a_time_signature_broadcast_carries_all_three_fields() {
    let mut app = app();
    let packets = broadcast(
        &mut app,
        BroadcastFlexMetadata::TimeSignature {
            numerator: 7,
            denominator: 8,
            num_32nd_notes: 8,
        },
    );

    assert_eq!(packets.len(), 1);
    let decoded = tutti_midi_types::midi2::UmpMessage::try_from(packets[0].data_words())
        .expect("the packet is a valid UMP message");
    let tutti_midi_types::midi2::UmpMessage::FlexData(
        tutti_midi_types::midi2::flex_data::FlexData::SetTimeSignature(m),
    ) = decoded
    else {
        panic!("expected Set Time Signature, got {decoded:?}");
    };
    assert_eq!(m.numerator(), 7, "7/8 must not arrive as 4/4");
    assert_eq!(m.denominator(), 8);
    assert_eq!(m.number_of_32nd_notes(), 8);
}

/// The metronome's three accent positions each reach the wire, in order.
///
/// They are three same-typed `u8`s side by side, which is exactly the shape a
/// transposed argument survives silently.
#[test]
fn a_metronome_broadcast_keeps_its_accents_in_order() {
    let mut app = app();
    let packets = broadcast(
        &mut app,
        BroadcastFlexMetadata::Metronome {
            clocks_per_click: 24,
            accents: BarAccents {
                primary: 1,
                secondary: 2,
                tertiary: 3,
            },
        },
    );

    assert_eq!(packets.len(), 1);
    let decoded = tutti_midi_types::midi2::UmpMessage::try_from(packets[0].data_words())
        .expect("the packet is a valid UMP message");
    let tutti_midi_types::midi2::UmpMessage::FlexData(
        tutti_midi_types::midi2::flex_data::FlexData::SetMetronome(m),
    ) = decoded
    else {
        panic!("expected Set Metronome, got {decoded:?}");
    };
    assert_eq!(m.number_of_clocks_per_primary_click(), 24);
    assert_eq!(
        (m.bar_accent1(), m.bar_accent2(), m.bar_accent3()),
        (1, 2, 3),
        "accents must not be transposed"
    );
}

/// The two variants that predate this change still work — they had no
/// ECS-level coverage, only upstream constructor tests.
#[test]
fn chord_name_and_key_signature_still_reach_the_wire() {
    let mut app = app();

    let chord = ChordName {
        tonic: Tonic::C,
        tonic_sharps_flats: ChordSharpsFlats::Natural,
        chord_type: ChordType::Major7th,
        alterations: [None; 4],
        bass: None,
    };
    let packets = broadcast(&mut app, BroadcastFlexMetadata::ChordName(chord));
    assert_eq!(
        flex_chord_name(&packets[0]),
        Some(chord),
        "the chord that went out is the chord that was asked for"
    );

    let sharps_2 = KeySharpsFlats::Sharps(u3::new(2));
    let packets = broadcast(
        &mut app,
        BroadcastFlexMetadata::KeySignature {
            tonic: Tonic::D,
            sharps_flats: sharps_2,
        },
    );
    let (tonic, sharps) = flex_key_signature(&packets[0]).expect("decodes as Set Key Signature");
    assert_eq!(tonic, Tonic::D);
    assert_eq!(sharps, sharps_2);
}

/// A short text broadcast round-trips: one packet, kind and string intact.
#[test]
fn a_short_text_broadcast_arrives_decodable() {
    let mut app = app();
    let packets = broadcast(
        &mut app,
        BroadcastFlexMetadata::Text {
            kind: FlexTextKind::CompositionName,
            text: "Verse 1".into(),
        },
    );

    assert_eq!(packets.len(), 1, "≤12 UTF-8 bytes fits one packet");
    let (kind, text) = flex_text(&packets[0]).expect("decodes as text");
    assert_eq!(kind, FlexTextKind::CompositionName);
    assert_eq!(text, "Verse 1");
}

/// A long text broadcast reaches the mailbox as several packets, and *every*
/// one of them arrives.
///
/// The text arm is the only one that pushes more than a single packet, so it is
/// the only one where a mailbox that silently kept the first and dropped the
/// rest would look like success. The count is the assertion.
///
/// What is deliberately *not* asserted: that a fragment decodes. `push_flex_text`
/// builds one logical midi2 message and chunks its words into 4-word packets, so
/// an individual packet past the first is not a standalone UMP message and
/// `flex_text` correctly declines it. Reassembly is the receiver's job and has
/// no implementation on this side to test.
#[test]
fn a_long_text_broadcast_arrives_as_every_one_of_its_packets() {
    let mut app = app();
    let title = "A composition title comfortably longer than one packet holds";
    let packets = broadcast(
        &mut app,
        BroadcastFlexMetadata::Text {
            kind: FlexTextKind::CompositionName,
            text: title.into(),
        },
    );

    // 4 words per packet, 4 bytes per word, minus the header word's 4 bytes.
    let expected = title.len().div_ceil(12);
    assert_eq!(
        packets.len(),
        expected,
        "a {}-byte title needs {expected} packets; the mailbox took {}",
        title.len(),
        packets.len()
    );
}

/// With no mailbox present the system drops the request instead of panicking or
/// holding it for a later frame.
///
/// `MidiOutRes` is inserted by `MidiOutPlugin`, which an app can leave out, so
/// `Option<Res<_>>` + `clear()` is the reachable path rather than an error case.
#[test]
fn a_broadcast_with_no_mailbox_is_dropped_not_queued() {
    let mut app = App::new();
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(MidiMetadataPlugin);

    app.world_mut()
        .write_message(BroadcastFlexMetadata::Tempo(120.0));
    app.update();

    // Reaching here is the claim: no panic on the missing resource. Now give it
    // a mailbox and confirm the earlier request did not resurface.
    app.init_resource::<MidiOutRes>();
    app.update();
    let out = app.world().resource::<MidiOutRes>();
    assert!(
        drain_midi_out(out).is_empty(),
        "a request raised with no mailbox is gone, not replayed once one appears"
    );
}
