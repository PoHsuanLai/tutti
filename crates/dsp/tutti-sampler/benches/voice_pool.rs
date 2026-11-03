//! **How many sample voices fit in a block?**
//!
//! `voices/plain` is the answer to read first. A 64-frame block at 48 kHz has
//! **1.333 ms**; divide a case's time by that for the fraction of the budget
//! one pool consumes, before any other track, effect or plugin.
//!
//! Memory sources only — **no butler, no disk, no page cache.** Streaming
//! from disk is dominated by the filesystem and by prefetch state, which is
//! not something criterion can summarise honestly; a counting-allocator and
//! sampling-profiler harness is the instrument for anything shaped like that
//! (spreads of 81× in wall clock on identical work are possible there, and
//! criterion's outlier *rejection* would discard exactly the samples that
//! matter).
//!
//! `voices/stretch` is the case to watch: the phase vocoder is by far the
//! most expensive thing in this crate, and `StretchFactor(1.0)` is a bypass,
//! so the 1.0-vs-0.5 gap is the whole cost of engaging it.
//!
//! Driven through `tutti_graph::contract::Direct` (the pool called by hand,
//! its buffers built once), each block under the `Env` of a transport that
//! moves a block at a time, as a host's does (see [`Rig`]).

use std::f32::consts::TAU;
use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::{Beat, Bpm, Cents, PlaybackRate, SamplePosition, SampleRate, StretchFactor};
use tutti_graph::contract::Direct;
use tutti_io::Wave;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{
    Command, DiskStreamer, DiskStreamerConfig, LoopSetting, MemorySource, Playback, SlotId,
    StepOutcome, Voice, VoiceCommand, VoiceNode, VoicePool, VoiceSource,
};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;

/// Where the transport winds back: 2 s at 120 BPM, inside every wave the
/// cases below read, at every rate they read it at (1.37x varispeed reads
/// 2.74 s of the 3 s `pool64` file).
const WRAP_BEATS: f64 = 4.0;

/// A pool driven a block at a time under a transport that moves one block
/// after every block, as a host's does, and winds back to beat 0 at
/// [`WRAP_BEATS`].
///
/// A transport held still would read as a seek on every block (a placed
/// voice's block starts where the last one should have ended, or it is a
/// jump, which flushes the voice); one that only moved would run every read
/// past the end of its wave after a few thousand blocks, and the pool would
/// measure the early return for silence. The wind-back costs one seat (and,
/// on the disk tier, one crossfade) per ~1 500 blocks.
struct Rig {
    direct: Direct<VoicePool>,
    t: Arc<MockTransport>,
    /// Frames per block, and the pool's `max_block`.
    block: usize,
}

impl Rig {
    fn new(pool: VoicePool) -> Self {
        Self::with_block(pool, BLOCK)
    }

    /// A rig whose blocks (and `max_block`) are `block` frames.
    fn with_block(pool: VoicePool, block: usize) -> Self {
        Self {
            direct: Direct::new(pool, SampleRate(SR), block),
            t: MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0)),
            block,
        }
    }

    /// One block, then the transport moves on. Returns the block's peak on
    /// channel 0.
    fn block(&mut self) -> f32 {
        self.direct.block_in(&self.t.env(self.block, SR));
        self.t.advance(self.block as i64, SR);
        if self.t.beat().get() >= WRAP_BEATS {
            self.t.seek(Beat::new(0.0));
        }
        self.direct
            .output(0)
            .iter()
            .fold(0.0f32, |a, s| a.max(s.abs()))
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
fn pool_with(n: usize, stretch: f32, cents: f32) -> Rig {
    let (pool, handle) = VoicePool::new().with_handle();
    // Generous length: at a slow stretch the source is consumed much faster
    // than it is emitted, and a short wave would run dry mid-measurement and
    // quietly turn the benchmark into one of silence.
    let wave = tone(440.0, 48_000 * 4);

    for i in 0..n {
        let source = MemorySource::placed(Arc::clone(&wave), Beat::new(0.0), None);
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(i as u128 + 1),
                voice: Box::new(Voice {
                    source: VoiceSource::Memory(source),
                    play: Playback {
                        stretch: StretchFactor::new(stretch),
                        // Every voice gets the SAME pitch, deliberately.
                        // `Cents(0)` is a resampler *bypass*: detuning each
                        // voice by `i` cents would put voice 0 on the fast
                        // path and voices 1..n on the pitch-shifting one, and
                        // `voices/plain` would price the shifter (1.37 µs for
                        // one voice, 144 µs for eight). `voices/pitch` is
                        // where the shifter is priced.
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
    let mut rig = Rig::new(pool);
    for _ in 0..64 {
        rig.block();
    }
    rig
}

fn drive(rig: &mut Rig) {
    black_box(rig.block());
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
// `pool64`: many voices, both tiers, on a transport that moves.
// ---------------------------------------------------------------------------

/// Seconds of material every `pool64` voice reads.
const FILE_SECS: f64 = 3.0;

/// Which tier a `pool64` case reads.
#[derive(Clone, Copy)]
enum Tier {
    Memory,
    Disk,
}

/// A pool of `n` placed voices on the moving transport, all reading the same
/// stereo tone at `speed` and `stretch`. The disk tier's streams are on a
/// hand-driven butler whose rings hold the whole file (a file under 30 s is
/// prefilled whole), so the measurement never waits on, or steps, a butler.
struct Pool64 {
    rig: Rig,
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
    pool_blocks(tier, n, speed, stretch, BLOCK)
}

/// What a `pool64` voice plays beyond its rate: its loop, and where its
/// window starts.
#[derive(Clone, Copy)]
struct Knobs {
    loop_: LoopSetting,
    /// Past [`WRAP_BEATS`], the window is never reached: every voice is
    /// resident and silent.
    start: Beat,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            loop_: LoopSetting::Off,
            start: Beat::new(0.0),
        }
    }
}

/// A one-second loop with a 256-frame crossfade, inside every `pool64`
/// file: the transport reaches its end, and wraps through it, before the
/// rig winds back.
const LOOP: LoopSetting = LoopSetting::On {
    start: SamplePosition(12_000.0),
    end: SamplePosition(60_000.0),
    crossfade_frames: 256,
};

/// [`pool64`] driven in `block`-frame blocks (the pool prepared for them).
fn pool_blocks(tier: Tier, n: usize, speed: f32, stretch: f32, block: usize) -> Pool64 {
    pool_knobs(tier, n, speed, stretch, block, Knobs::default())
}

/// [`pool_blocks`] with `knobs`.
fn pool_knobs(
    tier: Tier,
    n: usize,
    speed: f32,
    stretch: f32,
    block: usize,
    knobs: Knobs,
) -> Pool64 {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tone.wav");
    let frames = (SR * FILE_SECS) as usize;
    let (pool, handle) = VoicePool::new().with_handle();
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
                    VoiceSource::Memory(MemorySource::placed(Arc::clone(&wave), knobs.start, None))
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
                    let v = s
                        .status()
                        .take_disk_voice(channel_index, knobs.start, None)
                        .expect("a primed stream gives a voice");
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
                        loop_: knobs.loop_,
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
    // The rig prepares the pool, which re-rates every voice it holds.
    let mut p = Pool64 {
        rig: Rig::with_block(pool, block),
        _streamer: streamer,
        _dir: dir,
    };
    // Drain the adds, then run past the vocoder's fill-up (a 2048 window at
    // 0.5x is 64 blocks of source), so a stretch case measures a sounding
    // vocoder, not its silent fill. 256 blocks of 64 frames, however long
    // a block is.
    let mut heard = 0.0f32;
    for _ in 0..(256 * BLOCK).div_ceil(block) {
        heard = heard.max(drive64(&mut p));
    }
    if knobs.start.get() < WRAP_BEATS {
        assert!(
            heard > 0.01,
            "the pool is silent after warm-up: the bench would measure nothing"
        );
    } else {
        assert_eq!(heard, 0.0, "a voice out of its window sounds");
    }
    p
}

/// One block of a `pool64` case, then the transport moves on. Returns the
/// block's peak on channel 0.
fn drive64(p: &mut Pool64) -> f32 {
    p.rig.block()
}

/// **Many voices, both tiers, varispeed and stretch.** One block of `n`
/// voices per iteration, on a transport that moves (see [`Rig`]).
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
    // Every voice resident and out of its window: what a pool of clips
    // that are not playing yet costs.
    let silent = Knobs {
        start: Beat::new(1_000.0),
        ..Knobs::default()
    };
    let looped = Knobs {
        loop_: LOOP,
        ..Knobs::default()
    };
    let knobbed: &[(&str, Tier, f32, Knobs)] = &[
        ("memory-silent/64", Tier::Memory, 1.0, silent),
        ("memory-loop/64", Tier::Memory, 1.0, looped),
        ("memory-loop-varispeed/64", Tier::Memory, 1.37, looped),
    ];
    for &(name, tier, speed, knobs) in knobbed {
        let mut p = pool_knobs(tier, 64, speed, 1.0, BLOCK, knobs);
        group.bench_function(name, |b| b.iter(|| black_box(drive64(&mut p))));
    }
    group.finish();
}

/// [`bench_pool64`]'s cases in 512-frame blocks: what a host that calls
/// back with 512 frames pays per block (a voice renders a block in one
/// piece, its scratch sized from `Prepare::max_block`). Divide by 8 to
/// compare with `pool64` per 64 frames.
fn bench_pool512(c: &mut Criterion) {
    const BLOCK512: usize = 512;
    let mut group = c.benchmark_group("pool512");
    group.throughput(Throughput::Elements(BLOCK512 as u64));
    let cases: &[(&str, Tier, usize, f32, f32)] = &[
        ("memory/64", Tier::Memory, 64, 1.0, 1.0),
        ("memory-varispeed/64", Tier::Memory, 64, 1.37, 1.0),
        ("memory-stretch/64", Tier::Memory, 64, 1.0, 0.5),
        ("disk/64", Tier::Disk, 64, 1.0, 1.0),
        ("disk-varispeed/64", Tier::Disk, 64, 1.37, 1.0),
    ];
    for &(name, tier, n, speed, stretch) in cases {
        let mut p = pool_blocks(tier, n, speed, stretch, BLOCK512);
        group.bench_function(name, |b| b.iter(|| black_box(drive64(&mut p))));
    }
    group.finish();
}

/// `n` clips, each its own `VoiceNode` (how `bevy-tutti` spawns a clip:
/// one node per clip, each with its own scratch), in `block`-frame blocks on
/// the moving transport. One block of every node per iteration.
struct Clips {
    nodes: Vec<Direct<VoiceNode>>,
    t: Arc<MockTransport>,
    block: usize,
}

impl Clips {
    fn new(n: usize, block: usize) -> Self {
        let mut wave = Wave::new(2, SR);
        for i in 0..(SR * FILE_SECS) as usize {
            wave.push_frame(&tone_frame(i));
        }
        let wave = Arc::new(wave);
        let nodes = (0..n)
            .map(|_| {
                let source = MemorySource::placed(Arc::clone(&wave), Beat::new(0.0), None);
                let node = VoiceNode::new(Voice {
                    source: VoiceSource::Memory(source),
                    play: Playback::default(),
                    channel_index: None,
                });
                Direct::new(node, SampleRate(SR), block)
            })
            .collect();
        Self {
            nodes,
            t: MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0)),
            block,
        }
    }

    fn block(&mut self) -> f32 {
        let env = self.t.env(self.block, SR);
        let mut peak = 0.0f32;
        for d in &mut self.nodes {
            d.block_in(&env);
            peak = peak.max(d.output(0)[self.block - 1].abs());
        }
        self.t.advance(self.block as i64, SR);
        if self.t.beat().get() >= WRAP_BEATS {
            self.t.seek(Beat::new(0.0));
        }
        peak
    }
}

/// **A clip per `VoiceNode`**, as `bevy-tutti` spawns them: 64 stereo
/// nodes, at 64- and 512-frame blocks (each node's scratch sized for its
/// block: 57 bytes a stereo frame).
fn bench_clips(c: &mut Criterion) {
    let mut group = c.benchmark_group("clips");
    for block in [64usize, 512] {
        group.throughput(Throughput::Elements(block as u64));
        let mut clips = Clips::new(64, block);
        for _ in 0..(256 * BLOCK).div_ceil(block) {
            clips.block();
        }
        group.bench_function(format!("voice-node/64/block-{block}"), |b| {
            b.iter(|| black_box(clips.block()))
        });
    }
    group.finish();
}

/// **A `MemorySource` as a graph node of its own**: 64 placed stereo
/// sources, each driven as the graph drives it, in 64-frame blocks on the
/// moving transport. One block of every source per iteration.
fn bench_sources(c: &mut Criterion) {
    let mut group = c.benchmark_group("sources");
    group.throughput(Throughput::Elements(BLOCK as u64));
    let mut wave = Wave::new(2, SR);
    for i in 0..(SR * FILE_SECS) as usize {
        wave.push_frame(&tone_frame(i));
    }
    let wave = Arc::new(wave);
    let mut nodes: Vec<Direct<MemorySource>> = (0..64)
        .map(|_| {
            let source = MemorySource::placed(Arc::clone(&wave), Beat::new(0.0), None);
            Direct::new(source, SampleRate(SR), BLOCK)
        })
        .collect();
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
    let mut block = || {
        let env = t.env(BLOCK, SR);
        let mut peak = 0.0f32;
        for d in &mut nodes {
            d.block_in(&env);
            peak = peak.max(d.output(0)[BLOCK - 1].abs());
        }
        t.advance(BLOCK as i64, SR);
        if t.beat().get() >= WRAP_BEATS {
            t.seek(Beat::new(0.0));
        }
        peak
    };
    let mut heard = 0.0f32;
    for _ in 0..256 {
        heard = heard.max(block());
    }
    assert!(heard > 0.01, "the sources are silent after warm-up");
    group.bench_function("memory-source/64", |b| b.iter(|| black_box(block())));
    group.finish();
}

criterion_group!(
    benches,
    bench_voices,
    bench_stretch,
    bench_pitch,
    bench_idle,
    bench_pool64,
    bench_pool512,
    bench_clips,
    bench_sources
);
criterion_main!(benches);
