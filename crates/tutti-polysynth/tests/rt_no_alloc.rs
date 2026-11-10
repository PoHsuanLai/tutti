//! Regression gate: the synth's `Node::process` must not allocate.
//!
//! PolySynth is the umbrella audio-callback unit for the in-tree
//! polyphonic synth. It owns:
//! - a `Vec<SynthVoice>` (allocated once on build),
//! - a `Vec` of finished-voice indices (sized once, to `max_voices`),
//! - and a `mix_buffer` scalar pair.
//!
//! A block takes its event input's MIDI, iterates active voices, and mixes
//! into the output. Driven by hand through `support::Hand` (the graph's own
//! by-hand driver, `tutti_graph::contract::Direct`, underneath). A regression that collects the
//! block's events at runtime, or that frees the finished-indices buffer on the audio
//! thread, would land here.
//!
//! Note that most tests below run at `max_voices: 8`, so they exercise the
//! steady state but *not* the finished-indices list's worst case: every
//! voice finishing in one block, which fills the list to the `max_voices`
//! capacity it was sized with. `polysynth_all_voices_finishing_together_is_allocation_free`
//! is the one that fills it, at 64 voices. The constructor puts no upper
//! bound on `max_voices` (only 0 is refused).

use assert_no_alloc::AllocDisabler;
mod support;

use support::Hand;
use tutti_fixtures::midi;
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_polysynth::{FilterType, OscillatorType, PolySynth, SynthConfig};
use tutti_types::{Hz, Q, SampleRate};
use tutti_ump::{
    Address, Attribute, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber,
    RegisteredPerNoteController, U7, U14, Ump, Voice1, Voice2,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(channel: u8, note: u8, vel: u8) -> Ump {
    midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        note,
        midi1_velocity_to_midi2(vel),
    )
}

fn note_off(channel: u8, note: u8) -> Ump {
    midi::note_off(MidiGroup::FIRST, MidiChannel::of(channel), note, 0)
}

#[test]
fn polysynth_process_idle_is_allocation_free() {
    // No active voices — process should still tick through allocator
    // bookkeeping and the MIDI inbox poll.
    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            sample_rate: tutti_types::SampleRate::from(48_000.0),
            max_voices: 8,
            oscillator: OscillatorType::Saw,
            ..Default::default()
        })
        .unwrap(),
    );

    for _ in 0..16 {
        synth.block(64);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            synth.block(64);
        }
    });
}

#[test]
fn polysynth_process_with_active_voices_is_allocation_free() {
    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            sample_rate: tutti_types::SampleRate::from(48_000.0),
            max_voices: 8,
            oscillator: OscillatorType::Saw,
            filter: FilterType::Svf {
                cutoff: Hz(2_000.0),
                q: Q(0.707),
                mode: tutti_polysynth::SvfMode::Lowpass,
            },
            ..Default::default()
        })
        .unwrap(),
    );

    // Trigger 4 sustained voices before entering the gate.
    synth.queue_midi(&[
        note_on(0, 60, 100),
        note_on(0, 64, 100),
        note_on(0, 67, 100),
        note_on(0, 72, 100),
    ]);

    for _ in 0..32 {
        synth.block(64);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            synth.block(64);
        }
    });
}

/// One-frame blocks: each is a
/// whole block, cut from the control grid at its edge.
#[test]
fn polysynth_one_frame_blocks_with_active_voices_are_allocation_free() {
    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            sample_rate: tutti_types::SampleRate::from(48_000.0),
            max_voices: 8,
            oscillator: OscillatorType::Triangle,
            ..Default::default()
        })
        .unwrap(),
    );

    synth.queue_midi(&[note_on(0, 60, 90), note_on(0, 64, 90), note_on(0, 67, 90)]);

    for _ in 0..256 {
        synth.tick();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            synth.tick();
        }
    });
}

#[test]
fn polysynth_process_with_midi_events_inside_block_is_allocation_free() {
    // Sub-buffer split path: events on the event input so the block walks
    // the event-driven boundaries.
    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            sample_rate: tutti_types::SampleRate::from(48_000.0),
            max_voices: 8,
            oscillator: OscillatorType::Saw,
            ..Default::default()
        })
        .unwrap(),
    );

    // Warm up the voice pool + finished-indices list at full size.
    synth.queue_midi(&[
        note_on(0, 48, 100),
        note_on(0, 60, 100),
        note_on(0, 72, 100),
    ]);
    for _ in 0..64 {
        synth.block(64);
    }
    synth.queue_midi(&[note_off(0, 48), note_off(0, 60), note_off(0, 72)]);
    for _ in 0..256 {
        synth.block(64);
    }

    // Steady note-on/note-off churn inside the gate. Each iteration
    // routes events through the same code path that handles real DAW
    // playback: poll inbox → sort by frame offset → drive voices.
    let on = note_on(0, 60, 100);
    let off = note_off(0, 60);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..200 {
            if i % 2 == 0 {
                synth.queue_midi(&[on]);
            } else {
                synth.queue_midi(&[off]);
            }
            synth.block(64);
        }
    });
}

/// Every arm of the decode, MIDI 1.0 and 2.0, a reserved attribute, a packet
/// `decode` refuses and a message the synth has no use for, inside the gate.
/// Mutation: an `Vec::new()` + `push` in the `Voice1::PolyPressure` arm
/// aborts inside the gate.
#[test]
fn polysynth_every_decoded_message_is_allocation_free() {
    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            sample_rate: tutti_types::SampleRate::from(48_000.0),
            max_voices: 8,
            mpe_enabled: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let address = Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::of(1),
    };
    let note = NoteNumber::of(60);
    let ev = |m: Message| Ump::from(m);
    let m1 = |v| ev(Message::Midi1(address, v));
    let m2 = |v| ev(Message::Midi2(address, v));
    let events = [
        m1(Voice1::NoteOn {
            note,
            velocity: U7::of(100),
        }),
        m1(Voice1::PolyPressure {
            note,
            pressure: U7::of(50),
        }),
        m1(Voice1::ControlChange {
            controller: CCNumber::of(1),
            value: U7::of(20),
        }),
        m1(Voice1::ControlChange {
            controller: CCNumber::of(64),
            value: U7::of(127),
        }),
        m1(Voice1::PitchBend(U14::of(9000))),
        m1(Voice1::ChannelPressure(U7::of(5))),
        m1(Voice1::ProgramChange(U7::of(5))),
        m1(Voice1::NoteOff {
            note,
            velocity: U7::of(0),
        }),
        m1(Voice1::NoteOn {
            note,
            velocity: U7::of(0),
        }),
        m2(Voice2::NoteOn {
            note,
            velocity: 0x8000,
            attribute: Attribute::Pitch(60 << 9),
        }),
        m2(Voice2::NoteOn {
            note,
            velocity: 0x8000,
            attribute: Attribute::Reserved { kind: 5, data: 1 },
        }),
        m2(Voice2::PerNotePitchBend {
            note,
            value: 0xC000_0000,
        }),
        m2(Voice2::PolyPressure {
            note,
            value: 0x4000_0000,
        }),
        m2(Voice2::RegisteredPerNote {
            note,
            controller: RegisteredPerNoteController::Pitch,
            value: 69 << 25,
        }),
        m2(Voice2::AssignablePerNote {
            note,
            controller: 74,
            value: 1 << 30,
        }),
        m2(Voice2::PerNoteManagement {
            note,
            detach: false,
            reset: true,
        }),
        m2(Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(7),
            value: 12 << 25,
        }),
        // Pitch Bend Sensitivity (RPN 0) on the master channel, then
        // Reset All Controllers and All Notes Off.
        m2(Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: 7 << 25,
        }),
        m1(Voice1::ControlChange {
            controller: CCNumber::of(121),
            value: U7::of(0),
        }),
        m1(Voice1::ControlChange {
            controller: CCNumber::of(123),
            value: U7::of(0),
        }),
        m2(Voice2::Assignable {
            bank: U7::of(1),
            index: U7::of(2),
            value: 3,
        }),
        m2(Voice2::ControlChange {
            controller: CCNumber::of(7),
            value: 1 << 31,
        }),
        m2(Voice2::PitchBend(0x9000_0000)),
        m2(Voice2::ChannelPressure(1 << 30)),
        m2(Voice2::NoteOff {
            note,
            velocity: 0,
            attribute: Attribute::Reserved { kind: 9, data: 2 },
        }),
        ev(Message::System(
            MidiGroup::FIRST,
            tutti_ump::System::TimingClock,
        )),
        // A reserved message type: `decode` refuses it.
        Ump([0x6000_0000, 0, 0, 0]),
    ];
    for _ in 0..8 {
        synth.queue_midi(&events);
        synth.block(64);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            synth.queue_midi(&events);
            synth.block(64);
        }
    });
}

/// The case the tests above never reached: **every** voice finishing in the
/// same block, at a `max_voices` well past the old 16-voice cap.
///
/// `finished_indices` collects one entry per voice that finished, so this
/// fills it exactly to its capacity: every voice releasing in the same block,
/// on a heap buffer, inside the no-alloc gate. That pins the "sized once,
/// `clear()`ed thereafter" property at a voice count well past any small
/// inline buffer.
///
/// Constructing at all is half the assertion: the `.unwrap()` below shows
/// `max_voices` has no upper bound.
///
/// *Mutation:* `Vec::with_capacity(config.max_voices)` -> `Vec::new()` in
/// `PolySynth::new` aborts inside the gate on the first block that finishes
/// a voice.
#[test]
fn polysynth_all_voices_finishing_together_is_allocation_free() {
    // Large enough that the buffer under test is certainly on the heap.
    const MAX_VOICES: usize = 64;

    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            sample_rate: tutti_types::SampleRate::from(48_000.0),
            max_voices: MAX_VOICES,
            oscillator: OscillatorType::Saw,
            ..Default::default()
        })
        .unwrap(),
    );

    // Distinct pitches so the allocator assigns a separate voice to each
    // rather than retriggering one slot.
    let all_on: Vec<Ump> = (0..MAX_VOICES)
        .map(|i| note_on(0, 36 + i as u8, 100))
        .collect();
    let all_off: Vec<Ump> = (0..MAX_VOICES).map(|i| note_off(0, 36 + i as u8)).collect();

    // Warm up: run the full on/off cycle once outside the gate so every
    // lazily-sized buffer reaches its steady-state capacity first.
    for _ in 0..4 {
        synth.queue_midi(&all_on);
        for _ in 0..32 {
            synth.block(64);
        }
        synth.queue_midi(&all_off);
        for _ in 0..256 {
            synth.block(64);
        }
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..32 {
            synth.queue_midi(&all_on);
            for _ in 0..32 {
                synth.block(64);
            }
            // All releases land together, so a single block collects the full
            // `MAX_VOICES` finished indices — the worst case the capacity is
            // sized for.
            synth.queue_midi(&all_off);
            for _ in 0..256 {
                synth.block(64);
            }
        }
    });
}

/// **A freshly constructed synth allocates nothing, on a thread that is
/// already warm.**
///
/// This is the test that pins `Vec::with_capacity(max_voices)` in
/// `PolySynth::new`. The neighbouring steady-state test cannot: it warms the
/// *instance* before opening the gate, so a bare `Vec::new()` simply grows to
/// capacity during the warm-up and the mutation passes. Verified, not
/// assumed.
///
/// Warming the **thread** and gating a **fresh instance** separates the two
/// costs. It matters beyond the mutation: a never-processed `PolySynth` (a
/// fresh insert, a fork) goes straight into a callback that is already
/// running, which is exactly this shape.
///
/// *Mutations, both run:* `Vec::with_capacity(..)` -> `Vec::new()` in
/// `PolySynth::new` aborts on the `new` half; the same substitution in the
/// `Clone` impl aborts on the `clone` half. They are separate construction
/// sites and an earlier draft of this test covered only the first — the
/// `Clone` mutation passed against it.
#[test]
fn polysynth_allocates_nothing_on_a_fresh_instance() {
    const MAX_VOICES: usize = 64;

    let build = || {
        PolySynth::new(SynthConfig {
            sample_rate: tutti_types::SampleRate::from(48_000.0),
            max_voices: MAX_VOICES,
            oscillator: OscillatorType::Saw,
            ..Default::default()
        })
        .unwrap()
    };

    let notes: Vec<Ump> = (0..MAX_VOICES)
        .map(|i| note_on(0, 36 + i as u8, 100))
        .collect();
    let offs: Vec<Ump> = (0..MAX_VOICES).map(|i| note_off(0, 36 + i as u8)).collect();

    // Warm the THREAD only, so this test measures the instance alone: a cold
    // thread is `a_first_block_on_a_cold_thread_is_allocation_free`'s subject.
    {
        let mut throwaway = Hand::new(build());
        throwaway.block(64);
    }

    // Two never-processed synths: one straight from `new`, one from `Clone`.
    // The clone is the case that reaches a running graph as a fork (a fork
    // is a clone of the template `param_parts` keeps) — and it has its own
    // construction site for `finished_indices`, which the `new` half does
    // not cover. Wrapped (and prepared) outside the gate, as a graph
    // prepares a node on the control thread.
    let pristine = build();
    let cloned = pristine.clone();

    for (which, synth) in [("new", pristine), ("clone", cloned)] {
        let mut synth = Hand::new(synth);
        // Queued outside the gate: `queue` is a control-thread call.
        synth.queue_midi(&notes);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..32 {
                synth.block(64);
            }
        });

        synth.queue_midi(&offs);
        assert_no_alloc::assert_no_alloc(|| {
            // Long enough for every release to land, so the block that
            // collects all 64 finished indices is inside the gate.
            for _ in 0..256 {
                synth.block(64);
            }
        });
        // Named so a failure says which construction site leaked.
        let _ = which;
    }
}

/// **The first block on a *cold thread* is allocation-free.**
///
/// It used not to be, and was `#[ignore]`d as a recorded defect: the first
/// `process` on a thread went through `MidiInPort::poll`, whose
/// `ArcSwapOption::load` initialised arc-swap's per-thread slots lazily (128
/// bytes), so the cost landed on the first callback of every new audio thread
/// (`CpalDriver::restart` makes one on each device switch). The port is gone
/// (MIDI arrives on the event input), and with it the allocation.
///
/// The rest of the suite is blind to this: every other test warms the
/// instance, and so the thread, before opening the gate.
///
/// Mutation (run): a lazily initialised `thread_local!` `Vec` touched in the
/// synth's `render_events` → the gate aborts on the cold thread.
#[test]
fn a_first_block_on_a_cold_thread_is_allocation_free() {
    let mut synth = Hand::new(
        PolySynth::new(SynthConfig {
            sample_rate: tutti_types::SampleRate::from(48_000.0),
            max_voices: 8,
            oscillator: OscillatorType::Saw,
            ..Default::default()
        })
        .unwrap(),
    );

    assert_no_alloc::assert_no_alloc(|| {
        synth.block(64);
    });
}

/// **The graph path is allocation-free too:** a clip node playing a dense
/// clip into a synth, through the executor, rolling, then across a
/// stop (the clip ends its held notes) and a restart. Nothing is published
/// inside the gate: publishing is the control thread's.
///
/// Mutation: a `Vec` built in `MidiClipNode::process` → the gate aborts.
/// Mutation (run): the synth's `render_events` collecting the block's chunks
/// into a `Vec` → the gate aborts.
#[test]
fn a_clip_into_a_synth_is_allocation_free() {
    use tutti_graph::{Editor, EventEdge, EventIn, EventOut, Prepare, Transport};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_types::{Beat, Bpm, NodeKey, Samples};

    const BLOCK: usize = 256;
    // A note on or off every 37 frames over the first 8 192.
    let clip = MidiClipNode::new((0..220u64).map(|i| {
        let event = if i % 2 == 0 {
            note_on(0, 48 + (i % 24) as u8, 100)
        } else {
            note_off(0, 48 + ((i - 1) % 24) as u8)
        };
        TimedMidiEvent::new(Beat((i * 37) as f64 / 24_000.0), event)
    }));
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(BLOCK)));
    ed.insert(NodeKey(1), "clip", clip);
    ed.insert(
        NodeKey(2),
        "synth",
        PolySynth::new(SynthConfig {
            sample_rate: SampleRate(48_000.0),
            max_voices: 8,
            oscillator: OscillatorType::Saw,
            ..Default::default()
        })
        .unwrap(),
    );
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(2),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 0,
        }),
    );
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| {
            tutti_types::graph::Source::Node(tutti_types::graph::OutPort {
                node: NodeKey(2),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let at = |frame: usize, playing: bool| {
        Transport::new(playing, Bpm::of(120.0), Beat(frame as f64 / 24_000.0), None)
    };
    // Installs the plan (and its first-block setup) outside the gate.
    let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    exec.process(BLOCK, &at(0, true), &[], &mut [&mut l[..], &mut r[..]]);
    assert_no_alloc::assert_no_alloc(|| {
        for b in 1..40 {
            exec.process(
                BLOCK,
                &at(b * BLOCK, true),
                &[],
                &mut [&mut l[..], &mut r[..]],
            );
        }
        exec.process(
            BLOCK,
            &at(40 * BLOCK, false),
            &[],
            &mut [&mut l[..], &mut r[..]],
        );
        for b in 0..8 {
            exec.process(
                BLOCK,
                &at(b * BLOCK, true),
                &[],
                &mut [&mut l[..], &mut r[..]],
            );
        }
    });
}

/// The note logic's other paths run inside the gate too: a mono and a legato
/// synth taking a note over, and returning to a held key from a note-off (which
/// starts a voice, `Gate::Start`, from the event path), with the pedals.
#[test]
fn mono_and_legato_event_paths_are_allocation_free() {
    for mode in [
        tutti_polysynth::VoiceMode::Mono,
        tutti_polysynth::VoiceMode::Legato,
    ] {
        let mut synth = Hand::new(
            PolySynth::new(SynthConfig {
                sample_rate: tutti_types::SampleRate::from(48_000.0),
                max_voices: 4,
                voice_mode: mode,
                oscillator: OscillatorType::Saw,
                ..Default::default()
            })
            .unwrap(),
        );
        for _ in 0..8 {
            synth.block(64);
        }
        assert_no_alloc::assert_no_alloc(|| {
            for round in 0..200u8 {
                let cc = |n: u8, v: u32| {
                    midi::cc(
                        MidiGroup::FIRST,
                        MidiChannel::of(0),
                        CCNumber::of(n),
                        v << 25,
                    )
                };
                // 60 held, 64 over it, 64 let go: the return to 60.
                synth.queue_midi(&[
                    note_on(0, 60, 100),
                    note_on(0, 64, 90),
                    note_on(1, 67, 80),
                    note_off(0, 64),
                    cc(64, u32::from(round % 2) * 127),
                    cc(66, u32::from(round % 3 == 0) * 127),
                ]);
                synth.block(64);
                synth.queue_midi(&[note_off(0, 60), note_off(1, 67), cc(123, 0), cc(124, 0)]);
                synth.block(64);
            }
        });
    }
}
