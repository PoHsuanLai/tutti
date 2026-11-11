//! **What does one block of each per-sample kernel cost, at 64 and 1024
//! frames?**
//!
//! The nodes here are the waveshaper, the brickwall clip,
//! the width adapter, the dynamics (the lookahead limiter, the gate and the
//! compressor) and the ring-buffer family (the feedback delay, the lookahead
//! limiter's ring and, with `--features convolution`, the convolver's dry
//! alignment), and one graph, the fan-in sum (the op, through the graph). Each case
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

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_graph::Node;
use tutti_graph::contract::Direct;
use tutti_nodes::{
    BrickwallLimiterNode, CompressorNode, DelayLineNode, DesignNode, DistortionNode, FdnControls,
    FreeverbNode, GateNode, LimiterNode, ShapeKind,
};
use tutti_types::{ChannelLayout, CompressionRatio, Db, Drive, Feedback, SampleRate, Seconds};

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
        let width = shape.audio_in().count().max(shape.audio_out().count()) as usize;
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

/// The graph's fan-in (`Op::Sum`, doc 013 "Audio fan-in sums"), measured
/// through the graph: `sources` groups of `width` global input channels, each
/// output channel summing its channel of every group. No node runs, so the
/// figure is the op's.
fn channel_sum(c: &mut Criterion) {
    use tutti_graph::{Editor, Prepare, Source, Transport};
    use tutti_types::{ChannelLayout, Samples};

    let mut g = c.benchmark_group("channel_sum");
    for (case, sources, width) in [("8x2", 8u16, 2u16), ("16x1", 16, 1), ("2x6", 2, 6)] {
        for block in BLOCKS {
            let (mut ed, mut exec) = Editor::new(Prepare::new(SR, Samples(block)));
            ed.spec_mut().topology.inputs = ChannelLayout::from(sources * width);
            ed.spec_mut()
                .topology
                .set_outputs(vec![Source::Zero; width as usize]);
            for channel in 0..width {
                for k in 0..sources {
                    ed.spec_mut()
                        .topology
                        .connect_output(channel as usize, Source::Global(k * width + channel));
                }
            }
            ed.commit().expect("commits");
            exec.apply_pending();
            ed.collect();

            let mut state = 0x2545_f491_u32;
            let input: Vec<Vec<f32>> = (0..sources * width)
                .map(|_| {
                    (0..block)
                        .map(|_| {
                            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
                        })
                        .collect()
                })
                .collect();
            let input: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut out = vec![vec![0.0f32; block]; width as usize];
            let t = Transport::default();
            g.throughput(Throughput::Elements((block * width as usize) as u64));
            g.bench_function(BenchmarkId::new(case, block), |b| {
                b.iter(|| {
                    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
                    exec.process(block, &t, black_box(&input), &mut outs);
                    black_box(outs[0][block - 1]);
                })
            });
        }
    }
    g.finish();
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
        || DistortionNode::new(ShapeKind::Tanh, Drive::new(2.0)),
        None,
    );
    run(
        c,
        "distortion",
        "softsign_2",
        || DistortionNode::new(ShapeKind::Softsign, Drive::new(2.0)),
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
            || DistortionNode::new(kind, Drive::new(2.0)),
            None,
        );
    }
    run(
        c,
        "distortion",
        "hardclip_6",
        || DistortionNode::with_channels(6u16, ShapeKind::HardClip, Drive::new(2.0)),
        None,
    );
    run(
        c,
        "distortion",
        "softsign_2_fed",
        || DistortionNode::new(ShapeKind::Softsign, Drive::new(2.0)),
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
        || BrickwallLimiterNode::new(Db::new(-3.0)),
        None,
    );
    run(
        c,
        "brickwall",
        "6",
        || BrickwallLimiterNode::with_channels(ChannelLayout::from(6u16), Db::new(-3.0)),
        None,
    );
    run(
        c,
        "brickwall",
        "6_fed",
        || BrickwallLimiterNode::with_channels(ChannelLayout::from(6u16), Db::new(-3.0)),
        Some(ceiling_sweep),
    );
}

fn delay(c: &mut Criterion) {
    run(
        c,
        "delay",
        "1",
        || DelayLineNode::new(Seconds::new(1.0), Seconds::new(0.25), Feedback::new(0.4)),
        None,
    );
    run(
        c,
        "delay",
        "6",
        || {
            DelayLineNode::with_channels(
                ChannelLayout::from(6u16),
                Seconds::new(1.0),
                Seconds::new(0.25),
                Feedback::new(0.4),
            )
        },
        None,
    );
    run(
        c,
        "delay",
        "2_cross",
        || {
            let n = DelayLineNode::stereo(
                Seconds::new(1.0),
                Seconds::new(0.25),
                Seconds::new(0.3),
                Feedback::new(0.4),
            );
            n.set_cross_feedback(Feedback::new(0.3));
            n
        },
        None,
    );
}

fn limiter(c: &mut Criterion) {
    run(
        c,
        "limiter",
        "2",
        || LimiterNode::new(Db::new(-6.0), Db::new(-0.3)),
        None,
    );
    run(
        c,
        "limiter",
        "6",
        || LimiterNode::with_channels(ChannelLayout::from(6u16), Db::new(-6.0), Db::new(-0.3)),
        None,
    );
    // Over the threshold on every frame, and not at all: the two branches of
    // the gain curve.
    run(
        c,
        "limiter",
        "2_quiet",
        || LimiterNode::new(Db::new(6.0), Db::new(6.0)),
        None,
    );
    run(
        c,
        "limiter",
        "2_fed",
        || LimiterNode::new(Db::new(-6.0), Db::new(-0.3)),
        Some(ceiling_sweep),
    );
}

/// A threshold that moves every frame, between -30 and -10 dB.
fn threshold_sweep(n: usize) -> Vec<f32> {
    (0..n).map(|i| -30.0 + 20.0 * i as f32 / n as f32).collect()
}

fn dynamics(c: &mut Criterion) {
    run(
        c,
        "gate",
        "2",
        || {
            GateNode::stereo(
                Db::new(-12.0),
                Seconds::new(0.001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            )
        },
        None,
    );
    run(
        c,
        "gate",
        "6",
        || {
            GateNode::with_channels(
                Db::new(-12.0),
                Seconds::new(0.001),
                Seconds::new(0.01),
                Seconds::new(0.1),
                6,
            )
        },
        None,
    );
    run(
        c,
        "gate",
        "2_fed",
        || {
            GateNode::stereo(
                Db::new(-12.0),
                Seconds::new(0.001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            )
        },
        Some(threshold_sweep),
    );
    run(
        c,
        "compressor",
        "2",
        || {
            CompressorNode::stereo(
                Db::new(-20.0),
                CompressionRatio::new(4.0),
                Seconds::new(0.001),
                Seconds::new(0.1),
            )
        },
        None,
    );
    run(
        c,
        "compressor",
        "6",
        || {
            CompressorNode::with_channels(
                Db::new(-20.0),
                CompressionRatio::new(4.0),
                Seconds::new(0.001),
                Seconds::new(0.1),
                6,
            )
        },
        None,
    );
    run(
        c,
        "compressor",
        "2_fed",
        || {
            CompressorNode::stereo(
                Db::new(-20.0),
                CompressionRatio::new(4.0),
                Seconds::new(0.001),
                Seconds::new(0.1),
            )
        },
        Some(threshold_sweep),
    );
}

/// The reverb: one input, two outputs, so the throughput counts two channels.
fn reverb(c: &mut Criterion) {
    run(c, "reverb", "freeverb", FreeverbNode::general_midi, None);
    run(
        c,
        "reverb",
        "fdn",
        || DesignNode::live(tutti_nodes::FdnDesign::default(), FdnControls::DEFAULT),
        None,
    );
}

#[cfg(feature = "convolution")]
fn convolver(c: &mut Criterion) {
    use tutti_nodes::{ConvolverNode, generate_test_ir};
    let ir = generate_test_ir(256, Seconds::new(0.05), SR);
    run(c, "convolver", "1", || ConvolverNode::new(&ir, 64), None);
}

#[cfg(not(feature = "convolution"))]
criterion_group!(
    benches,
    channel_sum,
    distortion,
    brickwall,
    delay,
    limiter,
    dynamics,
    reverb
);
#[cfg(feature = "convolution")]
criterion_group!(
    benches,
    channel_sum,
    distortion,
    brickwall,
    delay,
    limiter,
    dynamics,
    reverb,
    convolver
);
criterion_main!(benches);
