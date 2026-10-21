//! **What does the disk tier's ring read cost per block?**
//!
//! `N` disk voices' rings, each read as `read_ring` (`voice/live_read.rs`)
//! reads one: a claim per block, then per output frame the four taps of a
//! cubic around a position that moves at a varispeed rate, each tap a
//! `PosClaim::frame` (the slot map) and a stereo load, then the cubic. No
//! butler, no decode and no voice around it, so the slot map is a visible
//! share of the time; `voice_pool`'s `pool64/disk*` cases are the same read
//! inside the whole voice.
//!
//! Each ring holds its whole window far from position 0 (ten rings' worth
//! in, as a stream some minutes into a long file is), so the slot map
//! reduces positions much larger than the ring. Two ring lengths: the 3 s of
//! `voice_pool`'s file, and the 30 s a small file's ring is capped at
//! (`buffer_size_for_file`), both at 48 kHz and neither a power of two.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tutti_core::{PosReader, PosRing, PosWriter};

/// Frames per block.
const BLOCK: usize = 64;
/// The varispeed rate every voice reads at.
const RATE: f64 = 1.37;

/// The cubic `read_ring` interpolates its four taps with
/// (`voice::interp::cubic_hermite`, which the crate does not export).
#[inline]
fn cubic(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c0 = y1;
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + c0
}

/// One disk voice: its ring's two ends and where it reads.
struct Voice {
    /// Keeps the ring's writer alive (the butler's, in the sampler).
    _writer: PosWriter,
    reader: PosReader,
    /// The window's first position.
    from: u64,
    /// Positions the window holds.
    len: u64,
    /// Where the voice reads, relative to `from`.
    pos: f64,
}

/// `n` stereo rings of `frames` frames, each filled whole at ten rings'
/// worth of positions in, every voice starting at a different place.
fn voices(n: usize, frames: usize) -> Vec<Voice> {
    let from = 10 * frames as u64;
    let data: Vec<f32> = (0..frames * 2).map(|i| (i as f32 * 1e-3).sin()).collect();
    (0..n)
        .map(|v| {
            let (mut writer, reader) = PosRing::new(frames, 2, 4);
            writer.reset(from);
            writer.set_play(from + 4);
            assert_eq!(writer.push(&data), frames, "the ring fills whole");
            Voice {
                _writer: writer,
                reader,
                from,
                len: frames as u64,
                pos: 1.0 + (v * 997 % frames) as f64,
            }
        })
        .collect()
}

/// One block of every voice; returns a sum so nothing is optimised out.
fn block(voices: &mut [Voice], out: &mut [[f32; 2]; BLOCK]) -> f32 {
    let mut sum = 0.0;
    for v in voices.iter_mut() {
        // Wind back before a tap would leave the window.
        let span = BLOCK as f64 * RATE + 4.0;
        if v.pos + span >= v.len as f64 {
            v.pos = 1.0;
        }
        let first = v.from + v.pos as u64 - 1;
        let last = v.from + (v.pos + span) as u64;
        let claim = v
            .reader
            .claim(v.from + v.pos as u64, [(first, last), (0, 0)]);
        for o in out.iter_mut() {
            let idx = v.pos as u64;
            let frac = (v.pos - idx as f64) as f32;
            v.pos += RATE;
            let s = v.from + idx;
            let taps = [s - 1, s, s + 1, s + 2];
            if !taps.iter().all(|&t| claim.holds(t)) {
                *o = [0.0; 2];
                continue;
            }
            let f = [
                claim.frame(taps[0]),
                claim.frame(taps[1]),
                claim.frame(taps[2]),
                claim.frame(taps[3]),
            ];
            for (c, o) in o.iter_mut().enumerate() {
                *o = cubic(f[0].get(c), f[1].get(c), f[2].get(c), f[3].get(c), frac);
            }
        }
        sum += out[BLOCK - 1][0];
    }
    sum
}

fn bench_ring_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring_read");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for (name, n, frames) in [
        ("3s/1", 1, 144_000),
        ("3s/64", 64, 144_000),
        ("30s/64", 64, 1_440_000),
    ] {
        let mut v = voices(n, frames);
        let mut out = [[0.0f32; 2]; BLOCK];
        group.bench_function(name, |b| b.iter(|| black_box(block(&mut v, &mut out))));
    }
    group.finish();
}

criterion_group!(benches, bench_ring_read);
criterion_main!(benches);
