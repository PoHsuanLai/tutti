//! What a CLAP plugin is handed of MIDI 2.0's Groups and per-note state
//! (M2-104 sections 2.1.2 and 7.4, Appendices C.3 and D.2): a Group is a note
//! port, a note has an id of its own, per-note values told before a Note On
//! are its initial values, and an RPN or NRPN is Control Changes through the
//! stateful narrower. Every expectation is the spec's figure, written out.

use clap_sys::events::{
    CLAP_NOTE_EXPRESSION_BRIGHTNESS, CLAP_NOTE_EXPRESSION_PRESSURE, CLAP_NOTE_EXPRESSION_TUNING,
};
use tutti_clap_host::{ClapEvent, InputEventList};
use tutti_fixtures::midi as fx;
use tutti_graph::Offset;
use tutti_midi_protocol::PitchBendSensitivity;
use tutti_types::Samples;
use tutti_ump::{Address, Attribute, MidiChannel, MidiGroup, NoteId, NoteNumber, Rpn, Ump, Voice2};

const BLOCK: usize = 1_000;

fn at(k: usize) -> Offset {
    Offset::new(k, Samples(BLOCK)).expect("inside the block")
}

fn group(n: u8) -> MidiGroup {
    MidiGroup::new(n).expect("a Group")
}

fn ch(n: u8) -> MidiChannel {
    MidiChannel::of(n)
}

fn add(list: &mut InputEventList, ump: Ump) {
    list.add_midi(&(at(0), ump));
}

/// The events the list holds, then empty.
fn drain(list: &mut InputEventList) -> Vec<ClapEvent> {
    list.take_events()
}

/// `(port, channel, key, note_id)` of a NOTE_ON.
fn note_on(event: &ClapEvent) -> (i16, i16, i16, i32) {
    let ClapEvent::NoteOn(e) = event else {
        panic!("expected a NoteOn, got {event:?}");
    };
    (e.port_index, e.channel, e.key, e.note_id)
}

/// `(note_id, expression_id, value)` of a NOTE_EXPRESSION.
fn expression(event: &ClapEvent) -> (i32, i32, f64) {
    let ClapEvent::NoteExpression(e) = event else {
        panic!("expected a NoteExpression, got {event:?}");
    };
    (e.note_id, e.expression_id, e.value)
}

fn midi_bytes(event: &ClapEvent) -> (u16, [u8; 3]) {
    let ClapEvent::Midi(e) = event else {
        panic!("expected a Midi event, got {event:?}");
    };
    (e.port_index, e.data)
}

fn near(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-3, "{got} vs {want}");
}

/// A Note On with the Attribute Pitch `pitch` (Q7.9, so in 1/512 semitone).
fn note_on_pitched(group: MidiGroup, channel: MidiChannel, key: u8, pitch: f64) -> Ump {
    Ump::from(tutti_ump::Message::Midi2(
        Address { group, channel },
        Voice2::NoteOn {
            note: NoteNumber::of(key),
            velocity: fx::MID_VELOCITY,
            attribute: Attribute::Pitch((pitch * 512.0) as u16),
        },
    ))
}

/// **A Group is a note port, and Groups are not folded into channels**
/// (M2-104 section 2.1.2, Appendix B.1: each Group is a separate 16-channel
/// space). Group 0 and Group 1 each play channel 0, key 60: two notes, on
/// ports 0 and 1, on channel 0 both, with ids of their own; and a controller
/// of Group 1 is MIDI on port 1.
#[test]
fn two_groups_land_on_note_ports_zero_and_one() {
    let mut list = InputEventList::new();
    list.set_note_ports(&[false, false]);
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    add(
        &mut list,
        fx::note_on(group(1), ch(0), 60, fx::MID_VELOCITY),
    );
    add(
        &mut list,
        fx::cc(
            group(1),
            ch(0),
            tutti_ump::CCNumber::BRIGHTNESS,
            fx::wide_value(100),
        ),
    );
    let events = drain(&mut list);
    assert_eq!(events.len(), 3, "{events:?}");
    let (p0, c0, k0, id0) = note_on(&events[0]);
    let (p1, c1, k1, id1) = note_on(&events[1]);
    assert_eq!((p0, c0, k0), (0, 0, 60));
    assert_eq!((p1, c1, k1), (1, 0, 60));
    assert_ne!(id0, id1, "two notes, two ids");
    assert_eq!(midi_bytes(&events[2]), (1, [0xB0, 74, 100]));
    assert_eq!(list.groups_dropped(), 0);
}

/// **A Group the plugin has no note port for is dropped and counted**, whole
/// (a note, a controller), and not heard on Group 0's channels. A message that
/// names no Group's channels (System Real Time) still goes to port 0 and is
/// not counted.
#[test]
fn a_group_without_a_note_port_is_dropped_and_counted() {
    let mut list = InputEventList::new();
    list.set_note_ports(&[false]);
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    add(
        &mut list,
        fx::note_on(group(1), ch(0), 61, fx::MID_VELOCITY),
    );
    add(
        &mut list,
        fx::cc(
            group(2),
            ch(0),
            tutti_ump::CCNumber::BRIGHTNESS,
            fx::wide_value(1),
        ),
    );
    add(&mut list, fx::timing_clock(group(1)));
    let events = drain(&mut list);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(note_on(&events[0]).2, 60, "only Group 0's note is heard");
    assert_eq!(midi_bytes(&events[1]), (0, [0xF8, 0, 0]));
    assert_eq!(
        list.groups_dropped(),
        2,
        "the Group 1 note and the Group 2 CC"
    );
}

/// **A Detach ends the note on a key, and the next Note On is another note**
/// (M2-104 section 7.4.5): its id is new, and a per-note controller after it
/// reaches the new note alone. Before the Detach the same controller reaches
/// the first.
#[test]
fn a_detach_then_a_note_on_is_a_new_note_that_alone_hears_the_controller() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    let first = note_on(&drain(&mut list)[0]).3;

    add(
        &mut list,
        fx::poly_pressure(group(0), ch(0), 60, 0x8000_0000),
    );
    let before = drain(&mut list);
    assert_eq!(before.len(), 1);
    assert_eq!(expression(&before[0]).0, first, "the first note hears it");

    add(
        &mut list,
        fx::per_note_management(group(0), ch(0), 60, true, false),
    );
    assert!(drain(&mut list).is_empty(), "a Detach sends nothing");
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    let second = note_on(&drain(&mut list)[0]).3;
    assert_ne!(
        second, first,
        "a Detach separates the old note from the next"
    );

    add(
        &mut list,
        fx::poly_pressure(group(0), ch(0), 60, 0x4000_0000),
    );
    let after = drain(&mut list);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(expression(&after[0]).0, second);
}

/// Without a Detach both notes of a key hear a controller, and a plugin's
/// `NOTE_END` takes one out: a Note Off does not end a note (its tail still
/// responds), the plugin's report does.
#[test]
fn only_a_note_end_stops_a_released_note_hearing_controllers() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    let id = note_on(&drain(&mut list)[0]).3;
    add(&mut list, fx::note_off(group(0), ch(0), 60, 0));
    drain(&mut list);

    add(
        &mut list,
        fx::poly_pressure(group(0), ch(0), 60, 0x8000_0000),
    );
    let tail = drain(&mut list);
    assert_eq!(tail.len(), 1, "a released note's tail still responds");
    assert_eq!(expression(&tail[0]).0, id);

    list.note_ended(id);
    add(
        &mut list,
        fx::poly_pressure(group(0), ch(0), 60, 0x4000_0000),
    );
    assert!(drain(&mut list).is_empty(), "an ended note hears nothing");
}

/// **Per-note values sent before a Note On are the note's initial values**
/// (M2-104 Appendix C.3): nothing is sent while no note sounds, and the Note
/// On is followed by each held value as an expression of the new note.
#[test]
fn values_told_before_a_note_on_are_its_initial_expressions() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::per_note_controller(group(0), ch(2), 64, 74, 0x8000_0000, true),
    );
    add(
        &mut list,
        fx::poly_pressure(group(0), ch(2), 64, 0x4000_0000),
    );
    assert!(drain(&mut list).is_empty(), "no note sounds yet");

    add(
        &mut list,
        fx::note_on(group(0), ch(2), 64, fx::MID_VELOCITY),
    );
    let events = drain(&mut list);
    assert_eq!(events.len(), 3, "{events:?}");
    let (_, _, _, id) = note_on(&events[0]);
    let held: Vec<(i32, i32, f64)> = events[1..].iter().map(expression).collect();
    assert!(held.iter().all(|e| e.0 == id), "{held:?}");
    let value = |expression_id: i32| {
        held.iter()
            .find(|e| e.1 == expression_id)
            .unwrap_or_else(|| panic!("no expression {expression_id}: {held:?}"))
            .2
    };
    near(value(CLAP_NOTE_EXPRESSION_BRIGHTNESS), 0.5);
    near(value(CLAP_NOTE_EXPRESSION_PRESSURE), 0.25);
}

/// **Per-note Pitch is a tuning** (M2-104 section 7.4.15): the Registered
/// Per-Note Controller #3 (Pitch 7.25), and a Note On's Attribute Pitch 7.9,
/// are the semitones from the note's key: 60.25 on key 60 is +0.25, and an
/// attribute of 60.5 is +0.5.
#[test]
fn per_note_pitch_and_the_pitch_attribute_are_tunings() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    drain(&mut list);
    // 60.25 as 7.25 fixed point: 60 in the top 7 bits, a quarter in the 25.
    add(
        &mut list,
        fx::per_note_controller(group(0), ch(0), 60, 3, (60 << 25) | (1 << 23), true),
    );
    let events = drain(&mut list);
    assert_eq!(events.len(), 1, "{events:?}");
    let (_, expression_id, value) = expression(&events[0]);
    assert_eq!(expression_id, CLAP_NOTE_EXPRESSION_TUNING);
    near(value, 0.25);

    add(&mut list, note_on_pitched(group(0), ch(1), 60, 60.5));
    let events = drain(&mut list);
    assert_eq!(events.len(), 2, "{events:?}");
    let (_, _, _, id) = note_on(&events[0]);
    let (note_id, expression_id, value) = expression(&events[1]);
    assert_eq!((note_id, expression_id), (id, CLAP_NOTE_EXPRESSION_TUNING));
    near(value, 0.5);
}

/// **A per-note bend is scaled by the channel's RPN #00/07 sensitivity**
/// (M2-104 section 7.4.13): half scale over the ±48 default is 24 semitones,
/// and when the sensitivity changes to 12 the held bend is 6, sent again for
/// the sounding note. A Note On's own held bend and Attribute Pitch are one
/// tuning, their sum.
#[test]
fn a_per_note_bend_is_scaled_by_the_channels_sensitivity() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::note_on(group(0), ch(3), 60, fx::MID_VELOCITY),
    );
    let id = note_on(&drain(&mut list)[0]).3;

    add(
        &mut list,
        fx::per_note_pitch_bend(group(0), ch(3), 60, 0xC000_0000),
    );
    let events = drain(&mut list);
    assert_eq!(events.len(), 1);
    near(expression(&events[0]).2, 24.0);

    add(
        &mut list,
        fx::registered_rpn(
            group(0),
            ch(3),
            Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
            PitchBendSensitivity::of(12).to_rpn_bits(),
        ),
    );
    let events = drain(&mut list);
    assert_eq!(
        events.len(),
        1,
        "the sensitivity is consumed, the bend re-sent"
    );
    let (note_id, expression_id, value) = expression(&events[0]);
    assert_eq!((note_id, expression_id), (id, CLAP_NOTE_EXPRESSION_TUNING));
    near(value, 6.0);

    // Another channel of the Group keeps the default.
    add(
        &mut list,
        fx::note_on(group(0), ch(4), 60, fx::MID_VELOCITY),
    );
    drain(&mut list);
    add(
        &mut list,
        fx::per_note_pitch_bend(group(0), ch(4), 60, 0xC000_0000),
    );
    near(expression(&drain(&mut list)[0]).2, 24.0);

    // A held bend and an attribute pitch on the next Note On add up: +6 over
    // ch 3's 12, +0.5 from 60.5.
    add(&mut list, note_on_pitched(group(0), ch(3), 60, 60.5));
    let events = drain(&mut list);
    let tunings: Vec<f64> = events
        .iter()
        .filter(|e| matches!(e, ClapEvent::NoteExpression(_)))
        .map(|e| expression(e).2)
        .collect();
    assert_eq!(tunings.len(), 1, "one tuning for the note: {events:?}");
    near(tunings[0], 6.5);
}

/// **A Group's sensitivity is its own**: RPN #00/07 on Group 0 does not
/// change the range of the same channel in Group 1.
#[test]
fn the_sensitivity_is_per_group() {
    let mut list = InputEventList::new();
    list.set_note_ports(&[false, false]);
    add(
        &mut list,
        fx::registered_rpn(
            group(0),
            ch(0),
            Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
            PitchBendSensitivity::of(12).to_rpn_bits(),
        ),
    );
    for g in [0, 1] {
        add(
            &mut list,
            fx::note_on(group(g), ch(0), 60, fx::MID_VELOCITY),
        );
        add(
            &mut list,
            fx::per_note_pitch_bend(group(g), ch(0), 60, 0xC000_0000),
        );
    }
    let events = drain(&mut list);
    let tunings: Vec<f64> = events
        .iter()
        .filter(|e| matches!(e, ClapEvent::NoteExpression(_)))
        .map(|e| expression(e).2)
        .collect();
    assert_eq!(tunings.len(), 2);
    near(tunings[0], 6.0);
    near(tunings[1], 24.0);
}

/// A Per-Note Management Reset puts the note's controllers back: the held
/// values are gone for the next note, and the sounding note's tuning and
/// pressure return to 0.
#[test]
fn a_reset_returns_the_notes_expressions_to_their_defaults() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    let id = note_on(&drain(&mut list)[0]).3;
    add(
        &mut list,
        fx::poly_pressure(group(0), ch(0), 60, 0xFFFF_FFFF),
    );
    drain(&mut list);

    add(
        &mut list,
        fx::per_note_management(group(0), ch(0), 60, false, true),
    );
    let events = drain(&mut list);
    let reset: Vec<(i32, i32, f64)> = events.iter().map(expression).collect();
    assert!(reset.iter().all(|e| e.0 == id), "{reset:?}");
    let value = |expression_id: i32| reset.iter().find(|e| e.1 == expression_id).unwrap().2;
    near(value(CLAP_NOTE_EXPRESSION_TUNING), 0.0);
    near(value(CLAP_NOTE_EXPRESSION_PRESSURE), 0.0);

    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    assert_eq!(drain(&mut list).len(), 1, "the held pressure is gone");
}

/// A plugin reset forgets the held values and the notes.
#[test]
fn a_reset_forgets_the_per_note_state() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::poly_pressure(group(0), ch(0), 60, 0x8000_0000),
    );
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 61, fx::MID_VELOCITY),
    );
    drain(&mut list);
    list.reset_midi();
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    assert_eq!(
        drain(&mut list).len(),
        1,
        "no initial pressure after a reset"
    );
    add(
        &mut list,
        fx::poly_pressure(group(0), ch(0), 61, 0x8000_0000),
    );
    assert!(drain(&mut list).is_empty(), "the old note is forgotten");
}

fn rpn(group: MidiGroup, channel: MidiChannel, index: u8, value: u32) -> Ump {
    fx::registered_controller(group, channel, 0, index, value)
}

/// **An RPN is four Control Changes through the stateful narrower** (M2-104
/// Appendix D.2.3): CC 101 and 100 select, CC 6 and 38 carry the top 14 bits;
/// the select is not repeated for the same parameter, and is sent in full for
/// another channel, another Group (its port), and after a reset. The value
/// 12 << 25 is 12 in the MSB and 0 in the LSB (a zero-extended 7.25 number).
#[test]
fn an_rpn_is_four_control_changes_and_the_select_is_not_repeated() {
    let mut list = InputEventList::new();
    list.set_note_ports(&[false, false]);

    add(&mut list, rpn(group(0), ch(1), 0, 12 << 25));
    let bytes: Vec<_> = drain(&mut list).iter().map(midi_bytes).collect();
    assert_eq!(
        bytes,
        [
            (0, [0xB1, 101, 0]),
            (0, [0xB1, 100, 0]),
            (0, [0xB1, 6, 12]),
            (0, [0xB1, 38, 0]),
        ]
    );

    add(&mut list, rpn(group(0), ch(1), 0, 24 << 25));
    let bytes: Vec<_> = drain(&mut list).iter().map(midi_bytes).collect();
    assert_eq!(bytes, [(0, [0xB1, 6, 24]), (0, [0xB1, 38, 0])]);

    // The same channel number of Group 1 is another channel, on port 1.
    add(&mut list, rpn(group(1), ch(1), 0, 12 << 25));
    let bytes: Vec<_> = drain(&mut list).iter().map(midi_bytes).collect();
    assert_eq!(bytes.len(), 4, "{bytes:?}");
    assert!(bytes.iter().all(|b| b.0 == 1));

    // A reset: the plugin forgot its selection.
    list.reset_midi();
    add(&mut list, rpn(group(0), ch(1), 0, 12 << 25));
    assert_eq!(drain(&mut list).len(), 4);
}

/// **An NRPN is CC 99 and 98, then CC 6 and 38** (M2-104 Appendix D.2.3).
#[test]
fn an_nrpn_selects_with_cc_99_and_98() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::assignable_controller(group(0), ch(0), 5, 9, 0x8000_0000),
    );
    let bytes: Vec<_> = drain(&mut list).iter().map(midi_bytes).collect();
    assert_eq!(
        bytes,
        [
            (0, [0xB0, 99, 5]),
            (0, [0xB0, 98, 9]),
            (0, [0xB0, 6, 64]),
            (0, [0xB0, 38, 0]),
        ]
    );
}

/// **A bank is resent only when it changes** (M2-104 Appendix D.2.4): the
/// first banked Program Change is CC 0, CC 32 and the program; the same bank
/// again is the program alone; another bank is all three.
#[test]
fn a_bank_is_resent_only_when_it_changes() {
    let mut list = InputEventList::new();
    let bank = 5 << 7 | 9;
    add(
        &mut list,
        fx::program_change(group(0), ch(0), 3, Some(bank)),
    );
    let bytes: Vec<_> = drain(&mut list).iter().map(midi_bytes).collect();
    assert_eq!(
        bytes,
        [(0, [0xB0, 0, 5]), (0, [0xB0, 32, 9]), (0, [0xC0, 3, 0])]
    );
    add(
        &mut list,
        fx::program_change(group(0), ch(0), 4, Some(bank)),
    );
    let bytes: Vec<_> = drain(&mut list).iter().map(midi_bytes).collect();
    assert_eq!(bytes, [(0, [0xC0, 4, 0])]);
    add(
        &mut list,
        fx::program_change(group(0), ch(0), 4, Some(6 << 7)),
    );
    assert_eq!(drain(&mut list).len(), 3);
}

/// The ids a note is told by are not `(channel, key)` pairs: a Note On's id
/// is above `NoteId::MAX_CHANNEL_NOTE`, so it names no pair and a plugin never
/// confuses two notes of one key.
#[test]
fn a_minted_id_names_no_channel_and_key() {
    let mut list = InputEventList::new();
    add(
        &mut list,
        fx::note_on(group(0), ch(0), 60, fx::MID_VELOCITY),
    );
    let id = note_on(&drain(&mut list)[0]).3;
    assert!(u32::try_from(id).unwrap() > NoteId::MAX_CHANNEL_NOTE);
}
