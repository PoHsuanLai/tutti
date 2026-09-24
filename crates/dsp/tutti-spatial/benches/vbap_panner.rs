//! **What does one 64-frame block of VBAP panning cost, at 2 and 6 speakers?**
//!
//! A 64-frame block at 48 kHz has **1.333 ms**; divide a case's time by that
//! for the fraction of the budget one panned source consumes.
//!
//! `vbap_static` holds the source still; `vbap_moving` writes a new bearing
//! every block, which keeps the 50 ms de-zipper ramp permanently in flight —
//! the case where the gain vector genuinely changes inside a block. Both feed a
//! stereo pair at the default width, the branch that solves two virtual
//! sources.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::{AudioUnit, Azimuth, BufferVec, ChannelLayout, Elevation, SampleRate};
use tutti_spatial::VbapPannerNode;

const BLOCK: usize = 64;

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

fn bench(c: &mut Criterion) {
    for moving in [false, true] {
        let name = if moving { "vbap_moving" } else { "vbap_static" };
        let mut g = c.benchmark_group(name);
        for speakers in [2u16, 6] {
            g.throughput(Throughput::Elements(BLOCK as u64));
            let mut node = VbapPannerNode::for_layout(ChannelLayout::from(speakers)).unwrap();
            node.set_sample_rate(SampleRate(48_000.0));
            node.set_position(Azimuth(20.0), Elevation::LEVEL);
            let input = noise_block(2);
            let mut out = BufferVec::new(node.outputs());
            for _ in 0..16 {
                node.process(BLOCK, &input.buffer_ref(), &mut out.buffer_mut());
            }
            let mut bearing = 0.0f32;
            g.bench_with_input(BenchmarkId::from_parameter(speakers), &speakers, |b, _| {
                b.iter(|| {
                    if moving {
                        bearing = if bearing > 170.0 {
                            -170.0
                        } else {
                            bearing + 7.0
                        };
                        node.set_position(Azimuth(bearing), Elevation::LEVEL);
                    }
                    node.process(BLOCK, black_box(&input.buffer_ref()), &mut out.buffer_mut());
                    black_box(out.at_f32(0, BLOCK - 1));
                })
            });
        }
        g.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
