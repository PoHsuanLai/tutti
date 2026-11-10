//! A beat-locked sampler voice follows the project's tempo map (doc 020,
//! stage 5): a real render of a `VoicePool` through the live engine over a
//! published `TempoMap`, the voice's output read back as the source position
//! it played.
//!
//! A placed voice is a clip: its window is in beats, so it enters on the
//! frame the playhead reaches its start beat and (with a duration) leaves on
//! the frame it reaches its end, and between them it plays its samples at
//! their own speed. So the oracle is the map's definition, written here and
//! never read back from `tutti-tempo` or another render. A step segment of
//! `bpm` takes `60 / bpm` seconds a beat; a ramp linear in time from `r0` to
//! `r1` beats per second over `D` beats lasts `T = 2 D / (r0 + r1)`, has
//! slope `a = (r1 - r0) / T` and reaches beat `b` at
//! `s = (sqrt(r0^2 + 2 a b) - r0) / a`. A clip starting at beat `S` begins at
//! frame `seconds(S) x rate`, and `f` frames on the voice is at source frame
//! `f - seconds(S) x rate`.
//!
//! The wave is a ramp (sample `i` is `i / 2^17`, exact in `f32`), so the
//! output says which source frame it is, and a re-seat, a jump or a fade at a
//! tempo change shows as a source frame that is not where the map puts it.
//!
//! **The bound for a ramp** (doc 020 section 4): the engine runs a ramp as
//! constant-tempo cells of 128 frames whose beats are the chord of the curve
//! between the cell's ends, so the playhead is within `|a| h^2 / 8` beats of
//! the curve inside a cell (`h` = 128 / rate): 8.3e-7 beats for 60 to 240 BPM
//! over 8 beats at 48 kHz, which at the ramp's slowest rate (1 beat/s) is
//! 0.04 frames. The voice's position is a time difference, so it is within
//! that of the map's, plus the ramp wave's `f32` rounding (0.02 frames at the
//! largest source frame read here).
//!
//! Each test names the mutation that fails it.

use std::sync::Arc;

use tutti_codec::Wave;
use tutti_engine::Engine;
use tutti_graph::{Editor, NodeKey, OutPort, Prepare, Source};
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
use tutti_tempo::{Shape, TempoMap, TempoPoint};
use tutti_transport::{MotionEvent, Transport};
use tutti_types::{Beat, BeatDuration, Bpm, ChannelLayout, InterleavedMut, SampleRate, Samples};

const SR: f64 = 48_000.0;
const MAX_BLOCK: usize = 4_096;
/// The wave is `i / SCALE` at frame `i`.
const SCALE: f32 = 131_072.0;
/// The most a ramp's position sits from the map's, in frames (see the module
/// docs: 0.04 for the grid, 0.02 for the wave's `f32`, and some slack).
const E: f64 = 0.08;
const BLOCKS: [usize; 6] = [1, 64, 100, 512, 1_000, 4_096];

fn map(points: &[(f64, f64, Shape)]) -> Arc<TempoMap> {
    Arc::new(
        TempoMap::new(points.iter().map(|&(beat, bpm, shape)| TempoPoint {
            beat: Beat(beat),
            tempo: Bpm::of(bpm),
            shape,
        }))
        .expect("a map"),
    )
}

/// 120 BPM, then 60 from beat 2.3 (frame 55 200: in the middle of the cell
/// 431 of the grid, so the step is not on a cell edge), then 180 from 4.
fn steps() -> Arc<TempoMap> {
    map(&[
        (0.0, 120.0, Shape::Step),
        (2.3, 60.0, Shape::Step),
        (4.0, 180.0, Shape::Step),
    ])
}

/// The seconds of beat `b` of `steps()`: each segment takes `60 / bpm` a beat.
fn step_seconds(b: f64) -> f64 {
    let seg = |from: f64, to: f64, bpm: f64| (b.min(to) - from).max(0.0) * 60.0 / bpm;
    seg(0.0, 2.3, 120.0) + seg(2.3, 4.0, 60.0) + seg(4.0, f64::INFINITY, 180.0)
}

/// 60 to 240 BPM over 8 beats, linear in time, then 240.
fn ramp() -> Arc<TempoMap> {
    map(&[(0.0, 60.0, Shape::RampTime), (8.0, 240.0, Shape::Step)])
}

/// The seconds of beat `b` of `ramp()`: `r0` = 1, `r1` = 4 beats/s, `D` = 8.
fn ramp_seconds(b: f64) -> f64 {
    const R0: f64 = 1.0;
    const T: f64 = 2.0 * 8.0 / (1.0 + 4.0);
    const A: f64 = (4.0 - R0) / T;
    if b <= 8.0 {
        ((R0 * R0 + 2.0 * A * b).sqrt() - R0) / A
    } else {
        T + (b - 8.0) * 60.0 / 240.0
    }
}

/// The first frame at or after `x`, with the 1e-6 of slack the engine's rule
/// has for a rate's rounding.
fn frame_after(x: f64) -> usize {
    (x - 1e-6).ceil().max(0.0) as usize
}

/// One placed voice (from `start`, for `duration` beats or the whole wave)
/// played from beat 0 over `map` for `frames`, in blocks of `block`, through
/// a pool bound to the map; the left channel.
fn render(
    map: &Arc<TempoMap>,
    voice: Option<(f64, Option<f64>)>,
    frames: usize,
    block: usize,
) -> Vec<f32> {
    let transport = Transport::new(SampleRate::new(SR));
    let mut wave = Wave::new(1, SampleRate::new(SR));
    for i in 0..4 * SR as usize {
        wave.push_frame(&[i as f32 / SCALE]);
    }
    let wave = Arc::new(wave);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    match voice {
        Some((start, duration)) => {
            let source = MemorySource::placed(wave, Beat(start), duration.map(BeatDuration::new))
                .expect("a placeable wave");
            let mut pool = VoicePool::new().with_tempo(transport.settings.tempo_reader());
            pool.insert_voice(
                SlotId(1),
                Voice {
                    source: VoiceSource::Memory(source),
                    play: Playback::default(),
                },
            );
            ed.insert(NodeKey(1), "voice", pool);
        }
        // Free-running: a node of its own, from its first frame, at its own
        // speed, on no window.
        None => {
            let source = MemorySource::new(wave).expect("a placeable wave");
            source.trigger();
            ed.insert(NodeKey(1), "voice", source);
        }
    }
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        Source::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings.set_tempo_map(Arc::clone(map));
    transport.motion.try_send(MotionEvent::Play).expect("room");
    let mut left = Vec::with_capacity(frames + block);
    let mut buf = vec![0.0f32; block * 2];
    while left.len() < frames {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        left.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
    }
    left.truncate(frames);
    left
}

/// The source frame the output says it played, when it played one.
fn played(sample: f32) -> f64 {
    f64::from(sample) * f64::from(SCALE)
}

/// **A step map: a clip is where the map puts it, exactly.** 120 BPM, 60 from
/// beat 2.3 (mid-cell) and 180 from beat 4; clips from beat 0 (through every
/// step), from beat 1 (through two), from beat 3 (inside the 60 segment, then
/// the last step), and one from beat 1 for 2.5 beats (ends in the 60
/// segment). Each enters on the first frame at or after `seconds(S) x rate`,
/// plays its source frames one a frame without a seam at either step (a step
/// is exact: the engine seats the segment on the map's beat), leaves on the
/// first frame at or after `seconds(S + d) x rate`, and is silent outside.
/// At every block size.
///
/// Mutation (run): `Gate::origin` taking `None` for the clock (the beat
/// offset over the run's own tempo, the voice as it was before the map was
/// read) -> a clip is re-seated at the tempo of each segment: at the step to
/// 60 the source frame jumps from 55 199 to 110 400 (a 4 s clip played
/// at the tempo of the moment, not from its start) -> fails. `BlockClock::
/// seconds` measuring from the block's first beat instead of the window's
/// start -> every block restarts the clip -> fails.
#[test]
fn a_step_map_puts_a_clips_frames_where_the_map_says() {
    let m = steps();
    let frames = 3 * 48_000;
    for (start, duration) in [(0.0, None), (1.0, None), (3.0, None), (1.0, Some(2.5))] {
        let enter = frame_after(step_seconds(start) * SR);
        let leave = duration.map_or(frames, |d| {
            frame_after(step_seconds(start + d) * SR).min(frames)
        });
        assert!(enter < leave && leave <= frames, "clip {start}");
        for block in BLOCKS {
            let out = render(&m, Some((start, duration)), frames, block);
            for (f, &got) in out.iter().enumerate() {
                let what = format!("clip {start} {duration:?} block {block} frame {f}");
                if (enter..leave).contains(&f) {
                    let want = (f - enter) as f32 / SCALE;
                    assert_eq!(got.to_bits(), want.to_bits(), "{what}");
                } else {
                    assert_eq!(got, 0.0, "{what}: silent outside the window");
                }
            }
        }
    }
}

/// **A ramp: the clip is within the doc 020 section 4 bound of the map's
/// frames, and a cell edge is no seam.** A clip from beat 0 and one from
/// beat 2 (mid-ramp, so it enters at a fractional frame: its first frame is
/// the fraction past the start) over the 60 to 240 ramp: at every frame the
/// voice is at source frame `f - seconds(S) x rate` within 0.08 frames, and
/// it advances one source frame a frame within 0.1 at every frame, in
/// particular at each of the ramp's cell edges (a re-seat at the cell's tempo
/// is a jump of frames; a wrap fade or a declick is a level step). The
/// bound is reached somewhere (some frame is more than 0.001 from the exact
/// position) or the test would pass on a clock that is not the curve.
///
/// Mutation (run): `Gate::origin` taking `None` for the clock -> the position
/// is re-seated at each cell's tempo, jumping by frames at each of the ramp's
/// cell edges (the first one, 128 frames in, by 0.5 s of source at 1.00 vs
/// 1.003 beats/s: a few hundredths of a frame at the first edge, growing to
/// thousands by beat 2) -> fails.
#[test]
fn a_ramp_puts_a_clips_frames_within_the_grid_bound_of_the_maps() {
    let m = ramp();
    let frames = 2 * 48_000;
    let mut off = 0.0f64;
    for start in [0.0, 2.0] {
        let x_start = ramp_seconds(start) * SR;
        let enter = frame_after(x_start);
        for block in [64, 100, 4_096] {
            let out = render(&m, Some((start, None)), frames, block);
            assert!(out[..enter].iter().all(|&s| s == 0.0), "clip {start}");
            for f in enter..frames {
                let want = f as f64 - x_start;
                let err = (played(out[f]) - want).abs();
                off = off.max(err);
                assert!(
                    err <= E,
                    "clip {start} block {block} frame {f}: source frame {}, the map says {want}",
                    played(out[f])
                );
                if f > enter {
                    let step = played(out[f]) - played(out[f - 1]);
                    assert!(
                        (step - 1.0).abs() <= 0.1,
                        "clip {start} block {block} frame {f}: a step of {step} frames"
                    );
                }
            }
        }
    }
    assert!(off > 0.001, "the bound is reached: {off}");
}

/// **A constant map is what it was.** A clip from beat 1 at 120 BPM through a
/// pool bound to a constant map is at source frame `f - 24 000` exactly from
/// frame 24 000, at every block size: the voice reads a constant map the way
/// it reads no map (the beats over the tempo, not the map's seconds). That
/// the bits are the old render's is held by the golden digests and
/// `block_length_invariance`, which this change leaves untouched; this case
/// pins the figure through a bound reader.
///
/// Mutation (run): `BlockClock::seconds` returning 0 for a constant map (the
/// `_` arm) -> the clip is at source frame 0 for ever -> fails (this case
/// alone). Taking the map's seconds for a constant map too (the
/// `constant_tempo().is_none()` guard removed) is not caught: the figures
/// agree exactly here, so only the digests hold that the bits do.
#[test]
fn a_constant_map_keeps_the_clip_where_it_was() {
    let m = map(&[(0.0, 120.0, Shape::Step)]);
    let frames = 48_000;
    for block in BLOCKS {
        let out = render(&m, Some((1.0, None)), frames, block);
        for (f, &got) in out.iter().enumerate() {
            let want = f.checked_sub(24_000).map_or(0.0, |i| i as f32 / SCALE);
            assert_eq!(got.to_bits(), want.to_bits(), "block {block} frame {f}");
        }
    }
}

/// **A time-locked voice ignores the map.** A free-running source (no
/// window, from its first frame at its own speed) is at source frame `f` at
/// every frame across the steps and the ramp's cells, whatever the tempo.
///
/// Mutation: none a render can show. A free-running source reads no transport
/// and no map, so the property holds by construction and this case pins it
/// against a change that gives an unplaced source a window on the map. (A
/// free-running source given a window at beat 0 plays the same figures as
/// the clip of the first case, which is the same answer on this map: the
/// two differ only in where they start, and both start at frame 0 here.)
#[test]
fn a_time_locked_voice_ignores_the_tempo_map() {
    for m in [steps(), ramp()] {
        for block in [64, 1_000] {
            let out = render(&m, None, 3 * 48_000, block);
            for (f, &got) in out.iter().enumerate() {
                assert_eq!(got.to_bits(), (f as f32 / SCALE).to_bits(), "frame {f}");
            }
        }
    }
}
