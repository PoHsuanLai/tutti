//! Regression gate: `PolySynth::process` must not allocate.
//!
//! PolySynth is the umbrella audio-callback unit for the in-tree
//! polyphonic synth. It owns:
//! - a `Vec<SynthVoice>` (allocated once on build),
//! - a `SmallVec<[usize; 16]>` of finished-voice indices (inline),
//! - a per-process `midi_buffer` for sorted events,
//! - and a `mix_buffer` scalar pair.
//!
//! `tick` and `process` both pull MIDI off the inbox, iterate active
//! voices, and mix into the output. A regression that grows `midi_buffer`
//! at runtime, or that frees the finished-indices buffer on the audio
//! thread, would land here.
//!
//! Note that most tests below run at `max_voices: 8` — half the inline
//! capacity — so they exercise the steady state but *not* the collection's
//! worst case. `polysynth_all_voices_finishing_together_is_allocation_free`
//! is the one that fills it; `max_voices` past the inline capacity is
//! refused by the constructor and covered separately.

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec, Hz, SampleRate, Q};
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_polysynth::{FilterType, OscillatorType, PolySynth, SynthConfig};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
    MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::new(channel),
        note,
        midi1_velocity_to_midi2(vel),
    )
}

fn note_off(channel: u8, note: u8) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(channel), note, 0)
}

#[test]
fn polysynth_process_idle_is_allocation_free() {
    // No active voices — process should still tick through allocator
    // bookkeeping and the MIDI inbox poll.
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: 8,
        oscillator: OscillatorType::Saw,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
    });
}

#[test]
fn polysynth_process_with_active_voices_is_allocation_free() {
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: 8,
        oscillator: OscillatorType::Saw,
        filter: FilterType::Svf {
            cutoff: Hz(2_000.0),
            q: Q(0.707),
            mode: tutti_polysynth::SvfMode::Lowpass,
        },
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));

    // Trigger 4 sustained voices before entering the gate.
    let sender = synth.midi_sender();
    sender.queue(&[
        note_on(0, 60, 100),
        note_on(0, 64, 100),
        note_on(0, 67, 100),
        note_on(0, 72, 100),
    ]);

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..32 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
    });
}

#[test]
fn polysynth_tick_with_active_voices_is_allocation_free() {
    // `tick` is the per-sample path — drives `Voice::tick_stereo`
    // directly without MIDI sub-buffer splitting.
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: 8,
        oscillator: OscillatorType::Triangle,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));

    synth
        .midi_sender()
        .queue(&[note_on(0, 60, 90), note_on(0, 64, 90), note_on(0, 67, 90)]);

    let mut output = [0.0f32; 2];
    for _ in 0..256 {
        synth.tick(&[], &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            synth.tick(&[], &mut output);
        }
    });
}

#[test]
fn polysynth_process_with_midi_events_inside_block_is_allocation_free() {
    // Sub-buffer split path: queue events with non-zero frame offsets so
    // `process` walks the event-driven block boundaries.
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: 8,
        oscillator: OscillatorType::Saw,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));
    let sender = synth.midi_sender();

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Warm up the voice pool + finished-indices SmallVec at full size.
    sender.queue(&[
        note_on(0, 48, 100),
        note_on(0, 60, 100),
        note_on(0, 72, 100),
    ]);
    for _ in 0..64 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    }
    sender.queue(&[note_off(0, 48), note_off(0, 60), note_off(0, 72)]);
    for _ in 0..256 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    }

    // Steady note-on/note-off churn inside the gate. Each iteration
    // routes events through the same code path that handles real DAW
    // playback: poll inbox → sort by frame offset → drive voices.
    let on = note_on(0, 60, 100);
    let off = note_off(0, 60);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..200 {
            if i % 2 == 0 {
                sender.queue(&[on]);
            } else {
                sender.queue(&[off]);
            }
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
    });
}

/// The case the tests above never reached: **every** voice finishing in the
/// same block, at the maximum `max_voices` the constructor accepts.
///
/// `finished_indices` collects one entry per voice that finished, so this
/// fills it exactly to its inline capacity. The other tests in this file all
/// run at `max_voices: 8`, half of it.
///
/// **Run at 64 voices, which is the point.** This test used to run at 16,
/// because 16 was the ceiling `PolySynth::new` enforced: `finished_indices`
/// was a `SmallVec<[usize; 16]>` and a larger `max_voices` could have spilled
/// it onto the heap inside the callback. At 16-of-16 the collection was
/// always inline, so the test passed whether the drain indexed or used
/// `mem::take`, and it proved nothing about the heap.
///
/// `finished_indices` is now a `Vec` sized to `max_voices` at construction,
/// so there is no ceiling and this runs well past the old one — every voice
/// releasing in the same block, on a heap buffer, inside the no-alloc gate.
/// That is a strictly stronger statement than the version with the cap: it
/// pins the "sized once, `clear()`ed thereafter" property that replaced the
/// ceiling, rather than a bound that made the property untestable.
///
/// Constructing at all is half the assertion — the `.unwrap()` below is what
/// used to be `polysynth_rejects_max_voices_past_inline_capacity`.
///
/// *Mutation:* `Vec::with_capacity(config.max_voices)` -> `Vec::new()` in
/// `PolySynth::new` aborts inside the gate on the first block that finishes
/// a voice.
#[test]
fn polysynth_all_voices_finishing_together_is_allocation_free() {
    // Four times the old inline ceiling, so the buffer under test is on the heap.
    const MAX_VOICES: usize = 64;

    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: MAX_VOICES,
        oscillator: OscillatorType::Saw,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));
    let sender = synth.midi_sender();

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Distinct pitches so the allocator assigns a separate voice to each
    // rather than retriggering one slot.
    let all_on: Vec<MidiEvent> = (0..MAX_VOICES)
        .map(|i| note_on(0, 36 + i as u8, 100))
        .collect();
    let all_off: Vec<MidiEvent> = (0..MAX_VOICES).map(|i| note_off(0, 36 + i as u8)).collect();

    // Warm up: run the full on/off cycle once outside the gate so every
    // lazily-sized buffer reaches its steady-state capacity first.
    for _ in 0..4 {
        sender.queue(&all_on);
        for _ in 0..32 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
        sender.queue(&all_off);
        for _ in 0..256 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..32 {
            sender.queue(&all_on);
            for _ in 0..32 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                synth.process(64, &input, &mut output);
            }
            // All releases land together, so a single block collects the full
            // `MAX_VOICES` finished indices — the worst case the capacity is
            // sized for.
            sender.queue(&all_off);
            for _ in 0..256 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                synth.process(64, &input, &mut output);
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
/// costs. It matters beyond the mutation: `Net::commit` clones graph nodes,
/// so a never-processed `PolySynth` goes straight into a callback that is
/// already running, which is exactly this shape.
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
        let mut synth = PolySynth::new(SynthConfig {
            sample_rate: tutti_core::SampleRate::from(48_000.0),
            max_voices: MAX_VOICES,
            oscillator: OscillatorType::Saw,
            ..Default::default()
        })
        .unwrap();
        synth.set_sample_rate(SampleRate(48_000.0));
        synth
    };

    let notes: Vec<MidiEvent> = (0..MAX_VOICES)
        .map(|i| note_on(0, 36 + i as u8, 100))
        .collect();
    let offs: Vec<MidiEvent> = (0..MAX_VOICES).map(|i| note_off(0, 36 + i as u8)).collect();

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Warm the THREAD only — see `a_first_block_on_a_cold_thread_allocates`
    // for what this is paying for and why it is not this test's subject.
    {
        let mut throwaway = build();
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        throwaway.process(64, &input, &mut output);
    }

    // Two never-processed synths: one straight from `new`, one from `Clone`.
    // The clone is the case that actually reaches a running callback, since
    // `Net::commit` clones nodes — and it has its own construction site for
    // `finished_indices`, which the `new` half does not cover.
    let pristine = build();
    let cloned = pristine.clone();

    for (which, mut synth) in [("new", pristine), ("clone", cloned)] {
        let sender = synth.midi_sender();
        // Queued outside the gate: `queue` is a control-thread call.
        sender.queue(&notes);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..32 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                synth.process(64, &input, &mut output);
            }
        });

        sender.queue(&offs);
        assert_no_alloc::assert_no_alloc(|| {
            // Long enough for every release to land, so the block that
            // collects all 64 finished indices is inside the gate.
            for _ in 0..256 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                synth.process(64, &input, &mut output);
            }
        });
        // Named so a failure says which construction site leaked.
        let _ = which;
    }
}

/// **A pre-existing defect, recorded rather than fixed here: the first block
/// on a *cold thread* allocates.**
///
/// `#[ignore]`d because it fails, and it fails on code this change did not
/// touch — it reproduces identically at the default 8 voices on the commit
/// before `finished_indices` became a `Vec`. It is filed here because this is
/// where it was found and this file is where someone will look.
///
/// What was established, by bisection:
///
/// - A synth that has never been processed, given no MIDI and holding no
///   active voices, allocates 128 bytes on its first `process`.
/// - It is **per thread, not per instance**: a brand-new synth on a thread
///   that has already processed one allocates nothing (that is the
///   neighbouring test, which passes).
/// - The first thing `process` calls is `poll_midi_events_sorted` ->
///   `MidiInPort::poll`, whose first statement after the mailbox drain is
///   `self.source.load()` on an `ArcSwapOption`. `arc-swap` initialises its
///   per-thread fast slots lazily, on first use from each thread.
///
/// Why it matters rather than being a curiosity: the cost lands on the
/// **first callback of any new audio thread**, and `CpalDriver::restart`
/// makes a new one on every device switch. The whole `rt_no_alloc` suite is
/// blind to it because every other test warms the instance — and therefore
/// the thread — before opening the gate.
///
/// Fixing it belongs in `MidiInPort`, not here: something has to touch the
/// `ArcSwap` once from the audio thread before the first real block, or the
/// port has to stop using one on this path.
#[test]
#[ignore = "pre-existing: arc-swap's per-thread slots allocate on first load; see the doc"]
fn a_first_block_on_a_cold_thread_allocates() {
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: 8,
        oscillator: OscillatorType::Saw,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));
    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    assert_no_alloc::assert_no_alloc(|| {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    });
}

/// **The native path is allocation-free too:** a clip node playing a dense
/// clip into a native synth, through the executor, rolling, then across a
/// stop (the clip ends its held notes) and a restart. Nothing is published
/// inside the gate: publishing is the control thread's.
///
/// Mutation: a `Vec` built in `MidiClipNode::process` → the gate aborts.
/// Mutation: the synth's `gather_events` collecting the event input into a
/// `Vec` before merging → the gate aborts.
#[test]
fn a_clip_into_a_native_synth_is_allocation_free() {
    use tutti_core::{Beat, Bpm, NodeKey, Samples};
    use tutti_graph::{Editor, EventEdge, EventIn, EventOut, Prepare, Transport};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};

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
            tutti_core::graph::Source::Node(tutti_core::graph::OutPort {
                node: NodeKey(2),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let at = |frame: usize, playing: bool| {
        Transport::new(playing, Bpm(120.0), Beat(frame as f64 / 24_000.0), None)
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
