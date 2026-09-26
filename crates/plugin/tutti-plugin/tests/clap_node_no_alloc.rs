//! RT-safety regression: a bound out-of-process plugin, run by the native
//! graph's executor, does not allocate on the audio thread in steady state —
//! the chunk walk, the transport snapshot from `Env` (with a meter
//! installed), the IPC submit and collect, and the executor around them.
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
use tutti_core::RtPublish;
use tutti_graph::{Offset, Transport, TransportChanges};
use tutti_types::{Beat, Bpm, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
/// Two chunks a block, so the walk inside `process` runs more than once.
const BLOCK: usize = 128;

/// Steady state: a rolling transport with a tempo change inside every block,
/// a meter installed, the plugin echoing the transport it is sent, and real
/// time between blocks so chunks are collected as well as submitted.
///
/// Mutation: allocate in the node's chunk walk (a `Vec` of the chunk's
/// input slices in place of the stack array in `graph_node.rs`) → the
/// guarded blocks allocate → the test aborts. Mutation: build a fresh
/// `MeterMap::default()` per block in place of the prebuilt
/// `Bound::default_meter` → it allocates → aborts.
#[test]
fn a_bound_plugin_does_not_allocate_on_the_audio_thread() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::TRANSPORT);
    let probe = load_probe(SAMPLE_RATE);
    let mut rig = Rig::new(probe.client.bind(), SAMPLE_RATE, BLOCK);
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
            Offset::new(40, Samples(BLOCK)).expect("inside"),
            Transport::new(true, Bpm(90.0), Beat(9.0), None),
        )
        .expect("a change inside the block");
    let pace = Duration::from_millis(2);
    let mut block = |b: usize, outs: &mut [&mut [f32]]| {
        let t = Transport::new(true, Bpm(120.0), Beat(b as f64), None).with_recording(true);
        rig.renderer()
            .executor_mut()
            .process_with_changes(BLOCK, &t, &changes, &inputs, outs);
        std::thread::sleep(pace);
    };

    // Warm up: the first blocks fault in the slab and fill the pipeline.
    for b in 0..32 {
        block(b, &mut outs);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for b in 32..160 {
            block(b, &mut outs);
        }
    });
    drop(outs);
    // The plugin rendered through it all (its transport echo is in the last
    // block), so the collect path ran inside the guard, not only the submit.
    assert!(
        out[0].iter().any(|&s| s != 0.0),
        "the steady state rendered nothing; the guarded blocks never collected"
    );
    drop(probe.handle);
}
