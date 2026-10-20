//! RT-safety regression: a bound out-of-process plugin, run by the
//! graph's executor, does not allocate on the audio thread in steady state —
//! the FIFO and ring, the transport snapshot from `Env` (with a meter
//! installed), the per-chunk payload (MIDI from the live inbox, and curve
//! segments from two foreign control lanes on the plugin's event input, an
//! LFO and an automation envelope, followed along by the node's
//! `SegmentPoints`), the IPC submit and collect, and the executor around
//! them, the lanes included.
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
use tutti_core::{ArcLend, BeatDuration, Depth, PhaseIncrement};
use tutti_graph::{Offset, Transport, TransportChanges};
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_mod::{Amount, Clock, ControlProgram, Layer, LayerId, LfoShape, Polarity, Wave};
use tutti_nodes::automation::{AutomationEnvelope, AutomationPoint, ControlLaneNode, CurveType};
use tutti_types::{Beat, Bpm, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
/// The graph's `MaxBlock`: blocks of 64 (as the engine rendered in passes
/// while a `Legacy` node was in the graph), so a chunk collects eight
/// blocks of automation points.
const BLOCK: usize = 64;
/// The device callback, and so the plugin's chunk: eight passes of 64.
const QUANTUM: usize = 512;
/// Passes per callback.
const PASSES: usize = QUANTUM / BLOCK;

/// Steady state: a rolling transport with a tempo change inside every block,
/// a meter installed, MIDI notes queued on a keyboard's queue node and an LFO
/// automating a parameter (so every payload carries MIDI and parameter
/// points), the plugin echoing the transport it is sent, and real time
/// between callbacks so chunks are collected as well as submitted. Shaped as
/// the engine renders a 512-frame device callback: eight 64-frame passes,
/// the FIFO carrying the chunk across all eight, the lanes' points on the
/// play clock's 32-frame grid across it.
///
/// Mutation: allocate in the node's chunk walk (a `Vec` of the chunk's
/// input slices in place of the stack array in `graph_node.rs`) → the
/// guarded blocks allocate → the test aborts. Mutation: build a fresh
/// `MeterMap::default()` per block in place of the prebuilt
/// `Bound::default_meter` → it allocates → aborts. Mutation: collect each
/// payload's MIDI into a `Vec` before cloning it into the payload → aborts.
/// Mutation: let a chunk's parameter queue grow past its point room (no
/// `has_room` check in `ParamDelivery::place`) → eight blocks of points
/// outgrow the queue's reserved storage → aborts.
/// Mutation (run, doc 014 PR 5, on the lanes' segments): allocate where
/// the node starts following a segment (a `Vec` per
/// `SegmentPoints::start`) → aborts. (Growing the slots past what
/// `prepare` reserved is not seen here: each lane has one segment in
/// flight on its own parameter, so two slots are ever live; the overflow
/// path's fallback is pinned by `segment_points`' unit tests.)
///
/// Ported in doc 014 PR 5: the LFO was an `LfoCurve` sampled by the deleted
/// `PluginAutomation` node; it is a lane's LFO layer around its knob now.
#[test]
fn a_bound_plugin_does_not_allocate_on_the_audio_thread() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let probe = load_probe(SAMPLE_RATE);
    // A sine LFO a quarter beat long swinging parameter 77 across its
    // whole range: a layer of depth 0.5 around a knob at 0.5.
    let lfo = Layer {
        id: LayerId(1),
        clock: Clock::Beats {
            per_cycle: BeatDuration(0.25),
            phase: PhaseIncrement(0.0),
        },
        wave: Wave::Shape {
            shape: LfoShape::Sine,
            seed: 0,
        },
        amount: Amount {
            depth: Depth(0.5),
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
        },
        depth_by: None,
    };
    let program = ControlProgram::new(None, vec![lfo], vec![]).expect("a valid program");
    let lfo = ControlLaneNode::for_foreign(77, program, Some(0.5), (0.0, 1.0));
    let prepare = tutti_graph::Prepare::new(tutti_types::SampleRate(SAMPLE_RATE), Samples(BLOCK))
        .with_quantum(Samples(QUANTUM));
    // Curve segments on another parameter: an exponential envelope, so
    // each segment runs across chunks and the node follows it.
    let mut env = AutomationEnvelope::new(());
    for k in 0..200 {
        let v = if k % 2 == 0 { 0.2 } else { 0.9 };
        env.add_point(AutomationPoint::with_curve(
            f64::from(k) * 2.0,
            v,
            CurveType::Exponential,
        ));
    }
    let lane = ControlLaneNode::for_foreign(
        78,
        ControlProgram::from_automation(Arc::new(env)),
        None,
        (0.0, 1.0),
    );
    let (mut rig, sender) = Rig::prepared_with_keys(probe.client.bind(), prepare, vec![lfo, lane]);
    let meter = Arc::new(ArcLend::new(MeterMap::new([MeterChange::new(
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
