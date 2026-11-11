//! `Voicing::step` allocates nothing, on every path: poly stealing, the
//! exclusive cut, mono and legato takeover, the return to a held key, the
//! pedals, the panics, the bends and the per-note controllers.

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_midi_protocol::{MpeZoneConfig, MpeZones};
use tutti_types::{Amplitude, Semitones};
use tutti_ump::{
    Attribute, CCNumber, MidiChannel, NoteNumber, RegisteredPerNoteController, U7, Voice2,
};
use tutti_voicing::{
    Command, Mode, Order, Patch, Retrigger, Rules, Steal, VoiceSpec, VoiceView, Voicing,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Two layers, one exclusive class.
struct Grouped;
impl Patch for Grouped {
    fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
        for tag in 0..2 {
            spec(VoiceSpec {
                group: 7,
                scope: 1,
                tag,
            });
        }
    }
}

fn rules(mode: Mode) -> Rules {
    Rules {
        mode,
        retrigger: Retrigger::Replace,
        polyphony: 6,
        steal: Steal::By(Order::Quietest),
        exclusive_groups: true,
        sostenuto: true,
        bend_range: Semitones(2.0),
        per_note_bend_range: Semitones(48.0),
    }
}

#[test]
fn step_is_allocation_free_in_every_mode() {
    for mode in [Mode::Poly, Mode::Mono, Mode::Legato] {
        let mut core = Voicing::new(rules(mode));
        core.set_zones(MpeZones::new(MpeZoneConfig::lower(3), None));
        let mut voices: Vec<VoiceView> = Vec::with_capacity(4096);
        let ch = |n| MidiChannel::of(n);
        let cc = |n: u8, v: u32| Voice2::ControlChange {
            controller: CCNumber::of(n),
            value: v << 25,
        };
        assert_no_alloc(|| {
            for round in 0..300u32 {
                let k = 40 + (round * 7 % 30) as u8;
                let c = ch((round % 3) as u8);
                let msgs = [
                    Voice2::NoteOn {
                        note: NoteNumber::of(k),
                        velocity: 0x8000,
                        attribute: Attribute::None,
                    },
                    Voice2::NoteOn {
                        note: NoteNumber::of(k + 3),
                        velocity: 0x4000,
                        attribute: Attribute::None,
                    },
                    Voice2::NoteOff {
                        note: NoteNumber::of(k + 3),
                        velocity: 0x8000,
                        attribute: Attribute::None,
                    },
                    // Per-note state: stored before and during a note, set
                    // by every message kind, detached, reset, and a
                    // sensitivity that moves the bends held.
                    Voice2::RegisteredPerNote {
                        note: NoteNumber::of(k),
                        controller: RegisteredPerNoteController::Pitch,
                        value: 60 << 25,
                    },
                    Voice2::PerNotePitchBend {
                        note: NoteNumber::of(k),
                        value: round.wrapping_mul(0x0101_0101),
                    },
                    Voice2::PolyPressure {
                        note: NoteNumber::of(k),
                        value: 1 << 30,
                    },
                    Voice2::AssignablePerNote {
                        note: NoteNumber::of(k),
                        controller: 74,
                        value: 1 << 30,
                    },
                    Voice2::Registered {
                        bank: U7::of(0),
                        index: U7::of(7),
                        value: (round % 48) << 25,
                    },
                    Voice2::PerNoteManagement {
                        note: NoteNumber::of(k),
                        detach: round % 2 == 0,
                        reset: round % 3 == 0,
                    },
                    Voice2::NoteOn {
                        note: NoteNumber::of(k),
                        velocity: 0x8000,
                        attribute: Attribute::Pitch(60 << 9),
                    },
                    cc(64, round % 2 * 127),
                    cc(66, round % 3 / 2 * 127),
                    Voice2::PitchBend(round.wrapping_mul(0x0101_0101)),
                    Voice2::NoteOff {
                        note: NoteNumber::of(k),
                        velocity: 0x8000,
                        attribute: Attribute::None,
                    },
                    cc([120, 121, 123, 124][(round % 4) as usize], 0),
                ];
                for m in &msgs {
                    // The instrument's side, without allocating: the views
                    // are rebuilt from what the commands left.
                    for cmd in core.step(c, m, &Grouped, &voices) {
                        match *cmd {
                            Command::Start { id, steal, .. } => {
                                voices.retain(|v| Some(v.id) != steal);
                                voices.push(VoiceView {
                                    id,
                                    level: Amplitude(0.5),
                                });
                            }
                            Command::Cut(id) => voices.retain(|v| v.id != id),
                            _ => {}
                        }
                    }
                }
            }
        });
    }
}
