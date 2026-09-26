//! **A plugin's output is its input delayed by exactly the latency it
//! declares, however the graph cuts its blocks** — through the reference CLAP
//! plugin in a real `plugin-server`, sample for sample.
//!
//! The plugin node ships whole chunks through a FIFO (the batcher's module
//! docs), so a block that is not a whole number of chunks — an export's
//! 100-frame blocks, a 441- or 480-frame device quantum rendered in 64-frame
//! passes plus a remainder — neither drops nor zero-pads a frame. Before the
//! FIFO, a chunk shorter than the one before it lost the difference: 2 303 of
//! 3 399 samples wrong at 100-frame blocks.
//!
//! The probe runs in `Latency` mode: it delays its input by the 137 frames it
//! reports. The node adds its pipeline chunk (the device's callback when the
//! graph knows it; here, an offline fork, the graph's `MaxBlock`), and
//! declares the sum. An offline fork waits for
//! every chunk, so each render here is deterministic, unpaced.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv, Rig};

use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_graph::{ForkMode, Transport};
use tutti_types::{SampleRate, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
/// What the probe declares and delays by (`REPORTED_LATENCY_SAMPLES`).
const PROBE_LATENCY: usize = 137;

/// Input frame `t`: distinct, never zero, exact in `f32`.
fn input(t: usize) -> f32 {
    ((t % 997) + 1) as f32 / 1024.0
}

/// An offline fork of the probe in `Latency` mode, as the only node of a graph
/// prepared for blocks of up to `max_block` frames.
fn fork_rig(max_block: usize) -> (Rig, tutti_plugin::handles::PluginHandle) {
    let probe = load_probe(SAMPLE_RATE);
    let offline = OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    })));
    let fork = probe
        .client
        .fork_instance(ForkMode::Offline(&offline))
        .expect("the probe forks");
    (Rig::new(fork, SAMPLE_RATE, max_block), probe.handle)
}

/// Render `blocks` (frame counts) of the ramp through `rig`, one executor
/// call per block; return output channel 0.
fn render_blocks(rig: &mut Rig, blocks: &[usize]) -> Vec<f32> {
    let ins = rig.inputs();
    let mut out = Vec::new();
    let mut t = 0;
    for &n in blocks {
        let chan: Vec<f32> = (t..t + n).map(input).collect();
        let inputs: Vec<&[f32]> = (0..ins).map(|_| &chan[..]).collect();
        let mut o = vec![vec![0.0f32; n]; rig.outputs()];
        let mut outs: Vec<&mut [f32]> = o.iter_mut().map(|c| &mut c[..]).collect();
        let transport = Transport::default();
        rig.renderer()
            .executor_mut()
            .process(n, &transport, &inputs, &mut outs);
        rig.renderer().editor_mut().collect();
        out.extend_from_slice(&o[0]);
        t += n;
    }
    out
}

/// Every frame of `out` is the input `latency` frames earlier (silence before
/// it), exactly.
fn assert_delayed(out: &[f32], latency: usize, what: &str) {
    let wrong: Vec<usize> = (0..out.len())
        .filter(|&t| {
            let want = if t < latency { 0.0 } else { input(t - latency) };
            out[t] != want
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "{what}: {} of {} samples are not the input delayed by {latency}; \
         first at {:?}",
        wrong.len(),
        out.len(),
        wrong.first()
    );
}

/// `n` device blocks of `quantum` frames, each rendered as the engine renders
/// a graph holding a `Legacy`-flagged node: 64-frame passes plus the
/// remainder (`LEGACY_CHUNK`).
fn live_shaped(quantum: usize, n: usize) -> Vec<usize> {
    let mut blocks = Vec::new();
    for _ in 0..n {
        let mut left = quantum;
        while left > 0 {
            let b = left.min(64);
            blocks.push(b);
            left -= b;
        }
    }
    blocks
}

/// **Ragged blocks lose nothing.** 100-frame blocks, and 441- and 480-frame
/// device quanta cut as the engine cuts them (64s plus a 57 or a 32), and
/// whole 441-frame blocks: the output is the input delayed by the declared
/// 137 + 512 (the fork's `MaxBlock`) = 649, sample for sample, and the graph
/// declares exactly that.
///
/// Mutation: ship a partial chunk at the end of every call (in
/// `Batcher::process`, `if self.pos == chunk || i == frames`) → the chunks
/// no longer line up with the ring → fails for every ragged cut. Mutation:
/// collect into the ring before submitting (`collect` after `submit` in the
/// same step) → the output is a chunk early → fails.
#[test]
fn ragged_blocks_are_delayed_by_exactly_the_declared_latency() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    const DECLARED: usize = PROBE_LATENCY + 512;

    let cuts: [(&str, Vec<usize>); 4] = [
        ("100-frame blocks", vec![100; 34]),
        ("441-frame quanta in 64s", live_shaped(441, 8)),
        ("480-frame quanta in 64s", live_shaped(480, 8)),
        ("whole 441-frame blocks", vec![441; 8]),
    ];
    for (what, blocks) in cuts {
        let (mut rig, _probe) = fork_rig(512);
        assert_eq!(rig.latency(), Samples(DECLARED), "{what}: declared");
        let out = render_blocks(&mut rig, &blocks);
        assert_delayed(&out, DECLARED, what);
    }
}

/// **A graph that knows no device quantum ships `MaxBlock` chunks, and
/// declares so.** A graph that never hands the node more than 32 frames gets
/// a 32-frame chunk:
/// the node declares 137 + 32 = 169 and delays by exactly that, in 32-frame
/// blocks and in ragged 20- and 12-frame ones.
///
/// Mutation: drop `controls.set_pipeline(..)` from the node's `prepare` →
/// the node declares the slab's 4096 + 137 while delaying 169 → fails.
#[test]
fn a_max_block_below_the_chunk_is_the_pipeline() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    const DECLARED: usize = PROBE_LATENCY + 32;

    let (mut rig, _probe) = fork_rig(32);
    assert_eq!(rig.latency(), Samples(DECLARED), "declared");
    assert_eq!(rig.controls.declared_latency().samples(), Samples(DECLARED));
    let out = render_blocks(&mut rig, &[32; 40]);
    assert_delayed(&out, DECLARED, "32-frame blocks");

    let (mut rig, _probe) = fork_rig(32);
    let out = render_blocks(&mut rig, &[20, 12].repeat(40));
    assert_delayed(&out, DECLARED, "20- and 12-frame blocks");
}
