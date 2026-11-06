//! What does the phase vocoder cost per block?
//!
//! A stereo `stretch::Unit` processing 512-frame blocks of a sine, engaged
//! (stretch 1.5, +200 cents), at each preset window. The vocoder runs one
//! frame per hop, so the time per block is the FFT pair, the per-bin phase
//! work and the overlap-add together. Throughput is in frames.

use std::f32::consts::TAU;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_sampler::stretch::{FftSize, Unit};
use tutti_types::{Cents, StretchFactor};

const BLOCK: usize = 512;

fn stretch(c: &mut Criterion) {
    let mut group = c.benchmark_group("stretch");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for size in [FftSize::N1024, FftSize::N2048, FftSize::N4096] {
        let mut unit = Unit::with_fft_size(48_000.0, size);
        unit.set_stretch_factor(StretchFactor(1.5));
        unit.set_pitch_cents(Cents(200.0));
        let sine: Vec<f32> = (0..BLOCK)
            .map(|i| (TAU * 440.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();
        let input = [&sine[..], &sine[..]];
        let mut left = vec![0.0f32; BLOCK];
        let mut right = vec![0.0f32; BLOCK];
        // Warm up past the latency so every block runs frames.
        for _ in 0..16 {
            let mut out = [&mut left[..], &mut right[..]];
            unit.process(BLOCK, &input, &mut out);
        }
        group.bench_function(BenchmarkId::from_parameter(size), |b| {
            b.iter(|| {
                let mut out = [&mut left[..], &mut right[..]];
                unit.process(BLOCK, black_box(&input), &mut out);
                black_box(&out);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, stretch);
criterion_main!(benches);
