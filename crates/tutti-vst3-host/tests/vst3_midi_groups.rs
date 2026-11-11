//! What a VST3 plugin's `getEvent` hands it of MIDI 2.0's Groups and per-note
//! state (M2-104 sections 2.1.2 and 7.4, Appendices C.3 and D.2): a Group is an
//! event bus, a note has a `noteId` of its own, per-note values told before a
//! Note On are its initial values, and an RPN or NRPN is Control Changes
//! through the stateful narrower. Every expectation is the spec's figure,
//! written out; the plugin's side of the calls is `unsafe` COM, so these live
//! here and not in the host's `src/`.

use tutti_fixtures::midi;
use tutti_graph::{Offset, UmpAt};
use tutti_midi_protocol::PitchBendSensitivity;
use tutti_types::Samples;
use tutti_ump::{
    Address, Attribute, Message, MidiChannel, MidiGroup, NoteId, NoteNumber, Rpn, Ump, Voice2,
};
use tutti_vst3_ffi::{EventKind, EventList};
use tutti_vst3_host::{InputEvents, Stager};
use vst3::Steinberg::Vst::{Event, IEventListTrait};
use vst3::Steinberg::kResultOk;

/// `kTuningTypeID`, `kVolumeTypeID`, `kPanTypeID`, `kBrightnessTypeID`.
const TUNING: u32 = 2;
const BRIGHTNESS: u32 = 5;

/// An event as the plugin reads it.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    NoteOn {
        bus: i32,
        channel: i16,
        pitch: i16,
        id: i32,
    },
    NoteOff {
        bus: i32,
        id: i32,
    },
    Pressure {
        id: i32,
        pressure: f32,
    },
    Expression {
        id: i32,
        type_id: u32,
        value: f64,
    },
    Data {
        bus: i32,
        bytes: Vec<u8>,
    },
}

fn at(k: usize) -> Offset {
    Offset::new(k, Samples(1 << 16)).expect("inside the block")
}

fn group(n: u8) -> MidiGroup {
    MidiGroup::new(n).expect("a Group")
}

fn ch(n: u8) -> MidiChannel {
    MidiChannel::of(n)
}

/// Stages `packets` as one block and reads the list back through
/// `IEventList::getEvent`, as a plugin does.
fn block(stager: &mut Stager, packets: &[Ump]) -> Vec<Seen> {
    let events: Vec<UmpAt> = packets.iter().map(|&ump| (at(0), ump)).collect();
    let list = EventList::input();
    assert!(stager.stage(
        &list,
        &InputEvents {
            midi: &events,
            ..Default::default()
        }
    ));
    let mut seen = Vec::new();
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct, and
    // `getEvent` fills it.
    unsafe {
        for i in 0..list.getEventCount() {
            let mut e: Event = std::mem::zeroed();
            assert_eq!(list.getEvent(i, &mut e), kResultOk);
            let bus = e.busIndex;
            seen.push(match e.r#type {
                t if t == EventKind::NoteOn as u16 => {
                    let n = e.__field0.noteOn;
                    Seen::NoteOn {
                        bus,
                        channel: n.channel,
                        pitch: n.pitch,
                        id: n.noteId,
                    }
                }
                t if t == EventKind::NoteOff as u16 => Seen::NoteOff {
                    bus,
                    id: e.__field0.noteOff.noteId,
                },
                t if t == EventKind::PolyPressure as u16 => {
                    let p = e.__field0.polyPressure;
                    Seen::Pressure {
                        id: p.noteId,
                        pressure: p.pressure,
                    }
                }
                t if t == EventKind::NoteExpressionValue as u16 => {
                    let x = e.__field0.noteExpressionValue;
                    Seen::Expression {
                        id: x.noteId,
                        type_id: x.typeId,
                        value: x.value,
                    }
                }
                t if t == EventKind::Data as u16 => {
                    let d = e.__field0.data;
                    Seen::Data {
                        bus,
                        bytes: std::slice::from_raw_parts(d.bytes, d.size as usize).to_vec(),
                    }
                }
                other => panic!("an unexpected event type {other}"),
            });
        }
    }
    seen
}

fn note_on_id(seen: &Seen) -> i32 {
    match seen {
        Seen::NoteOn { id, .. } => *id,
        other => panic!("expected a Note On, got {other:?}"),
    }
}

fn near(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-4, "{got} vs {want}");
}

fn note_on_pitched(group: MidiGroup, channel: MidiChannel, key: u8, pitch: f64) -> Ump {
    Ump::from(Message::Midi2(
        Address { group, channel },
        Voice2::NoteOn {
            note: NoteNumber::of(key),
            velocity: midi::MID_VELOCITY,
            attribute: Attribute::Pitch((pitch * 512.0) as u16),
        },
    ))
}

/// **A Group is an event bus, and Groups are not folded into channels**
/// (M2-104 section 2.1.2, Appendix B.1): Group 0 and Group 1 each play channel
/// 0, key 60: two notes, on buses 0 and 1, channel 0 both, with ids of their
/// own; and a controller of Group 1 is a Data event on bus 1.
#[test]
fn two_groups_land_on_event_buses_zero_and_one() {
    let mut stager = Stager::new(2);
    let seen = block(
        &mut stager,
        &[
            midi::note_on(group(0), ch(0), 60, midi::MID_VELOCITY),
            midi::note_on(group(1), ch(0), 60, midi::MID_VELOCITY),
            midi::cc(
                group(1),
                ch(0),
                tutti_ump::CCNumber::BRIGHTNESS,
                midi::wide_value(100),
            ),
        ],
    );
    assert_eq!(seen.len(), 3, "{seen:?}");
    let (id0, id1) = (note_on_id(&seen[0]), note_on_id(&seen[1]));
    assert_ne!(id0, id1);
    assert!(matches!(
        seen[0],
        Seen::NoteOn {
            bus: 0,
            channel: 0,
            pitch: 60,
            ..
        }
    ));
    assert!(matches!(
        seen[1],
        Seen::NoteOn {
            bus: 1,
            channel: 0,
            pitch: 60,
            ..
        }
    ));
    assert_eq!(
        seen[2],
        Seen::Data {
            bus: 1,
            bytes: vec![0xB0, 74, 100]
        }
    );
    assert_eq!(stager.groups_dropped(), 0);
}

/// **A Group the plugin has no event bus for is dropped and counted**, whole,
/// and not heard on Group 0's channels. A message that names no Group's
/// channels (System Real Time) still goes to bus 0 and is not counted.
#[test]
fn a_group_without_an_event_bus_is_dropped_and_counted() {
    let mut stager = Stager::new(1);
    let seen = block(
        &mut stager,
        &[
            midi::note_on(group(0), ch(0), 60, midi::MID_VELOCITY),
            midi::note_on(group(1), ch(0), 61, midi::MID_VELOCITY),
            midi::cc(
                group(2),
                ch(0),
                tutti_ump::CCNumber::BRIGHTNESS,
                midi::wide_value(1),
            ),
            midi::timing_clock(group(1)),
        ],
    );
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(matches!(
        seen[0],
        Seen::NoteOn {
            bus: 0,
            pitch: 60,
            ..
        }
    ));
    assert_eq!(
        seen[1],
        Seen::Data {
            bus: 0,
            bytes: vec![0xF8, 0, 0]
        }
    );
    assert_eq!(
        stager.groups_dropped(),
        2,
        "the Group 1 note and the Group 2 CC"
    );
}

/// **A Detach ends the note on a key, and the next Note On is another note**
/// (M2-104 section 7.4.5): its `noteId` is new, and a per-note controller after
/// it reaches the new note alone.
#[test]
fn a_detach_then_a_note_on_is_a_new_note_that_alone_hears_the_controller() {
    let mut stager = Stager::new(1);
    let first = note_on_id(&block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)])[0]);
    let before = block(
        &mut stager,
        &[midi::poly_pressure(group(0), ch(0), 60, 0x8000_0000)],
    );
    assert!(
        matches!(before[..], [Seen::Pressure { id, .. }] if id == first),
        "{before:?}"
    );

    assert!(
        block(
            &mut stager,
            &[midi::per_note_management(group(0), ch(0), 60, true, false)]
        )
        .is_empty()
    );
    let second = note_on_id(&block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)])[0]);
    assert_ne!(second, first);

    let after = block(
        &mut stager,
        &[midi::poly_pressure(group(0), ch(0), 60, 0x4000_0000)],
    );
    assert!(
        matches!(after[..], [Seen::Pressure { id, .. }] if id == second),
        "{after:?}"
    );
}

/// A Note Off names no note (`-1`): the plugin matches it by channel and pitch.
#[test]
fn a_note_off_names_no_note() {
    let mut stager = Stager::new(1);
    block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)]);
    let seen = block(&mut stager, &[midi::note_off(group(0), ch(0), 60, 0)]);
    assert_eq!(seen, [Seen::NoteOff { bus: 0, id: -1 }]);
}

/// **Per-note values sent before a Note On are the note's initial values**
/// (M2-104 Appendix C.3): nothing is sent while no note sounds, and the Note On
/// is followed by each held value as an event of the new note.
#[test]
fn values_told_before_a_note_on_are_its_initial_events() {
    let mut stager = Stager::new(1);
    let held = block(
        &mut stager,
        &[
            midi::per_note_controller(group(0), ch(2), 64, 74, 0x8000_0000, true),
            midi::poly_pressure(group(0), ch(2), 64, 0x4000_0000),
        ],
    );
    assert!(held.is_empty(), "no note sounds yet: {held:?}");

    let seen = block(&mut stager, &[midi::note_on(group(0), ch(2), 64, 0x8000)]);
    assert_eq!(seen.len(), 3, "{seen:?}");
    let id = note_on_id(&seen[0]);
    let mut brightness = None;
    let mut pressure = None;
    for event in &seen[1..] {
        match event {
            Seen::Expression {
                id: note,
                type_id: BRIGHTNESS,
                value,
            } => {
                assert_eq!(*note, id);
                brightness = Some(*value);
            }
            Seen::Pressure {
                id: note,
                pressure: p,
            } => {
                assert_eq!(*note, id);
                pressure = Some(*p);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    near(brightness.expect("brightness"), 0.5);
    near(f64::from(pressure.expect("pressure")), 0.25);
}

/// **Per-note Pitch is a tuning** (M2-104 section 7.4.15): Registered Per-Note
/// Controller #3 (Pitch 7.25) and a Note On's Attribute Pitch 7.9 are the
/// semitones from the note's key on VST3's tuning scale (`0.5 + st / 240`):
/// 60.25 on key 60 is +0.25 st, and an attribute of 60.5 is +0.5 st.
#[test]
fn per_note_pitch_and_the_pitch_attribute_are_tunings() {
    let mut stager = Stager::new(1);
    block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)]);
    let seen = block(
        &mut stager,
        &[midi::per_note_controller(
            group(0),
            ch(0),
            60,
            3,
            (60 << 25) | (1 << 23),
            true,
        )],
    );
    let [Seen::Expression { type_id, value, .. }] = seen[..] else {
        panic!("{seen:?}");
    };
    assert_eq!(type_id, TUNING);
    near(value, 0.5 + 0.25 / 240.0);

    let seen = block(&mut stager, &[note_on_pitched(group(0), ch(1), 60, 60.5)]);
    assert_eq!(seen.len(), 2, "{seen:?}");
    let id = note_on_id(&seen[0]);
    let Seen::Expression {
        id: note,
        type_id,
        value,
    } = seen[1]
    else {
        panic!("{seen:?}");
    };
    assert_eq!((note, type_id), (id, TUNING));
    near(value, 0.5 + 0.5 / 240.0);
}

/// **A per-note bend is scaled by the channel's RPN #00/07 sensitivity** (M2-104
/// section 7.4.13), on VST3's tuning scale: half scale over the ±48 default is 24
/// semitones, and when the sensitivity becomes 12 the held bend is 6 and is told
/// again for the sounding note; the RPN itself is consumed.
#[test]
fn a_per_note_bend_is_scaled_by_the_channels_sensitivity() {
    let mut stager = Stager::new(1);
    let id = note_on_id(&block(&mut stager, &[midi::note_on(group(0), ch(3), 60, 0x8000)])[0]);

    let seen = block(
        &mut stager,
        &[midi::per_note_pitch_bend(group(0), ch(3), 60, 0xC000_0000)],
    );
    let [Seen::Expression { value, .. }] = seen[..] else {
        panic!("{seen:?}");
    };
    near(value, 0.5 + 24.0 / 240.0);

    let seen = block(
        &mut stager,
        &[midi::registered_rpn(
            group(0),
            ch(3),
            Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
            PitchBendSensitivity::of(12).to_rpn_bits(),
        )],
    );
    let [
        Seen::Expression {
            id: note,
            type_id,
            value,
        },
    ] = seen[..]
    else {
        panic!("the RPN is consumed and the bend told again: {seen:?}");
    };
    assert_eq!((note, type_id), (id, TUNING));
    near(value, 0.5 + 6.0 / 240.0);
}

/// **One tuning is the sounding pitch**: a Pitch told to a sounding note and
/// then a bend are one tuning, their sum from the key, in either order: key 60
/// told 60.5, then half a bend over ±48, is +24.5 semitones.
#[test]
fn a_pitch_then_a_bend_is_one_tuning_from_the_key() {
    for bend_first in [false, true] {
        let mut stager = Stager::new(1);
        block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)]);
        let pitch = midi::per_note_controller(group(0), ch(0), 60, 3, (60 << 25) | (1 << 24), true);
        let bend = midi::per_note_pitch_bend(group(0), ch(0), 60, 0xC000_0000);
        let (first, second) = if bend_first {
            (bend, pitch)
        } else {
            (pitch, bend)
        };
        block(&mut stager, &[first]);
        let seen = block(&mut stager, &[second]);
        let [Seen::Expression { value, .. }] = seen[..] else {
            panic!("{seen:?}");
        };
        near(value, 0.5 + 24.5 / 240.0);
    }
}

/// A Per-Note Management Reset returns the sounding note's tuning to centre
/// and its pressure to 0.
#[test]
fn a_reset_returns_the_notes_values_to_their_defaults() {
    let mut stager = Stager::new(1);
    let id = note_on_id(&block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)])[0]);
    block(
        &mut stager,
        &[midi::poly_pressure(group(0), ch(0), 60, u32::MAX)],
    );
    let seen = block(
        &mut stager,
        &[midi::per_note_management(group(0), ch(0), 60, false, true)],
    );
    let tuning = seen.iter().find_map(|e| match e {
        Seen::Expression {
            id: note,
            type_id: TUNING,
            value,
        } if *note == id => Some(*value),
        _ => None,
    });
    near(tuning.expect("a tuning reset"), 0.5);
    assert!(
        seen.iter()
            .any(|e| matches!(e, Seen::Pressure { id: note, pressure } if *note == id && *pressure == 0.0)),
        "{seen:?}"
    );
    // The held pressure is gone for the next note.
    let next = block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)]);
    assert_eq!(next.len(), 1, "{next:?}");
}

/// A stop or reset of the plugin forgets the notes and the held values.
#[test]
fn a_reset_forgets_the_per_note_state() {
    let mut stager = Stager::new(1);
    block(
        &mut stager,
        &[midi::poly_pressure(group(0), ch(0), 60, 0x8000_0000)],
    );
    block(&mut stager, &[midi::note_on(group(0), ch(0), 61, 0x8000)]);
    stager.reset();
    let seen = block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)]);
    assert_eq!(seen.len(), 1, "no initial pressure after a reset: {seen:?}");
    let seen = block(
        &mut stager,
        &[midi::poly_pressure(group(0), ch(0), 61, 0x8000_0000)],
    );
    assert!(seen.is_empty(), "the old note is forgotten: {seen:?}");
}

fn data(bus: i32, bytes: [u8; 3]) -> Seen {
    Seen::Data {
        bus,
        bytes: bytes.to_vec(),
    }
}

/// **An RPN is four Control Changes through the stateful narrower** (M2-104
/// Appendix D.2.3): CC 101 and 100 select, CC 6 and 38 carry the top 14 bits;
/// the select is not repeated for the same parameter, and is sent in full for
/// the same channel of another Group (another bus) and after a reset.
#[test]
fn an_rpn_is_four_control_changes_and_the_select_is_not_repeated() {
    let rpn = |g: u8, value: u32| midi::registered_controller(group(g), ch(1), 0, 0, value);
    let mut stager = Stager::new(2);
    assert_eq!(
        block(&mut stager, &[rpn(0, 12 << 25)]),
        [
            data(0, [0xB1, 101, 0]),
            data(0, [0xB1, 100, 0]),
            data(0, [0xB1, 6, 12]),
            data(0, [0xB1, 38, 0]),
        ]
    );
    assert_eq!(
        block(&mut stager, &[rpn(0, 24 << 25)]),
        [data(0, [0xB1, 6, 24]), data(0, [0xB1, 38, 0])]
    );
    let other_group = block(&mut stager, &[rpn(1, 12 << 25)]);
    assert_eq!(other_group.len(), 4, "{other_group:?}");
    assert!(
        other_group
            .iter()
            .all(|e| matches!(e, Seen::Data { bus: 1, .. }))
    );

    stager.reset();
    assert_eq!(block(&mut stager, &[rpn(0, 12 << 25)]).len(), 4);
}

/// **A bank is resent only when it changes** (M2-104 Appendix D.2.4).
#[test]
fn a_bank_is_resent_only_when_it_changes() {
    let mut stager = Stager::new(1);
    let bank = 5 << 7 | 9;
    assert_eq!(
        block(
            &mut stager,
            &[midi::program_change(group(0), ch(0), 3, Some(bank))]
        ),
        [
            data(0, [0xB0, 0, 5]),
            data(0, [0xB0, 32, 9]),
            data(0, [0xC0, 3, 0])
        ]
    );
    assert_eq!(
        block(
            &mut stager,
            &[midi::program_change(group(0), ch(0), 4, Some(bank))]
        ),
        [data(0, [0xC0, 4, 0])]
    );
    assert_eq!(
        block(
            &mut stager,
            &[midi::program_change(group(0), ch(0), 4, Some(6 << 7))]
        )
        .len(),
        3
    );
}

/// A minted id names no `(channel, key)` pair.
#[test]
fn a_minted_id_is_above_every_channel_and_key_pair() {
    let mut stager = Stager::new(1);
    let id = note_on_id(&block(&mut stager, &[midi::note_on(group(0), ch(0), 60, 0x8000)])[0]);
    assert!(u32::try_from(id).unwrap() > NoteId::MAX_CHANNEL_NOTE);
}
