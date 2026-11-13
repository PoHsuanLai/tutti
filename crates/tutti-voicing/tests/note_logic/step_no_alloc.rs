//! `Voicing::step` allocates nothing, on every path: poly stealing, the
//! exclusive cut, mono and legato takeover, the return to a held key, the
//! pedals, the panics, the bends and the per-note controllers.

use std::num::{NonZeroU16, NonZeroUsize};

use assert_no_alloc::assert_no_alloc;
use tutti_midi_protocol::{
    Addressee, ChannelMask, GeneralMidi, Heard, KeyChange, KeyChanges, KeyTable, MpeZoneConfig,
    MpeZones, OctaveOffsets, Timing, TuningFrequency, TuningMessage, TuningName, Universal,
};
use tutti_types::{Amplitude, Semitones};
use tutti_ump::{
    Attribute, CCNumber, MidiChannel, NoteNumber, NoteVelocity, RegisteredPerNoteController, Rpn,
    U7, U14, Voice2,
};
use tutti_ump::{BendValue, ControlValue};
use tutti_voicing::{
    Command, Mode, Order, Patch, PerNoteState, Rules, Steal, TuningCapacity, VoiceId, VoiceSpec,
    Voicing,
};

/// Two layers, one exclusive class.
struct Grouped;
impl Patch for Grouped {
    fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
        for tag in 0..2 {
            spec(VoiceSpec {
                group: NonZeroU16::new(7),
                scope: 1,
                tag,
            });
        }
    }
}

fn rules(mode: Mode) -> Rules {
    Rules::poly(NonZeroUsize::new(6).unwrap())
        .with_mode(mode)
        .with_steal(Steal::By(Order::Quietest))
        .with_exclusive_groups(true)
}

#[test]
fn step_is_allocation_free_in_every_mode() {
    for mode in [Mode::Poly, Mode::Mono, Mode::Legato] {
        let mut core = Voicing::new(rules(mode));
        core.set_zones(MpeZones::new(MpeZoneConfig::lower(3), None));
        let mut voices: Vec<(VoiceId, Amplitude)> = Vec::with_capacity(4096);
        let ch = |n| MidiChannel::of(n);
        let cc = |n: u8, v: u32| Voice2::ControlChange {
            controller: CCNumber::of(n),
            value: ControlValue::new(v << 25),
        };
        assert_no_alloc(|| {
            for round in 0..300u32 {
                let k = 40 + (round * 7 % 30) as u8;
                let c = ch((round % 3) as u8);
                let msgs = [
                    Voice2::NoteOn {
                        note: NoteNumber::of(k),
                        velocity: NoteVelocity::CENTER,
                        attribute: Attribute::None,
                    },
                    Voice2::NoteOn {
                        note: NoteNumber::of(k + 3),
                        velocity: NoteVelocity::new(0x4000),
                        attribute: Attribute::None,
                    },
                    Voice2::NoteOff {
                        note: NoteNumber::of(k + 3),
                        velocity: NoteVelocity::CENTER,
                        attribute: Attribute::None,
                    },
                    // Per-note state: stored before and during a note, set
                    // by every message kind, detached, reset, and a
                    // sensitivity that moves the bends held.
                    Voice2::RegisteredPerNote {
                        note: NoteNumber::of(k),
                        controller: RegisteredPerNoteController::Pitch,
                        value: ControlValue::new(60 << 25),
                    },
                    Voice2::PerNotePitchBend {
                        note: NoteNumber::of(k),
                        value: BendValue::new(round.wrapping_mul(0x0101_0101)),
                    },
                    Voice2::PolyPressure {
                        note: NoteNumber::of(k),
                        value: ControlValue::new(1 << 30),
                    },
                    Voice2::AssignablePerNote {
                        note: NoteNumber::of(k),
                        controller: 74,
                        value: ControlValue::new(1 << 30),
                    },
                    Voice2::Registered {
                        bank: U7::of(0),
                        index: U7::of(7),
                        value: ControlValue::new((round % 48) << 25),
                    },
                    Voice2::PerNoteManagement {
                        note: NoteNumber::of(k),
                        detach: round % 2 == 0,
                        reset: round % 3 == 0,
                    },
                    Voice2::NoteOn {
                        note: NoteNumber::of(k),
                        velocity: NoteVelocity::CENTER,
                        attribute: Attribute::Pitch(60 << 9),
                    },
                    cc(64, round % 2 * 127),
                    cc(66, round % 3 / 2 * 127),
                    // The channel's controllers, its portamento and legato
                    // switches, a Portamento Control, the channel's registered
                    // parameters and pressure.
                    cc(1, round % 128),
                    cc(33, round % 128),
                    cc(5, round % 128),
                    cc(7, round % 128),
                    cc(10, round % 128),
                    cc(11, round % 128),
                    cc(65, round % 2 * 127),
                    cc(68, round % 3 / 2 * 127),
                    cc(84, k.into()),
                    Voice2::ChannelPressure(ControlValue::new(round.wrapping_mul(0x0101_0101))),
                    Voice2::Registered {
                        bank: U7::of(0),
                        index: U7::of([1, 2, 5][(round % 3) as usize]),
                        value: ControlValue::new((round % 128) << 25),
                    },
                    Voice2::PitchBend(BendValue::new(round.wrapping_mul(0x0101_0101))),
                    Voice2::NoteOff {
                        note: NoteNumber::of(k),
                        velocity: NoteVelocity::CENTER,
                        attribute: Attribute::None,
                    },
                    cc([120, 121, 123, 124][(round % 4) as usize], 0),
                ];
                if round % 97 == 96 {
                    // `reset` clears the per-note state in place.
                    core.reset();
                }
                for &m in &msgs {
                    // The instrument's side, without allocating: the views
                    // are rebuilt from what the commands left.
                    for cmd in core.step(c, m, &Grouped, |id| {
                        voices.iter().find(|v| v.0 == id).map(|v| v.1)
                    }) {
                        match *cmd {
                            Command::Start { id, steal, .. } => {
                                voices.retain(|v| Some(v.0) != steal);
                                voices.push((id, Amplitude(0.5)));
                            }
                            Command::Cut(id) => voices.retain(|v| v.0 != id),
                            _ => {}
                        }
                    }
                }
            }
        });
    }
}

/// `PerNoteState` allocates nothing after `new`, on every path: a start with
/// its held values and attribute, each controller, Per-Note Management, a
/// sensitivity that resizes a channel's bends, `clear`, and across Groups.
#[test]
fn per_note_state_is_allocation_free() {
    use tutti_ump::{Address, MidiGroup, RegisteredPerNoteController, U7};
    let mut state = PerNoteState::new(16, Semitones(48.0));
    let mut twin = state.clone();
    assert_no_alloc(|| {
        for round in 0..400u32 {
            let address = Address {
                group: MidiGroup::new((round % 16) as u8).unwrap(),
                channel: MidiChannel::of((round % 5) as u8),
            };
            let note = NoteNumber::of((round % 128) as u8);
            let msgs = [
                Voice2::RegisteredPerNote {
                    note,
                    controller: RegisteredPerNoteController::Pitch,
                    value: ControlValue::new(60 << 25),
                },
                Voice2::AssignablePerNote {
                    note,
                    controller: (round % 9) as u8,
                    value: ControlValue::new(1 << 30),
                },
                Voice2::PerNotePitchBend {
                    note,
                    value: BendValue::new(round.wrapping_mul(0x0101_0101)),
                },
                Voice2::PolyPressure {
                    note,
                    value: ControlValue::new(1 << 30),
                },
                Voice2::NoteOn {
                    note,
                    velocity: NoteVelocity::CENTER,
                    attribute: Attribute::Pitch(60 << 9),
                },
                Voice2::PerNoteManagement {
                    note,
                    detach: round % 2 == 0,
                    reset: round % 3 == 0,
                },
                Voice2::Registered {
                    bank: U7::of(0),
                    index: U7::of(7),
                    value: ControlValue::new((round % 48) << 25),
                },
            ];
            for &m in &msgs {
                std::hint::black_box(state.step(address, m));
                std::hint::black_box(twin.step(address, m));
            }
            std::hint::black_box(state.start(address, note, Attribute::None));
            state.end(tutti_ump::NoteId::from_raw(
                tutti_ump::NoteId::MAX_CHANNEL_NOTE + 1 + round,
            ));
            if round % 131 == 130 {
                state.clear();
            }
        }
    });
}

/// The Universal messages: master volume, balance and tuning, and a General
/// MIDI reset that cuts every voice, allocate nothing.
#[test]
fn universal_is_allocation_free() {
    let mut core = Voicing::new(
        Rules::poly(NonZeroUsize::new(6).unwrap()).with_percussion(MidiChannel::of(9)),
    );
    let on = |k: u8| Voice2::NoteOn {
        note: NoteNumber::of(k),
        velocity: NoteVelocity::CENTER,
        attribute: Attribute::None,
    };
    assert_no_alloc(|| {
        for round in 0..200u16 {
            for k in 40..46 {
                let level = |_: VoiceId| Some(Amplitude(1.0));
                std::hint::black_box(core.step(MidiChannel::of(0), on(k), &Grouped, level));
            }
            for message in [
                Universal::MasterVolume(U14::masked(round)),
                Universal::MasterBalance(U14::masked(round * 3)),
                Universal::MasterFineTuning(U14::masked(round * 7)),
                Universal::MasterCoarseTuning(U7::of((round % 128) as u8)),
                Universal::IdentityRequest,
                Universal::GeneralMidi(if round % 2 == 0 {
                    GeneralMidi::On
                } else {
                    GeneralMidi::Off
                }),
            ] {
                let to = if round % 3 == 0 {
                    Addressee::Own
                } else {
                    Addressee::All
                };
                std::hint::black_box(core.universal(Heard { to, message }));
            }
            std::hint::black_box(core.master_gain());
        }
    });
}

/// The MIDI Tuning Standard on the audio path: every message (real time and
/// not, for programs the core holds and ones it refuses, dumps, both Scale/Octave
/// forms), the Tuning Program and Bank Select that retunes sounding notes, and
/// the notes that start and move under them allocate nothing, in every mode.
#[test]
fn mts_is_allocation_free() {
    for mode in [Mode::Poly, Mode::Mono, Mode::Legato] {
        let mut core = Voicing::new(
            Rules::poly(NonZeroUsize::new(6).unwrap())
                .with_mode(mode)
                .with_percussion(MidiChannel::of(9))
                .with_tuning_capacity(TuningCapacity::new(2, 8).unwrap()),
        );
        let on = |k: u8| Voice2::NoteOn {
            note: NoteNumber::of(k),
            velocity: NoteVelocity::CENTER,
            attribute: Attribute::None,
        };
        let select = |rpn: Rpn, v: u8| Voice2::Registered {
            bank: rpn.bank(),
            index: rpn.index(),
            value: ControlValue::new(u32::from(v) << 25),
        };
        let name = TuningName::new([U7::of(b'm'); 16]);
        let mut table = KeyTable::equal_temperament();
        table.set(NoteNumber::of(44), TuningFrequency::of(50 << 14 | 77));
        assert_no_alloc(|| {
            for round in 0..200u32 {
                let r = round as u8;
                let mut changes = KeyChanges::new();
                for k in 0..(round % 20) as u8 {
                    changes.push(KeyChange {
                        key: NoteNumber::of(40 + k),
                        frequency: TuningFrequency::of(((40 + u32::from(k)) << 14) | round),
                    });
                }
                let offsets = if round % 2 == 0 {
                    OctaveOffsets::Cents([U7::of(r % 128); 12])
                } else {
                    OctaveOffsets::Fine([U14::masked(round as u16 * 97); 12])
                };
                let timing = if round % 3 == 0 {
                    Timing::NonRealTime
                } else {
                    Timing::RealTime
                };
                let channel = MidiChannel::of((round % 3) as u8);
                for k in 40..46 {
                    let level = |_: VoiceId| Some(Amplitude(1.0));
                    std::hint::black_box(core.step(channel, on(k), &Grouped, level));
                }
                for (rpn, v) in [
                    (Rpn::TUNING_PROGRAM_SELECT, r % 10),
                    (Rpn::TUNING_BANK_SELECT, r % 3),
                ] {
                    let level = |_: VoiceId| Some(Amplitude(1.0));
                    std::hint::black_box(core.step(channel, select(rpn, v), &Grouped, level));
                }
                for message in [
                    TuningMessage::NoteChange {
                        timing,
                        bank: (round % 4 != 0).then_some(U7::of(r % 3)),
                        program: U7::of(r % 10),
                        changes,
                    },
                    TuningMessage::OctaveTuning {
                        timing,
                        channels: ChannelMask::from_bits(round as u16),
                        offsets,
                    },
                    TuningMessage::KeyDump {
                        bank: Some(U7::of(r % 3)),
                        program: U7::of(r % 9),
                        name,
                        table,
                    },
                    TuningMessage::OctaveDump {
                        bank: U7::of(r % 3),
                        program: U7::of(r % 9),
                        name,
                        offsets,
                    },
                    TuningMessage::DumpRequest {
                        bank: None,
                        program: U7::of(r % 128),
                    },
                ] {
                    std::hint::black_box(core.universal(Heard {
                        to: Addressee::All,
                        message: Universal::Tuning(message),
                    }));
                }
            }
        });
        assert!(core.tuning_refused() > 0, "the refusals were exercised");
    }
}
