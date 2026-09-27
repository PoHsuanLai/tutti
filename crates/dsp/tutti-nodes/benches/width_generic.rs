//! **What does one 64-frame block of each per-channel effect cost, at 2 and 6
//! channels?**
//!
//! A 64-frame block at 48 kHz has **1.333 ms**; divide a case's time by that
//! for the fraction of the budget one instance consumes.
//!
//! Every case is driven through `process`, never `tick`: the per-block param
//! read and the channel-outer planar loop are exactly what `tick` cannot show.
//! Each node is primed with a few blocks first so the coefficient cache and any
//! delay line are warm, and the input is broadband noise rather than silence —
//! a filter fed zeros can take denormal-free shortcuts a real signal never does.
//!
//! The `*_mod` cases feed the cutoff per frame, as the graph's param
//! modulation does, with a sweep that moves every sample: the path where a
//! per-sample coefficient solve (a `tan` per sample for the SVF) used to be
//! the dominant cost.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::{BufferVec, ChannelLayout, SampleRate};
use tutti_graph::contract::Direct;
use tutti_graph::Node;
use tutti_nodes::{
    DelayLineNode, LadderFilterNode, LadderType, ModDelayNode, PhaserNode, SvfFilterNode, SvfType,
};

const BLOCK: usize = 64;
const SR: SampleRate = SampleRate(48_000.0);

/// Deterministic broadband input: a cheap LCG, so the bench needs no `rand`.
fn noise_block(channels: usize) -> BufferVec {
    let mut buf = BufferVec::new(channels);
    let mut state = 0x2545_f491_u32;
    for c in 0..channels {
        for i in 0..BLOCK {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            buf.set_f32(c, i, (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0);
        }
    }
    buf
}

/// A cutoff sweep that moves every sample.
fn sweep(lo: f32, hi: f32) -> [f32; BLOCK] {
    std::array::from_fn(|i| lo + (hi - lo) * (i as f32 / BLOCK as f32))
}

/// [`run`] for a graph node, called by hand through
/// `tutti_graph::contract::Direct`; `fed` feeds its first param port.
fn run_node(c: &mut Criterion, group: &str, width: usize, node: impl Node, fed: Option<&[f32]>) {
    let mut g = c.benchmark_group(group);
    g.throughput(Throughput::Elements((BLOCK * width) as u64));
    let mut d = Direct::new(node, SR, BLOCK);
    let input = noise_block(width);
    for (ch, buf) in d.inputs_mut().iter_mut().enumerate() {
        for (i, x) in buf.iter_mut().enumerate() {
            *x = input.at_f32(ch, i);
        }
    }
    // Live until cleared: every block of the bench reads it. A node with no
    // param ports (the chorus, the phaser) has nothing to feed.
    if fed.is_some() {
        d.feed(0, fed);
    }
    for _ in 0..16 {
        d.block();
    }
    g.bench_with_input(BenchmarkId::from_parameter(width), &width, |b, _| {
        b.iter(|| {
            d.block();
            black_box(d.output(0)[BLOCK - 1]);
        })
    });
    g.finish();
}

fn svf(c: &mut Criterion) {
    for w in [2usize, 6] {
        let make = || {
            SvfFilterNode::<f64>::with_channels(
                ChannelLayout::from(w),
                SvfType::LowPass,
                1_000.0,
                0.707,
            )
        };
        run_node(c, "svf", w, make(), None);
        run_node(c, "svf_mod", w, make(), Some(&sweep(300.0, 6_000.0)));
    }
}

fn ladder(c: &mut Criterion) {
    for w in [2usize, 6] {
        let node = LadderFilterNode::<f64>::with_channels(
            ChannelLayout::from(w),
            LadderType::LP24,
            1_000.0,
            0.5,
        );
        run_node(c, "ladder", w, node, None);
    }
}

fn delay(c: &mut Criterion) {
    for w in [2usize, 6] {
        let node = DelayLineNode::with_channels(ChannelLayout::from(w), 1.0, 0.25, 0.4);
        run_node(c, "delay", w, node, None);
    }
}

fn mod_delay(c: &mut Criterion) {
    for w in [2usize, 6] {
        run_node(
            c,
            "chorus",
            w,
            ModDelayNode::chorus(ChannelLayout::from(w)),
            None,
        );
    }
}

fn phaser(c: &mut Criterion) {
    for w in [2usize, 6] {
        run_node(
            c,
            "phaser",
            w,
            PhaserNode::with_channels(ChannelLayout::from(w), 6),
            None,
        );
    }
}

criterion_group!(benches, svf, ladder, delay, mod_delay, phaser);
criterion_main!(benches);
