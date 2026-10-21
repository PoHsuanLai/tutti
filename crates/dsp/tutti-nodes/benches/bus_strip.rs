//! **What does one `BusStripNode` block cost, steady, ramping and fed?**
//!
//! Each case drives one strip through `process` with whole 128-frame blocks
//! (`tutti_graph::contract::Direct`), over broadband noise:
//!
//! - `steady`: the controls hold, so the block applies one gain per channel.
//! - `ramp`: the volume alternates between two values every block, so every
//!   block is a linear ramp.
//! - `fed`: the volume is fed per frame on its param port, as modulation
//!   does.
//!
//! `reference` is the floor a ramping stereo strip is measured against: a
//! hand-written channel-major gain ramp (`x · (a·(1−t) + b·t)`) and nothing
//! else. `empty` is the harness alone (a node that writes nothing); subtract
//! it from every row.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench bus_strip
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::{Amplitude, ChannelLayout, SampleRate};
use tutti_graph::contract::Direct;
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_nodes::BusStripNode;

const BLOCK: usize = 128;
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

/// The floor: a stereo gain ramp from 0.5 to 0.75, channel-major.
struct Reference;

/// The harness alone: a stereo node that writes nothing, so its time is
/// what `Direct` costs a block.
struct Empty;

impl Node for Empty {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Modified
    }

    fn reset(&mut self) {}
}

impl Node for Reference {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let (inputs, mut outputs) = io.split();
        for c in 0..2 {
            let (x, y) = (inputs.get(c), outputs.get(c));
            for (i, (y, x)) in y[..n].iter_mut().zip(&x[..n]).enumerate() {
                let t = (i + 1) as i32 as f32 / n as f32;
                *y = x * (0.5 * (1.0 - t) + 0.75 * t);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

fn strip(c: &mut Criterion) {
    let mut g = c.benchmark_group("bus_strip");
    for width in [2u16, 6] {
        g.throughput(Throughput::Elements((BLOCK * width as usize) as u64));

        let mut d = Direct::new(
            BusStripNode::with_channels(ChannelLayout::from(width)),
            SR,
            BLOCK,
        );
        fill_noise(&mut d);
        d.node.set_volume(Amplitude(0.5));
        d.node.set_pan(tutti_core::Pan(0.25));
        d.block();
        g.bench_with_input(BenchmarkId::new("steady", width), &width, |b, _| {
            b.iter(|| {
                d.block();
                black_box(d.output(0)[BLOCK - 1]);
            })
        });

        let mut flip = false;
        g.bench_with_input(BenchmarkId::new("ramp", width), &width, |b, _| {
            b.iter(|| {
                flip = !flip;
                d.node.set_volume(Amplitude(if flip { 0.75 } else { 0.5 }));
                d.block();
                black_box(d.output(0)[BLOCK - 1]);
            })
        });

        let sweep: Vec<f32> = (0..BLOCK).map(|i| 0.25 + i as f32 / BLOCK as f32).collect();
        d.feed(0, Some(&sweep));
        d.block();
        g.bench_with_input(BenchmarkId::new("fed", width), &width, |b, _| {
            b.iter(|| {
                d.block();
                black_box(d.output(0)[BLOCK - 1]);
            })
        });
    }

    g.throughput(Throughput::Elements((BLOCK * 2) as u64));
    let mut d = Direct::new(Reference, SR, BLOCK);
    fill_noise(&mut d);
    g.bench_function(BenchmarkId::new("reference", 2), |b| {
        b.iter(|| {
            d.block();
            black_box(d.output(0)[BLOCK - 1]);
        })
    });

    let mut d = Direct::new(Empty, SR, BLOCK);
    g.bench_function(BenchmarkId::new("empty", 2), |b| {
        b.iter(|| {
            d.block();
            black_box(d.output(0)[BLOCK - 1]);
        })
    });
    g.finish();
}

criterion_group!(benches, strip);
criterion_main!(benches);
