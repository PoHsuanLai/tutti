use super::*;
use tutti_ump::{MidiChannel, MidiGroup};

/// Layout contract: `#[repr(C)] { u32, [u32;4] }` is exactly 20 bytes.
#[test]
fn layout_is_20_bytes() {
    assert_eq!(core::mem::size_of::<MidiEvent>(), 20);
}

// NOTE: that `with_frame_offset` sets the offset without disturbing the
// payload is pinned by `crate::message::tests::channel_controllers_round_trip_to_identical_wire_bytes`,
// which stamps offset 91 onto four events and then asserts the data words
// come back byte-identical through decode → re-encode.

#[test]
fn velocity_u16_is_lossless_where_u7_is_not() {
    // The reason this accessor exists: `velocity_u7` narrows a MIDI 2.0
    // velocity to 7 bits, so any value between 7-bit steps is destroyed.
    // Reading a MIDI-2 source through the u7 accessor would throw away
    // exactly the resolution MIDI 2.0 was for.
    for vel in [0x0000u16, 0x0001, 0x1234, 0x8000, 0xABCD, 0xFFFF] {
        let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, vel);
        assert_eq!(ev.velocity_u16(), Some(vel), "exact for {vel:#06x}");
    }

    // A value that is *not* on a 7-bit boundary survives here and does not
    // survive there.
    let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xABCD);
    assert_eq!(ev.velocity_u16(), Some(0xABCD));
    let narrowed = ev.velocity_u7().expect("u7 view exists");
    assert_ne!(
        crate::convert::midi1_velocity_to_midi2(narrowed),
        0xABCD,
        "u7 round-trip must be lossy — otherwise this accessor is pointless"
    );

    // Note Off carries velocity too.
    assert_eq!(
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000).velocity_u16(),
        Some(0x4000)
    );
}

#[test]
fn velocity_u16_upscales_a_midi1_note() {
    // A MIDI 1.0 note reads at full resolution via the spec's
    // Min-Center-Max upscale, so a caller need not know which protocol the
    // event arrived as. 127 → full scale and 64 → centre are the anchors
    // that algorithm guarantees.
    let ev = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 127]).expect("midi1 note-on");
    assert_eq!(ev.velocity_u16(), Some(0xFFFF));

    let ev = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 64]).expect("midi1 note-on");
    assert_eq!(ev.velocity_u16(), Some(0x8000));

    // Non-note messages have no velocity.
    assert_eq!(
        MidiEvent::timing_clock(MidiGroup::FIRST).velocity_u16(),
        None
    );
}

#[test]
fn channel_reads_both_voice_versions() {
    // MIDI 2.0 channel voice (UMP type 0x4).
    assert_eq!(
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::of(5), 60, 0x8000).channel(),
        Some(MidiChannel::of(5))
    );
    // MIDI 1.0 channel voice (UMP type 0x2), built via the wire bridge.
    let cv1 = MidiEvent::from_midi1_bytes(0, &[0x93, 0x3C, 0x64]).unwrap();
    assert_eq!(cv1.channel(), Some(MidiChannel::of(3)));
    // System messages carry no channel.
    assert_eq!(MidiEvent::timing_clock(MidiGroup::FIRST).channel(), None);
    assert_eq!(MidiEvent::noop().channel(), None);
}

#[test]
fn split_ump_stream_walks_mixed_word_lengths() {
    // A native-UMP packet concatenates messages of different lengths with no
    // separators: 1-word JR Timestamp, 2-word CV2 note-on, 1-word clock.
    let jr = MidiEvent::jr_timestamp(0x1234);
    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::of(3), 60, 0x8000);
    let clock = MidiEvent::timing_clock(MidiGroup::FIRST);

    let mut words = Vec::new();
    words.extend_from_slice(jr.data_words());
    words.extend_from_slice(note.data_words());
    words.extend_from_slice(clock.data_words());
    assert_eq!(words.len(), 4, "1 + 2 + 1 words");

    let split: Vec<_> = split_ump_stream(&words).collect();
    assert_eq!(split, vec![jr, note, clock]);
}

#[test]
fn split_ump_stream_drops_a_truncated_tail() {
    // A 2-word CV2 note-on with only its first word present is not decodable.
    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
    let words = [note.data_words()[0]];
    assert_eq!(split_ump_stream(&words).count(), 0);

    // …but a complete message before the truncated tail still comes out.
    let clock = MidiEvent::timing_clock(MidiGroup::FIRST);
    let words = [clock.data_words()[0], note.data_words()[0]];
    assert_eq!(split_ump_stream(&words).collect::<Vec<_>>(), vec![clock]);
}

#[test]
fn split_ump_stream_is_empty_for_no_words() {
    assert_eq!(split_ump_stream(&[]).count(), 0);
}

#[test]
fn reserved_message_types_have_their_table_4_sizes() {
    // M2-104-UM §2.1.4 Table 4. The reserved types carry fixed sizes
    // precisely so a parser can skip them without losing the stream, so
    // these are as load-bearing as the defined ones. 0xB/0xC are 96 bits.
    for (mt, words) in [
        (0x0, 1),
        (0x1, 1),
        (0x2, 1),
        (0x3, 2),
        (0x4, 2),
        (0x5, 4),
        (0x6, 1),
        (0x7, 1),
        (0x8, 2),
        (0x9, 2),
        (0xA, 2),
        (0xB, 3),
        (0xC, 3),
        (0xD, 4),
        (0xE, 4),
        (0xF, 4),
    ] {
        assert_eq!(ump_word_count(mt), words, "MT {mt:#x}");
    }
}

#[test]
fn a_reserved_type_packet_does_not_desync_the_stream() {
    // An MT-0xB packet (3 words) followed by a real message: if the walker
    // takes 4 words for the 0xB, it eats the note-on's first word and every
    // message after it decodes as garbage.
    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
    let mut words = vec![0xB000_0000u32, 0, 0];
    words.extend_from_slice(note.data_words());

    let split: Vec<_> = split_ump_stream(&words).collect();
    assert_eq!(split.len(), 2, "reserved packet, then the note-on");
    assert_eq!(split[1], note, "the note-on survives intact");
}
