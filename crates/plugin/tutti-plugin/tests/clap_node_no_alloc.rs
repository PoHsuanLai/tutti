//! RT-safety regression: a bound out-of-process plugin, run by the native
//! graph's executor, does not allocate on the audio thread in steady state —
//! the FIFO and ring, the transport snapshot from `Env` (with a meter
//! installed), the per-chunk payload (MIDI from the live inbox, parameter
//! automation from an LFO), the IPC submit and collect, and the executor
//! around them.
//!
//! What this can cover is the calling thread's code path; the audio thread
//! is this test's thread. The bridge thread and the `plugin-server` process
//! allocate as they like, and are not the audio thread.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv, Rig};

use std::sync::Arc;
use std::time::Duration;

use tutti_core::meter::{BeatsPerBar, MeterChange, MeterMap, NoteValue, TimeSignature};
use tutti_core::{BeatDuration, Depth, PhaseIncrement, RtPublish};
use tutti_graph::{Offset, Transport, TransportChanges};
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_plugin::handles::{LfoCurve, LfoShape, ParamAddress, ParamId, TimedParam};
use tutti_types::{Beat, Bpm, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
/// The graph's `MaxBlock`: the engine's pass while a `Legacy`-flagged node
/// (the plugin, still) is in the graph.
const BLOCK: usize = 64;
/// The device callback, and so the plugin's chunk: eight passes of 64.
const QUANTUM: usize = 512;
/// Passes per callback.
const PASSES: usize = QUANTUM / BLOCK;

/// Steady state: a rolling transport with a tempo change inside every block,
/// a meter installed, MIDI notes queued on the live inbox and an LFO
/// automating a parameter (so every payload carries MIDI and parameter
/// points), the plugin echoing the transport it is sent, and real time
/// between callbacks so chunks are collected as well as submitted. Shaped as
/// the engine renders a 512-frame device callback: eight 64-frame passes,
/// the FIFO carrying the chunk across all eight, the automation's points
/// spaced for a 512-frame block.
///
/// Mutation: allocate in the node's chunk walk (a `Vec` of the chunk's
/// input slices in place of the stack array in `graph_node.rs`) → the
/// guarded blocks allocate → the test aborts. Mutation: build a fresh
/// `MeterMap::default()` per block in place of the prebuilt
/// `Bound::default_meter` → it allocates → aborts. Mutation: collect each
/// payload's MIDI into a `Vec` before cloning it into the payload → aborts.
/// Mutation: space the automation's points at a fixed 8 samples
/// (`stride_for` returning `SAMPLE_STRIDE`) → 65 points a chunk spill the
/// queue to the heap → aborts.
#[test]
fn a_bound_plugin_does_not_allocate_on_the_audio_thread() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let mut probe = load_probe(SAMPLE_RATE);
    let transport = tutti_core::transport::Transport::new(SAMPLE_RATE);
    transport.settings.set_tempo(120.0);
    let _ = transport.motion.try_send(tutti_core::MotionEvent::Play);
    transport.motion.drain();
    probe.client.set_param_automation_source(
        [TimedParam {
            param_id: ParamAddress::Opaque(ParamId::new(77)),
            curve: Arc::new(LfoCurve::new(
                LfoShape::Sine,
                BeatDuration(0.25),
                Depth(1.0),
                PhaseIncrement(0.0),
                0.5,
                0.0,
                1.0,
            )),
        }],
        transport.clone(),
    );
    let sender = probe.client.midi_sender();
    let prepare = tutti_graph::Prepare::new(tutti_types::SampleRate(SAMPLE_RATE), Samples(BLOCK))
        .with_quantum(Samples(QUANTUM));
    let mut rig = Rig::prepared(probe.client.bind(), prepare);
    let meter = Arc::new(RtPublish::new(MeterMap::new([MeterChange::new(
        Beat(0.0),
        TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
    )])));
    rig.controls.set_meter(Arc::clone(&meter));

    let silence = vec![0.0f32; BLOCK];
    let inputs: Vec<&[f32]> = (0..rig.inputs()).map(|_| &silence[..]).collect();
    let mut out = vec![vec![0.0f32; BLOCK]; rig.outputs()];
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
    let mut changes = TransportChanges::NONE;
    changes
        .push(
            Offset::new(20, Samples(BLOCK)).expect("inside"),
            Transport::new(true, Bpm(90.0), Beat(9.0), None),
        )
        .expect("a change inside the block");
    let pace = Duration::from_millis(15);
    let mut block = |b: usize, outs: &mut [&mut [f32]]| {
        if b.is_multiple_of(4) {
            let note = 60 + (b % 12) as u8;
            let on = MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                note,
                midi1_velocity_to_midi2(100),
            );
            let off = MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(1), note, 0);
            sender.queue(&[on, off]);
        }
        let n = BLOCK;
        let t = Transport::new(true, Bpm(120.0), Beat(b as f64), None).with_recording(true);
        let ins: [&[f32]; 2] = [&inputs[0][..n], &inputs[1 % inputs.len()][..n]];
        let (a, rest) = outs.split_at_mut(1);
        let mut o: [&mut [f32]; 2] = [&mut a[0][..n], &mut rest[0][..n]];
        rig.renderer().executor_mut().process_with_changes(
            n,
            &t,
            &changes,
            &ins[..inputs.len()],
            &mut o[..],
        );
        if (b + 1).is_multiple_of(PASSES) {
            std::thread::sleep(pace);
        }
    };

    // Warm up: the first blocks fault in the slab and fill the pipeline.
    for b in 0..4 * PASSES {
        block(b, &mut outs);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for b in 4 * PASSES..=20 * PASSES {
            block(b, &mut outs);
        }
    });
    drop(outs);
    // The plugin rendered through it all (its transport echo opens every
    // chunk, and the last pass opened one), so the collect path ran inside
    // the guard, not only the submit.
    assert!(
        out[0].iter().any(|&s| s != 0.0),
        "the steady state rendered nothing; the guarded blocks never collected"
    );
    drop(probe.handle);
}
