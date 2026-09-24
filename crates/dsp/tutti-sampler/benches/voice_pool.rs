//! **How many sample voices fit in a block?**
//!
//! `voices/plain` is the answer to read first. A 64-frame block at 48 kHz has
//! **1.333 ms**; divide a case's time by that for the fraction of the budget
//! one pool consumes, before any other track, effect or plugin.
//!
//! Memory sources only — **no butler, no disk, no page cache.** Streaming
//! from disk is dominated by the filesystem and by prefetch state, which is
//! not something criterion can summarise honestly; `examples/profile_stretch_clone.rs`
//! is the harness for anything shaped like that, and its module doc explains
//! why (an 81× wall-clock spread on identical work, where criterion's outlier
//! *rejection* would discard exactly the samples that matter).
//!
//! `voices/stretch` is the case to watch: the phase vocoder is by far the
//! most expensive thing in this crate, and `StretchFactor(1.0)` is a bypass,
//! so the 1.0-vs-0.5 gap is the whole cost of engaging it.
//!
//! Driven with `pool.process`, never `tick` — `examples/render_cases.rs:110`
//! documents why at length: `tick` has no `offset_in_block`, so 64 calls
//! against one transport reading emit the same sample 64 times.

use std::f32::consts::TAU;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::BufferVec;
use tutti_core::{AudioUnit, Beat, Bpm, Cents, StretchFactor, Timeline, Wave};
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoiceCommand, VoicePool, VoiceSource};

const SR: f64 = 48_000.0;
/// `BufferArray<U2>` is `MAX_BUFFER_SIZE` frames wide, and that is 64, so a
/// pool block cannot be longer. Block size is `tutti-core`'s axis, not this
/// crate's.
const BLOCK: usize = 64;

/// A rolling clock. Copied from `examples/render_cases.rs` rather than shared:
/// the crate's own `MockTransport` is `#[cfg(test)]`-private and a bench
/// target cannot see it — the same wall `tests/tier_parity.rs:119` documents.
#[derive(Clone)]
struct Clock {
    beat: Arc<AtomicU64>,
    tempo: f64,
}

impl Clock {
    fn new(tempo: f64) -> Self {
        Self {
            beat: Arc::new(AtomicU64::new(0f64.to_bits())),
            tempo,
        }
    }
}

impl Timeline for Clock {
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(self.tempo)
    }
    fn is_rolling(&self) -> bool {
        true
    }
}

fn tone(freq: f32, frames: usize) -> Arc<Wave> {
    let mut w = Wave::new(1, SR);
    for i in 0..frames {
        w.push((TAU * freq * i as f32 / SR as f32).sin());
    }
    Arc::new(w)
}

/// A pool with `n` voices resident, each on its own slot and pitch.
fn pool_with(n: usize, stretch: f32, cents: f32) -> VoicePool {
    let (mut pool, handle) = VoicePool::new();
    let transport = Clock::new(120.0);
    // Generous length: at a slow stretch the source is consumed much faster
    // than it is emitted, and a short wave would run dry mid-measurement and
    // quietly turn the benchmark into one of silence.
    let wave = tone(440.0, 48_000 * 4);

    for i in 0..n {
        let source = MemorySource::with_transport(
            Arc::clone(&wave),
            Arc::new(transport.clone()) as Arc<dyn Timeline>,
            Beat::new(0.0),
            None,
        );
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(i as u128 + 1),
                voice: Box::new(Voice {
                    source: VoiceSource::Memory(source),
                    play: Playback {
                        stretch: StretchFactor::new(stretch),
                        // Every voice gets the SAME pitch, deliberately.
                        //
                        // An earlier draft detuned each one by `i` cents "so
                        // nothing can be shared between them". Voices are
                        // already independent `MemorySource`s, so it shared
                        // nothing — but `Cents(0)` is a resampler *bypass*,
                        // so it put voice 0 on the fast path and voices 1..n
                        // on the pitch-shifting one. `voices/plain` then
                        // measured 1.37 µs for one voice and 144 µs for eight
                        // — 105x for 8x the work — and the jump was the
                        // benchmark's doing, not the engine's. `voices/pitch`
                        // is where the shifter is priced.
                        pitch: Cents::new(cents),
                        ..Default::default()
                    },
                    channel_index: None,
                }),
                stretch: None,
            })
            .expect("the command queue has room");
    }

    // Apply the queued commands and let any analysis window settle before the
    // measurement starts.
    let ib = BufferVec::new(2);
    let mut ob = BufferVec::new(2);
    for _ in 0..64 {
        pool.process(BLOCK, &ib.buffer_ref(), &mut ob.buffer_mut());
    }
    pool
}

fn drive(pool: &mut VoicePool) {
    let ib = BufferVec::new(2);
    let mut ob = BufferVec::new(2);
    pool.process(BLOCK, &ib.buffer_ref(), &mut ob.buffer_mut());
    black_box(ob.buffer_ref().at_f32(0, 0));
}

/// **The headline: plain playback against voice count.**
fn bench_voices(c: &mut Criterion) {
    let mut group = c.benchmark_group("voices/plain");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for n in [1usize, 8, 32, 64] {
        let mut pool = pool_with(n, 1.0, 0.0);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| drive(&mut pool))
        });
    }
    group.finish();
}

/// **What engaging the phase vocoder costs.** `StretchFactor(1.0)` is a
/// bypass, so the gap between it and any other factor is the whole price.
fn bench_stretch(c: &mut Criterion) {
    let mut group = c.benchmark_group("voices/stretch");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for factor in [1.0f32, 0.5, 2.0] {
        let mut pool = pool_with(8, factor, 0.0);
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{factor:.1}x")),
            &factor,
            |b, _| b.iter(|| drive(&mut pool)),
        );
    }
    group.finish();
}

/// Pitch shift, at a fixed voice count.
fn bench_pitch(c: &mut Criterion) {
    let mut group = c.benchmark_group("voices/pitch");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for cents in [0.0f32, 700.0] {
        let mut pool = pool_with(8, 1.0, cents);
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{cents:.0}c")),
            &cents,
            |b, _| b.iter(|| drive(&mut pool)),
        );
    }
    group.finish();
}

/// The floor: what an idle pool costs per block. Everything above is measured
/// against this.
fn bench_idle(c: &mut Criterion) {
    let mut group = c.benchmark_group("voices/idle");
    group.throughput(Throughput::Elements(BLOCK as u64));
    let mut pool = pool_with(0, 1.0, 0.0);
    group.bench_function("empty-pool", |b| b.iter(|| drive(&mut pool)));
    group.finish();
}

criterion_group!(
    benches,
    bench_voices,
    bench_stretch,
    bench_pitch,
    bench_idle
);
criterion_main!(benches);
