//! **What does one block of VBAP panning cost, at 2 and 6 speakers, for a
//! 64-frame and a 1024-frame block?**
//!
//! A 64-frame block at 48 kHz has **1.333 ms** (1024 frames: 21.3 ms); divide
//! a case's time by that for the fraction of the budget one panned source
//! consumes.
//!
//! `vbap_static` holds the source still, warmed up for two seconds of audio
//! first so the 50 ms de-zipper has reached its fixed point (a shorter warmup
//! measures the tail of the initial glide instead); `vbap_moving` writes a new
//! bearing every block, which keeps the ramp permanently in flight — the case
//! where the gain vector genuinely changes inside a block. Both feed a stereo
//! pair at the default width, the branch that solves two virtual sources.
//!
//! The panner is a graph node, called by hand through
//! `tutti_graph::contract::Direct` (no graph around it, no allocation per
//! block).

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::{Azimuth, ChannelLayout, Elevation, SampleRate};
use tutti_graph::contract::Direct;
use tutti_spatial::VbapPannerNode;

/// Warmup before measuring, in frames: two seconds at 48 kHz, 40 time
/// constants of the de-zipper, so a held source is settled bit for bit.
const WARMUP_FRAMES: usize = 96_000;

/// Deterministic broadband input: a cheap LCG, so the bench needs no `rand`.
fn fill_noise(channels: &mut [Vec<f32>]) {
    let mut state = 0x2545_f491_u32;
    for c in channels {
        for x in c.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *x = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
        }
    }
}

fn bench(c: &mut Criterion) {
    for moving in [false, true] {
        let name = if moving { "vbap_moving" } else { "vbap_static" };
        let mut g = c.benchmark_group(name);
        for (speakers, block) in [(2u16, 64usize), (6, 64), (2, 1024), (6, 1024)] {
            g.throughput(Throughput::Elements(block as u64));
            let node = VbapPannerNode::for_layout(ChannelLayout::from(speakers)).unwrap();
            node.set_position(Azimuth(20.0), Elevation::LEVEL);
            let mut d = Direct::new(node, SampleRate(48_000.0), block);
            fill_noise(d.inputs_mut());
            for _ in 0..WARMUP_FRAMES / block {
                d.block();
            }
            let mut bearing = 0.0f32;
            let id = BenchmarkId::new(format!("{speakers}sp"), block);
            g.bench_with_input(id, &speakers, |b, _| {
                b.iter(|| {
                    if moving {
                        bearing = if bearing > 170.0 {
                            -170.0
                        } else {
                            bearing + 7.0
                        };
                        d.node.set_position(Azimuth(bearing), Elevation::LEVEL);
                    }
                    d.block();
                    black_box(d.output(0)[block - 1]);
                })
            });
        }
        g.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
