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
//! Driven with `pool.process`, the block path a host runs. (A placed voice's
//! `tick` used to read one sample for a whole block, `examples/README.md`'s
//! first trap; it now seats on the clock and steps as `process` does,
//! `MemorySource::seated_position`.)

use std::f32::consts::TAU;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::BufferVec;
use tutti_core::{
    AudioUnit, Beat, Bpm, Cents, PlaybackRate, SamplePosition, SampleRate, StretchFactor, Timeline,
};
use tutti_io::Wave;
use tutti_sampler::{
    Command, DiskStreamer, DiskStreamerConfig, MemorySource, Playback, SlotId, StepOutcome, Voice,
    VoiceCommand, VoicePool, VoiceSource,
};

const SR: f64 = 48_000.0;
/// A `BufferVec` channel is `MAX_BUFFER_SIZE` frames long, and that is 64, so a
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
    fn segment_generation(&self) -> u64 {
        0
    }
}

fn tone(freq: f32, frames: usize) -> Arc<Wave> {
    let mut w = Wave::new(1, SR);
    for i in 0..frames {
        w.push_frame(&[(TAU * freq * i as f32 / SR as f32).sin()]);
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

// ---------------------------------------------------------------------------
// `pool64`: many voices, both tiers, on a clock that moves.
// ---------------------------------------------------------------------------

/// A clock the bench advances by one block after every `process`, as a host's
/// transport does, and winds back to beat 0 at [`WRAP_BEATS`].
///
/// The groups above hold their clock at beat 0, so a placed voice seats once
/// and steps on for ever: after ~3 000 blocks (4 s of wave) every read is past
/// the end and the pool is measuring the early return for silence. This
/// clock keeps every voice reading audio for the whole measurement; the
/// wind-back costs one seat (and, on the disk tier, one crossfade) per ~1 500
/// blocks.
struct Moving {
    beat: AtomicU64,
}

/// Where [`Moving`] winds back: 2 s at 120 BPM, inside the 3 s file at every
/// rate the cases below read it at (1.37x varispeed reads 2.74 s).
const WRAP_BEATS: f64 = 4.0;

impl Moving {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            beat: AtomicU64::new(0f64.to_bits()),
        })
    }

    fn advance(&self, frames: usize) {
        let now = f64::from_bits(self.beat.load(Ordering::Relaxed));
        let mut next = now + frames as f64 * 120.0 / 60.0 / SR;
        if next >= WRAP_BEATS {
            next = 0.0;
        }
        self.beat.store(next.to_bits(), Ordering::Relaxed);
    }
}

impl Timeline for Moving {
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(120.0)
    }
    fn is_rolling(&self) -> bool {
        true
    }
    fn segment_generation(&self) -> u64 {
        0
    }
}

/// Seconds of material every `pool64` voice reads.
const FILE_SECS: f64 = 3.0;

/// Which tier a `pool64` case reads.
#[derive(Clone, Copy)]
enum Tier {
    Memory,
    Disk,
}

/// A pool of `n` placed voices on one moving clock, all reading the same
/// stereo tone at `speed` and `stretch`. The disk tier's streams are on a
/// hand-driven butler whose rings hold the whole file (a file under 30 s is
/// prefilled whole), so the measurement never waits on, or steps, a butler.
struct Pool64 {
    pool: VoicePool,
    clock: Arc<Moving>,
    /// Keeps the disk tier's streams (and their rings) alive.
    _streamer: Option<DiskStreamer>,
    _dir: tempfile::TempDir,
}

/// The stereo tone every `pool64` voice reads, sample `i`.
fn tone_frame(i: usize) -> [f32; 2] {
    let t = i as f32 / SR as f32;
    [
        (TAU * 440.0 * t).sin() * 0.25,
        (TAU * 660.0 * t).sin() * 0.25,
    ]
}

fn pool64(tier: Tier, n: usize, speed: f32, stretch: f32) -> Pool64 {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tone.wav");
    let frames = (SR * FILE_SECS) as usize;
    let clock = Moving::new();
    let (mut pool, handle) =
        VoicePool::with_transport(Arc::clone(&clock) as Arc<dyn Timeline>, None);
    let mut streamer = None;
    let sources: Vec<VoiceSource> = match tier {
        Tier::Memory => {
            let mut wave = Wave::new(2, SR);
            for i in 0..frames {
                wave.push_frame(&tone_frame(i));
            }
            let wave = Arc::new(wave);
            (0..n)
                .map(|_| {
                    let mut s = MemorySource::with_transport(
                        Arc::clone(&wave),
                        Arc::clone(&clock) as Arc<dyn Timeline>,
                        Beat::new(0.0),
                        None,
                    );
                    s.set_sample_rate(SampleRate(SR));
                    VoiceSource::Memory(s)
                })
                .collect()
        }
        Tier::Disk => {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: SR as u32,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(&path, spec).expect("create wav");
            for i in 0..frames {
                for s in tone_frame(i) {
                    w.write_sample(s).expect("write wav");
                }
            }
            w.finalize().expect("finalize wav");
            let mut s =
                DiskStreamer::manual(SR, DiskStreamerConfig::default()).expect("streamer builds");
            for channel_index in 0..n {
                s.commands()
                    .send(Command::Stream {
                        channel_index,
                        file_path: path.clone(),
                        offset: SamplePosition(0.0),
                    })
                    .expect("the butler is alive");
            }
            for _ in 0..4096 {
                if s.step_once() != StepOutcome::Busy {
                    break;
                }
            }
            let voices = (0..n)
                .map(|channel_index| {
                    let mut v = s
                        .status()
                        .take_disk_voice(
                            channel_index,
                            Arc::clone(&clock) as Arc<dyn Timeline>,
                            Beat::new(0.0),
                            None,
                        )
                        .expect("a primed stream gives a voice");
                    v.set_sample_rate(SampleRate(SR));
                    VoiceSource::Disk(v)
                })
                .collect();
            streamer = Some(s);
            voices
        }
    };
    for (i, source) in sources.into_iter().enumerate() {
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(i as u128 + 1),
                voice: Box::new(Voice {
                    source,
                    play: Playback {
                        speed: PlaybackRate::new(speed),
                        stretch: StretchFactor::new(stretch),
                        ..Default::default()
                    },
                    channel_index: None,
                }),
                // Built on this (control) thread, as `VoicePoolHandle::send`
                // builds one for a real host.
                stretch: None,
            })
            .expect("the command queue has room");
    }
    pool.set_sample_rate(SampleRate(SR));
    let mut p = Pool64 {
        pool,
        clock,
        _streamer: streamer,
        _dir: dir,
    };
    // Drain the adds, then run past the vocoder's fill-up (a 2048 window at
    // 0.5x is 64 blocks of source), so a stretch case measures a sounding
    // vocoder, not its silent fill.
    let mut heard = 0.0f32;
    for _ in 0..256 {
        heard = heard.max(drive64(&mut p));
    }
    assert!(
        heard > 0.01,
        "the pool is silent after warm-up: the bench would measure nothing"
    );
    p
}

/// One block of a `pool64` case, then the clock moves on. Returns the
/// block's peak on channel 0.
fn drive64(p: &mut Pool64) -> f32 {
    let ib = BufferVec::new(0);
    let mut ob = BufferVec::new(2);
    p.pool
        .process(BLOCK, &ib.buffer_ref(), &mut ob.buffer_mut());
    p.clock.advance(BLOCK);
    let b = ob.buffer_ref();
    (0..BLOCK).fold(0.0f32, |a, i| a.max(b.at_f32(0, i).abs()))
}

/// **Many voices, both tiers, varispeed and stretch.** One block of `n`
/// voices per iteration, on a clock that moves (see [`Moving`]).
fn bench_pool64(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool64");
    group.throughput(Throughput::Elements(BLOCK as u64));
    let cases: &[(&str, Tier, usize, f32, f32)] = &[
        ("memory/1", Tier::Memory, 1, 1.0, 1.0),
        ("memory/64", Tier::Memory, 64, 1.0, 1.0),
        ("memory-varispeed/64", Tier::Memory, 64, 1.37, 1.0),
        ("memory-stretch/64", Tier::Memory, 64, 1.0, 0.5),
        ("disk/1", Tier::Disk, 1, 1.0, 1.0),
        ("disk/64", Tier::Disk, 64, 1.0, 1.0),
        ("disk-varispeed/64", Tier::Disk, 64, 1.37, 1.0),
        ("disk-stretch/64", Tier::Disk, 64, 1.0, 0.5),
    ];
    for &(name, tier, n, speed, stretch) in cases {
        let mut p = pool64(tier, n, speed, stretch);
        group.bench_function(name, |b| b.iter(|| black_box(drive64(&mut p))));
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_voices,
    bench_stretch,
    bench_pitch,
    bench_idle,
    bench_pool64
);
criterion_main!(benches);
