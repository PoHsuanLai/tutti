//! **What does one block of each per-sample kernel cost, at 64 and 1024
//! frames?**
//!
//! The nodes here are the summing bus, the waveshaper, the brickwall clip,
//! the width adapter and the ring-buffer family (the feedback delay, the
//! lookahead limiter's ring and, with `--features convolution`, the
//! convolver's dry alignment). Each case
//! drives one node through `process` with whole blocks
//! (`tutti_graph::contract::Direct`) over broadband noise, after a few
//! warm-up blocks. A `fed` case feeds the node's first param port per frame,
//! as modulation does.
//!
//! Throughput is in channel-samples (frames times the wider side's width), so
//! rows of different widths compare per sample.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench node_kernels
//! cargo bench -p tutti-nodes --bench node_kernels --features convolution
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::{ChannelLayout, SampleRate};
use tutti_graph::contract::Direct;
use tutti_graph::Node;
use tutti_nodes::{
    BrickwallLimiterNode, ChannelSumNode, DelayLineNode, DistortionNode, DownmixNode, LimiterNode,
    ShapeKind,
};

const BLOCKS: [usize; 2] = [64, 1024];
const SR: SampleRate = SampleRate(48_000.0);

/// Broadband input from a cheap LCG, so the bench needs no `rand`.
fn fill_noise<N: Node>(d: &mut Direct<N>) {
    let mut state = 0x2545_f491_u32;
    for ch in d.inputs_mut() {
        for s in ch.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
        }
    }
}

/// Bench `make()` as `group/case/block` at every block size, `fed` (built
/// for the block length) on its first param port.
fn run<N: Node>(
    c: &mut Criterion,
    group: &str,
    case: &str,
    make: impl Fn() -> N,
    fed: Option<fn(usize) -> Vec<f32>>,
) {
    let mut g = c.benchmark_group(group);
    for block in BLOCKS {
        let mut d = Direct::new(make(), SR, block);
        let shape = d.node.shape();
        let width = shape.audio_in.count().max(shape.audio_out.count()) as usize;
        g.throughput(Throughput::Elements((block * width) as u64));
        fill_noise(&mut d);
        let values = fed.map(|f| f(block));
        if let Some(v) = &values {
            d.feed(0, Some(v));
        }
        for _ in 0..8 {
            d.block();
        }
        g.bench_function(BenchmarkId::new(case, block), |b| {
            b.iter(|| {
                d.block();
                black_box(d.output(0)[block - 1]);
            })
        });
    }
    g.finish();
}

fn channel_sum(c: &mut Criterion) {
    run(
        c,
        "channel_sum",
        "8x2",
        || ChannelSumNode::new(8, ChannelLayout::STEREO),
        None,
    );
    run(
        c,
        "channel_sum",
        "16x1",
        || ChannelSumNode::new(16, ChannelLayout::MONO),
        None,
    );
    run(
        c,
        "channel_sum",
        "2x6",
        || ChannelSumNode::new(2, ChannelLayout::from(6u16)),
        None,
    );
}

/// A drive that moves every frame, between 1 and 4.
fn drive_sweep(n: usize) -> Vec<f32> {
    (0..n).map(|i| 1.0 + 3.0 * i as f32 / n as f32).collect()
}

fn distortion(c: &mut Criterion) {
    run(
        c,
        "distortion",
        "tanh_2",
        || DistortionNode::new(ShapeKind::Tanh, 2.0),
        None,
    );
    run(
        c,
        "distortion",
        "softsign_2",
        || DistortionNode::new(ShapeKind::Softsign, 2.0),
        None,
    );
    for (case, kind) in [
        ("atan_2", ShapeKind::Atan),
        ("crush_2", ShapeKind::Crush),
        ("softcrush_2", ShapeKind::SoftCrush),
    ] {
        run(
            c,
            "distortion",
            case,
            || DistortionNode::new(kind, 2.0),
            None,
        );
    }
    run(
        c,
        "distortion",
        "hardclip_6",
        || DistortionNode::with_channels(6, ShapeKind::HardClip, 2.0),
        None,
    );
    run(
        c,
        "distortion",
        "softsign_2_fed",
        || DistortionNode::new(ShapeKind::Softsign, 2.0),
        Some(drive_sweep),
    );
}

/// A ceiling that moves every frame, between -6 and -1 dB.
fn ceiling_sweep(n: usize) -> Vec<f32> {
    (0..n).map(|i| -6.0 + 5.0 * i as f32 / n as f32).collect()
}

fn brickwall(c: &mut Criterion) {
    run(
        c,
        "brickwall",
        "2",
        || BrickwallLimiterNode::new(-3.0),
        None,
    );
    run(
        c,
        "brickwall",
        "6",
        || BrickwallLimiterNode::with_channels(ChannelLayout::from(6u16), -3.0),
        None,
    );
    run(
        c,
        "brickwall",
        "6_fed",
        || BrickwallLimiterNode::with_channels(ChannelLayout::from(6u16), -3.0),
        Some(ceiling_sweep),
    );
}

fn downmix(c: &mut Criterion) {
    for (src, dst) in [(6u16, 2u16), (8, 2), (12, 2), (6, 1), (2, 6)] {
        run(
            c,
            "downmix",
            &format!("{src}to{dst}"),
            || DownmixNode::new(ChannelLayout::from(src), ChannelLayout::from(dst)),
            None,
        );
    }
}

fn delay(c: &mut Criterion) {
    run(c, "delay", "1", || DelayLineNode::new(1.0, 0.25, 0.4), None);
    run(
        c,
        "delay",
        "6",
        || DelayLineNode::with_channels(ChannelLayout::from(6u16), 1.0, 0.25, 0.4),
        None,
    );
    run(
        c,
        "delay",
        "2_cross",
        || {
            let n = DelayLineNode::stereo(1.0, 0.25, 0.3, 0.4);
            n.set_cross_feedback(0.3);
            n
        },
        None,
    );
    run(c, "limiter", "2", || LimiterNode::new(-6.0, -0.3), None);
    #[cfg(feature = "convolution")]
    {
        use tutti_nodes::{generate_test_ir, ConvolverNode};
        let ir = generate_test_ir(256, 0.05, SR);
        run(c, "convolver", "1", || ConvolverNode::new(&ir, 64), None);
    }
}

criterion_group!(benches, channel_sum, distortion, brickwall, downmix, delay);
criterion_main!(benches);
