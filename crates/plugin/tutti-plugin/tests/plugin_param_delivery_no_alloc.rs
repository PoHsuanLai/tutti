//! RT-safety regression for parameter delivery at width: a bound
//! out-of-process plugin with two hundred parameters, every one of them
//! moving in every block, does not allocate on the audio thread in steady
//! state. The node's slots, its chunk's queues and their point room, and the
//! bridge's parameter buffers are all reserved at bind and prepare.
//!
//! What this can cover is the calling thread's code path; the audio thread is
//! this test's thread. The bridge thread and the `plugin-server` process
//! allocate as they like, and are not the audio thread.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[path = "support/clap_probe.rs"]
mod clap_probe;
#[path = "support/param_probe.rs"]
mod param_probe;
#[path = "support/probe_path.rs"]
mod probe_path;

use std::time::Duration;

use clap_probe::{exclusive, Rig};
use param_probe::{knobs, load_vst2_probe};
use tutti_graph::{Prepare, Transport, TransportChanges};
use tutti_types::{Beat, Bpm, SampleRate, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
/// The graph's `MaxBlock`.
const BLOCK: usize = 64;
/// The device callback, and so the plugin's chunk: sixteen passes of 64.
const QUANTUM: usize = 1024;
const PASSES: usize = QUANTUM / BLOCK;
const PARAMS: usize = 200;

/// Two hundred parameters, each a foreign control lane whose knob is set to
/// a new value before every block (the lanes run inside the gate too), so
/// every chunk carries a queue for each of them with a point from each of
/// its sixteen blocks (more than a queue's ten inline points, fewer than its
/// reserved room of 1024 / 32 + 4). Rolling transport, real time between
/// callbacks, so chunks are collected and the parameter buffers come back
/// from the bridge thread as well as go out. Nothing allocates or frees on
/// the audio thread, and no point is refused or merged for want of room.
///
/// Mutations (run), each fails: the node's staged list rebuilt per chunk
/// (`self.staged = Vec::new()` for `clear()` in `ParamDelivery::settle`) →
/// the next chunk's first point allocates → aborts; `open_queue` ignoring
/// the reserved spares (always `ParameterQueue::new`) → each queue holds
/// its ten inline points and merges the rest → `points_merged` is not 0;
/// the bridge's buffers reserved without point room
/// (`ParameterChanges::with_capacity(queues)` in `reserve_params`) → once
/// the node is handed one of them, the same.
#[test]
fn two_hundred_moving_parameters_do_not_allocate_on_the_audio_thread() {
    let _lock = exclusive();
    let (client, handle) = load_vst2_probe(PARAMS, SAMPLE_RATE);
    let (knobs, lanes) = knobs(PARAMS, |_| 0.5);
    let prepare =
        Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)).with_quantum(Samples(QUANTUM));
    let mut rig = Rig::prepared_with(client.bind(), prepare, lanes);

    let silence = vec![0.0f32; BLOCK];
    let inputs: Vec<&[f32]> = (0..rig.inputs()).map(|_| &silence[..]).collect();
    let mut out = vec![vec![0.0f32; BLOCK]; rig.outputs()];
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
    let pace = Duration::from_millis(15);
    let block = |b: usize, rig: &mut Rig, outs: &mut [&mut [f32]]| {
        for (i, k) in knobs.iter().enumerate() {
            // A new value for every parameter every block: sixteenths,
            // stepping by one each block.
            k.set_knob(((b + i) % 16) as f32 / 16.0);
        }
        let t = Transport::new(true, Bpm(120.0), Beat(b as f64), None);
        rig.renderer().executor_mut().process_with_changes(
            BLOCK,
            &t,
            &TransportChanges::NONE,
            &inputs,
            outs,
        );
        if (b + 1).is_multiple_of(PASSES) {
            std::thread::sleep(pace);
        }
    };

    // Warm up: the first blocks fault in the slab and fill the pipeline.
    for b in 0..4 * PASSES {
        block(b, &mut rig, &mut outs);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for b in 4 * PASSES..=20 * PASSES {
            block(b, &mut rig, &mut outs);
        }
    });
    drop(outs);
    drop(handle);
    let stats = rig.controls.param_delivery();
    assert_eq!(stats.points_dropped, 0, "{stats:?}");
    assert_eq!(stats.points_merged, 0, "{stats:?}");
    assert!(
        stats.points_sent >= (PARAMS * 16) as u64,
        "every parameter moved in every chunk: {stats:?}"
    );
}
