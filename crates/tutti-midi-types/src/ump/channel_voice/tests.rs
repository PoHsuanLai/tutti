use midi2::Channeled;
use midi2::{UmpMessage, channel_voice2};
use tutti_fixtures::midi;
use tutti_ump::CCNumber;
use tutti_ump::MidiChannel;
use tutti_ump::MidiGroup;

#[test]
fn note_on_decodes_via_midi2() {
    let ev = midi::note_on(MidiGroup::FIRST, MidiChannel::of(5), 60, 0x8000);
    let msg = UmpMessage::try_from(ev.words()).unwrap();
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
    let ev = midi::cc(
        MidiGroup::FIRST,
        MidiChannel::of(2),
        CCNumber::BRIGHTNESS,
        0xDEAD_BEEF,
    );
    let msg = UmpMessage::try_from(ev.words()).unwrap();
    let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::ControlChange(m)) = msg else {
        panic!("expected CV2 ControlChange");
    };
    assert_eq!(u8::from(m.channel()), 2);
    assert_eq!(u8::from(m.control()), 74);
    assert_eq!(m.control_change_data(), 0xDEAD_BEEF);
}

/// Every CC number survives the trip out to the wire and back. The mask
/// happens once, at `CCNumber::of`, and nothing downstream can change the
/// number (a debug-only assert plus a mask at the wire would let a release
/// build silently emit a *different* controller).
#[test]
fn every_cc_number_round_trips_through_the_wire() {
    for raw in 0u8..128 {
        let n = CCNumber::of(raw);
        let ev = midi::cc(MidiGroup::FIRST, MidiChannel::of(2), n, 0);
        let msg = UmpMessage::try_from(ev.words()).unwrap();
        let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::ControlChange(m)) = msg else {
            panic!("expected CV2 ControlChange");
        };
        assert_eq!(CCNumber::of(u8::from(m.control())), n, "CC {raw}");
    }
}

/// The channel and the CC number do not collide. They sit adjacent in
/// `cc(group, channel, cc, value)`, and as bare `u8`s a swap would emit a
/// well-formed packet addressing the wrong controller on the wrong channel.
/// The newtypes make that a compile error; this pins the property the
/// compile error protects — that the two land in *different* wire fields.
#[test]
fn the_channel_and_the_cc_number_land_in_different_fields() {
    // Deliberately equal raw values, so a field mix-up cannot hide behind
    // one of them: only ordering distinguishes correct from swapped.
    let ev = midi::cc(
        MidiGroup::FIRST,
        MidiChannel::of(7),
        CCNumber::of(3),
        0x1234_5678,
    );
    let msg = UmpMessage::try_from(ev.words()).unwrap();
    let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::ControlChange(m)) = msg else {
        panic!("expected CV2 ControlChange");
    };
    assert_eq!(u8::from(m.channel()), 7, "channel");
    assert_eq!(u8::from(m.control()), 3, "cc number");
}

/// Per-Note Management is written with D in bit 1 and S in bit 0 (M2-104
/// Figure 51), pinned as a literal word. `midi2` reads the two the other way
/// round, so it is not asked here. Mutation: swapping the flags in
/// `per_note_management` failed both asserts.
#[test]
fn per_note_management_writes_the_spec_bits() {
    let detach = midi::per_note_management(MidiGroup::FIRST, MidiChannel::of(4), 60, true, false);
    assert_eq!(detach.0[0], 0x40F4_3C02);
    let reset = midi::per_note_management(MidiGroup::FIRST, MidiChannel::of(4), 60, false, true);
    assert_eq!(reset.0[0], 0x40F4_3C01);
}

/// The app-facing view reads the same bits. Mutation: reading the flags from
/// `midi2` (`m.detach()`) made the first case read as a reset.
#[test]
fn per_note_management_message_reads_the_spec_bits() {
    use crate::MidiMessage;
    let msg = |detach, reset| {
        MidiMessage::from_ump(midi::per_note_management(
            MidiGroup::FIRST,
            MidiChannel::of(4),
            60,
            detach,
            reset,
        ))
    };
    assert!(matches!(
        msg(true, false),
        MidiMessage::PerNoteManagement {
            detach: true,
            reset: false,
            note: 60,
            ..
        }
    ));
    assert!(matches!(
        msg(false, true),
        MidiMessage::PerNoteManagement {
            detach: false,
            reset: true,
            ..
        }
    ));
}
