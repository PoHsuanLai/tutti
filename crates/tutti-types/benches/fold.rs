//! **What does `Fold` cost against `fold_frame`?**
//!
//! Each case is one block of 64 or 512 frames: interleaved to interleaved,
//! planar to interleaved (the engine's shape: `fold_frame` per gathered frame
//! against `Fold::planar_to_interleaved`), and `Fold` planar
//! alone. Widths are 2->2, 1->2, 6->2, 8->2 and 12->2, and a 6-channel
//! permutation, which `fold_frame` cannot do (its baseline is the 6->6 copy).

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_types::{
    ChannelLayout, ChannelTopology, Fold, Interleaved, InterleavedMut, Speaker, fold_frame,
};

fn noise(n: usize) -> Vec<f32> {
    let mut x = 0x2545_f491_u32;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::smpte(ChannelLayout::from(n)).unwrap()
}

fn cases() -> Vec<(&'static str, ChannelTopology, ChannelTopology, bool)> {
    use Speaker::*;
    let dts = ChannelTopology::new([
        FrontLeft,
        FrontCenter,
        FrontRight,
        SideLeft,
        SideRight,
        LowFrequency,
    ]);
    vec![
        ("2to2", smpte(2), smpte(2), false),
        ("1to2", smpte(1), smpte(2), false),
        ("6to2", smpte(6), smpte(2), false),
        ("8to2", smpte(8), smpte(2), false),
        ("12to2", smpte(12), smpte(2), false),
        ("perm6", smpte(6), dts, true),
    ]
}

fn bench(c: &mut Criterion) {
    for block in [64usize, 512] {
        let mut g = c.benchmark_group(format!("interleaved_{block}"));
        for (name, s, d, perm) in cases() {
            let (sw, dw) = (s.positions().len(), d.positions().len());
            g.throughput(Throughput::Elements(block as u64));
            let src = noise(sw * block);
            let mut dst = vec![0.0f32; dw * block];
            let fold = Fold::between(&s, &d).unwrap();
            g.bench_function(BenchmarkId::new("fold", name), |b| {
                b.iter(|| {
                    fold.interleaved(
                        Interleaved::new(black_box(&src), s.layout()),
                        InterleavedMut::new(&mut dst, d.layout()),
                    );
                    black_box(&dst);
                })
            });
            g.bench_function(BenchmarkId::new("width", name), |b| {
                b.iter(|| {
                    for (a, o) in black_box(&src)
                        .chunks_exact(sw)
                        .zip(dst.chunks_exact_mut(dw))
                    {
                        // The width folds cannot permute: copy 6->6 instead.
                        fold_frame(a, o);
                    }
                    black_box(&dst);
                })
            });
            let _ = perm;
        }
        g.finish();

        // The engine's shape: planar graph outputs to an interleaved device
        // block. The width fold gathers each frame and folds it; `Fold` folds
        // each output channel over whole slices, then interleaves.
        let mut g = c.benchmark_group(format!("engine_{block}"));
        for (name, s, d, _) in cases() {
            let (sw, dw) = (s.positions().len(), d.positions().len());
            g.throughput(Throughput::Elements(block as u64));
            let planes: Vec<Vec<f32>> = (0..sw).map(|_| noise(block)).collect();
            let mut dst = vec![0.0f32; dw * block];
            let fold = Fold::between(&s, &d).unwrap();
            g.bench_function(BenchmarkId::new("fold", name), |b| {
                b.iter(|| {
                    fold.planar_to_interleaved(
                        |k| &planes[k][..],
                        InterleavedMut::new(&mut dst, d.layout()),
                    );
                    black_box(&dst);
                })
            });
            g.bench_function(BenchmarkId::new("width", name), |b| {
                b.iter(|| {
                    for (i, o) in dst.chunks_exact_mut(dw).enumerate() {
                        let mut frame = [0.0f32; 12];
                        for (f, p) in frame.iter_mut().zip(&planes) {
                            *f = p[i];
                        }
                        fold_frame(&frame[..sw], o);
                    }
                    black_box(&dst);
                })
            });
        }
        g.finish();

        let mut g = c.benchmark_group(format!("planar_{block}"));
        for (name, s, d, _) in cases() {
            let (sw, dw) = (s.positions().len(), d.positions().len());
            g.throughput(Throughput::Elements((block * dw) as u64));
            let planes: Vec<Vec<f32>> = (0..sw).map(|_| noise(block)).collect();
            let mut outs = vec![vec![0.0f32; block]; dw];
            let fold = Fold::between(&s, &d).unwrap();
            g.bench_function(BenchmarkId::new("fold", name), |b| {
                b.iter(|| {
                    for (ch, out) in outs.iter_mut().enumerate() {
                        fold.planar_channel(|k| &planes[k][..], ch, out);
                    }
                    black_box(&outs);
                })
            });
        }
        g.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
