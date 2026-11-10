//! A tempo map with ramps, followed by the live engine and by an offline
//! render (doc 020, stage 4).
//!
//! The oracle is arithmetic written here, apart from the walker and from
//! `tutti-tempo`'s queries. A ramp linear in time from `r0` to `r1` beats per
//! second over `D` beats lasts `T = 2 D / (r0 + r1)`, has slope
//! `a = (r1 - r0) / T` and is at beat `B(s) = r0 s + a s^2 / 2` after `s`
//! seconds. A cell of 128 frames is a constant tempo whose beats are the chord
//! of `B` between its ends (doc 020, section 2.2). Where a test asks whether
//! the playhead is on the curve, the exact time of a beat is the integral
//! `t(b) = integral of 60 / bpm(b) db`, integrated numerically (Simpson) or in
//! the closed form of a ramp linear in beats, never read back from
//! `tutti-tempo`. Nothing is compared against another render except where the
//! property *is* an equality of two renders (block-size independence; live
//! equals offline), and those are also held to the arithmetic.
//!
//! Each test names the mutation that fails it.

use std::sync::{Arc, Mutex};

use tutti_engine::Engine;
use tutti_graph::{
    Cx, Editor, Executor, Io, Node, NodeKey, OutPort, Prepare, Shape, Source, Status, Unforkable,
};
use tutti_tempo::{Shape as TempoShape, TempoMap, TempoPoint};
use tutti_transport::{
    FadeOut, LoopEdit, MotionEvent, OfflineTimeline, OfflineTimelineConfig, Then, Transport,
};
use tutti_types::{
    At, Beat, Bpm, ChannelLayout, InterleavedMut, LoopRange, SampleRate, Samples, Tail,
    first_frame_at_or_after,
};

const SR: f64 = 48_000.0;
/// The cell grid of the engine (doc 020, section 10).
const GRID: u64 = 128;
/// The largest block the graph takes, so that a device block of 1 024 or more
/// reaches the graph whole and the 8-change cut is the engine's, not
/// `MaxBlock`'s.
const MAX_BLOCK: usize = 4_096;

/// What the node saw: the beat of every frame of every block, and each block's
/// length and number of transport changes.
#[derive(Default)]
struct Seen {
    beats: Vec<f64>,
    blocks: Vec<(usize, usize)>,
}

type Log = Arc<Mutex<Seen>>;

/// Logs the beat `Env::for_each_beat` gives every frame.
struct BeatLog(Log);

impl Node for BeatLog {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.0.lock().expect("log");
        cx.env.for_each_beat(|_, beat| seen.beats.push(beat.get()));
        seen.blocks
            .push((cx.env.block_len.get(), cx.env.changes.len()));
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn graph(log: &Log) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    ed.insert(NodeKey(1), "beats", Unforkable(BeatLog(Arc::clone(log))));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        })]);
    ed.commit().expect("commits");
    (ed, exec)
}

/// `(beat, bpm, shape of the segment that starts there)`; the last is a step.
fn tempo_map(points: &[(f64, f64, TempoShape)]) -> Arc<TempoMap> {
    Arc::new(
        TempoMap::new(points.iter().map(|&(beat, bpm, shape)| TempoPoint {
            beat: Beat(beat),
            tempo: Bpm::of(bpm),
            shape,
        }))
        .expect("a map"),
    )
}

/// 120 to 60 BPM over 4 beats, linear in time, then 60 BPM.
fn down() -> Arc<TempoMap> {
    tempo_map(&[
        (0.0, 120.0, TempoShape::RampTime),
        (4.0, 60.0, TempoShape::Step),
    ])
}

// The ramp of `down()`: r0 = 2 beats/s, r1 = 1, D = 4, so T = 8/3 s, which is
// 128 000 frames at 48 kHz, 1 000 cells exactly.
const R0: f64 = 2.0;
const R1: f64 = 1.0;
const T: f64 = 2.0 * 4.0 / (R0 + R1);
const A: f64 = (R1 - R0) / T;

/// The beat `s` seconds into `down()`'s ramp.
fn beat_at(s: f64) -> f64 {
    R0 * s + A * s * s / 2.0
}

/// The seconds of beat `b` in `down()`'s ramp: `r(b)^2 = r0^2 + 2 a b` and
/// `s = (r - r0) / a`.
fn seconds_of(b: f64) -> f64 {
    ((R0 * R0 + 2.0 * A * b).sqrt() - R0) / A
}

/// The beat on `frame` of the ramp of `down()` at `rate`, by its cells: the
/// frame is in cell `k`, which is the chord of `B` between its ends.
fn on_the_cells(frame: u64) -> f64 {
    let k = frame / GRID;
    let (s0, s1) = ((k * GRID) as f64 / SR, ((k + 1) * GRID) as f64 / SR);
    let (b0, b1) = (beat_at(s0), beat_at(s1));
    b0 + (frame - k * GRID) as f64 * (b1 - b0) / GRID as f64
}

/// Beats the playhead is allowed to be from the curve inside a cell: the
/// chord of a parabola with second derivative `a` over `h` seconds is
/// `|a| h^2 / 8` off at its middle, and no more anywhere (doc 020, section 4).
fn chord_bound() -> f64 {
    A.abs() * (GRID as f64 / SR).powi(2) / 8.0
}

fn render(engine: &Engine, frames: usize) {
    let mut buf = vec![0.0f32; frames];
    engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO));
}

/// The live engine over `map` from beat `start`, rendered in `blocks` (a
/// pattern repeated until `total` frames).
fn live(map: &Arc<TempoMap>, start: f64, blocks: &[usize], total: usize) -> Seen {
    let transport = Transport::new(SampleRate::new(SR));
    let log = Log::default();
    let (mut ed, exec) = graph(&log);
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings.set_tempo_map(Arc::clone(map));
    transport
        .motion
        .try_send(MotionEvent::play_from(Beat(start)))
        .expect("room");
    let mut done = 0;
    for &n in blocks.iter().cycle() {
        if done >= total {
            break;
        }
        let n = n.min(total - done);
        render(&engine, n);
        done += n;
    }
    drop(engine);
    Arc::try_unwrap(log)
        .ok()
        .expect("the engine is gone")
        .into_inner()
        .expect("log")
}

/// Simpson's rule for `integral of f` over `[0, x]` in `n` panels (even).
fn simpson(f: impl Fn(f64) -> f64, x: f64, n: usize) -> f64 {
    let h = x / n as f64;
    let mut sum = f(0.0) + f(x);
    for i in 1..n {
        sum += f(i as f64 * h) * if i % 2 == 1 { 4.0 } else { 2.0 };
    }
    sum * h / 3.0
}

/// The exact seconds to beat `b` of `down()`: `integral of 60 / bpm db`, with
/// `bpm(b) = 60 sqrt(r0^2 + 2 a b)` (tempo linear in time), integrated.
fn integral_seconds(b: f64) -> f64 {
    simpson(|x| 1.0 / (R0 * R0 + 2.0 * A * x).sqrt(), b, 2_000)
}

/// A linear ramp 120 to 60 BPM over 4 beats at 48 kHz. The ramp is 1 000 cells
/// and beat 4 is on frame 128 000: the sum, over the cells, of the frames each
/// cell's constant tempo takes to cover the beats the curve covers in it
/// (`delta / (chord / (60 x 48 000))`, with the chord `delta x 60 x 48 000 /
/// 128`, 128 each). The playhead stands on beat 4 on that frame, and on the
/// cells' lines before it.
///
/// Mutation (run): `TempoWalk::sync` taking the instantaneous tempo at the
/// cell's start (`map.tempo_at_frame`) instead of the chord -> every cell
/// covers too many beats and beat 4 is reached early -> fails; a ramp the walk
/// never follows (`stepped` left false in `tempo_for_block`) -> the clock stays
/// at 120 and beat 4 is on frame 96 000 -> fails; `TransportSettings::
/// set_tempo_map` ignoring a map with a ramp -> fails.
#[test]
fn a_ramp_from_120_to_60_over_4_beats_reaches_beat_4_on_frame_128000() {
    let seen = live(&down(), 0.0, &[512], 140_000);
    assert_eq!(seen.beats.len(), 140_000);

    let cells = 128_000 / GRID;
    let frames: f64 = (0..cells)
        .map(|k| {
            let (s0, s1) = ((k * GRID) as f64 / SR, ((k + 1) * GRID) as f64 / SR);
            let covered = beat_at(s1) - beat_at(s0);
            let chord_bpm = covered * 60.0 * SR / GRID as f64;
            covered / (chord_bpm / (60.0 * SR))
        })
        .sum();
    assert_eq!(frames.round() as usize, 128_000);

    let first = seen.beats.iter().position(|&b| b >= 4.0 - 1e-9);
    assert_eq!(first, Some(frames.round() as usize));
    assert!(
        (seen.beats[128_000] - 4.0).abs() < 1e-12,
        "on the map's beat"
    );
    // After the ramp: 60 BPM, a beat a second.
    assert!((seen.beats[128_000 + 10_000] - (4.0 + 10_000.0 / 48_000.0)).abs() < 1e-9);
    for (f, &b) in seen.beats.iter().take(128_000).enumerate() {
        let want = on_the_cells(f as u64);
        assert!(
            (b - want).abs() < 1e-10,
            "frame {f}: {b}, the cells say {want}"
        );
    }
}

/// The deviation from the exact integral is within the grid's bound: the
/// playhead's beat is within `|a| h^2 / 8` of the curve (`h` = a cell's
/// seconds), it is on the curve at every cell start, and the time of the
/// beat it shows, integrated as `integral of 60 / bpm db`, is within that
/// bound over the slowest rate (1 beat/s) of the frame it is on: 0.017 frames
/// here (the doc's figure for 60 to 240 BPM over 8 beats is 0.04). The bound
/// is reached (the error is not zero), so the test cannot pass on a render
/// that is the curve by another route.
///
/// Mutation (run): `CELL_GRID` = 512 -> the playhead is 16 times farther from
/// the curve (error ~ `grid^2`) -> the bound fails; `TempoWalk::sync` taking
/// the instantaneous tempo at the cell's start instead of the chord -> every
/// cell ends off the map's beat -> the cell-start equality fails. (Dropping
/// the re-seat on the map's beat does not fail it: a chord ends on the map's
/// beat by definition, so the re-seat only removes ulps; the seek test sees
/// it.)
#[test]
fn the_ramp_stays_within_the_grid_bound_of_the_exact_integral() {
    let seen = live(&down(), 0.0, &[512], 130_000);
    let bound = chord_bound();
    let mut worst = 0.0f64;
    for f in 0..128_000usize {
        let off = seen.beats[f] - beat_at(f as f64 / SR);
        worst = worst.max(off.abs());
        assert!(off.abs() <= bound + 1e-12, "frame {f}: {off} beats off");
        if (f as u64).is_multiple_of(GRID) {
            assert!(off.abs() < 1e-12, "cell start {f}: {off} beats off");
        }
    }
    assert!(
        worst > 0.9 * bound,
        "the bound is reached: {worst} of {bound}"
    );

    // Frames: the exact integral's time for the beat shown, against the frame.
    let frame_bound = bound / 1.0 * SR + 1e-6;
    assert!(frame_bound < 0.02, "{frame_bound}");
    for f in (0..128_000usize).step_by(7) {
        let t = integral_seconds(seen.beats[f]);
        let off = (t * SR - f as f64).abs();
        assert!(off <= frame_bound, "frame {f}: {off} frames off");
        if (f as u64).is_multiple_of(GRID) {
            assert!(off < 1e-5, "cell start {f}: {off} frames off the integral");
        }
    }
}

/// The same bound for a ramp linear in beats (120 to 60 BPM over 4 beats, so
/// `bpm(b) = 120 - 15 b` and `t(b) = 4 ln(120 / (120 - 15 b))`, which is
/// `T = 4 ln 2` s = 133 084.26 frames): at every cell start the playhead's
/// time is the exact one, and inside a cell it is within a frame's hundredth.
///
/// Mutation (run): `chord_cell` returning the segment's `bpm0` for a
/// `RampBeats` segment (a step) -> the playhead runs at 120 and is
/// 20 000 frames early at beat 4 -> fails; a ramp the walk never follows -> fails.
#[test]
fn a_ramp_linear_in_beats_is_on_the_integral_at_every_cell_start() {
    let map = tempo_map(&[
        (0.0, 120.0, TempoShape::RampBeats),
        (4.0, 60.0, TempoShape::Step),
    ]);
    let seen = live(&map, 0.0, &[512], 135_000);
    let t_of = |b: f64| 4.0 * (120.0 / (120.0 - 15.0 * b)).ln();
    let end = (t_of(4.0) * SR) as u64;
    assert_eq!(end, 133_084);
    for f in (0..end).step_by(5) {
        let t = t_of(seen.beats[f as usize]);
        let off = (t * SR - f as f64).abs();
        assert!(off < 0.03, "frame {f}: {off} frames off the integral");
        if f % GRID == 0 {
            assert!(off < 1e-5, "cell start {f}: {off} frames");
        }
    }
}

/// A ramp into a ramp into a step, the beats of every frame at block sizes 1,
/// 64, 100, 512, 1 000 (two cells' worth of changes more than one block holds
/// at 4 096) and a ragged pattern: the same to the bit, and the first ramp is
/// on the cells' lines.
///
/// Mutation (run): `TempoWalk::roll` rolling a whole `advance` as one chunk
/// (`chunk = frames - rolled`, no cell edge) -> a ramp cell is taken at the end
/// of the call it falls in, so the beats depend on the block size -> fails;
/// `chord_cell` starting the cell at `frame` instead of the grid multiple (in
/// `tutti-tempo`) -> cells depend on where a block began -> fails.
#[test]
fn the_beats_do_not_depend_on_the_block_size() {
    let map = tempo_map(&[
        (0.0, 120.0, TempoShape::RampTime),
        (4.0, 60.0, TempoShape::RampBeats),
        (6.0, 150.0, TempoShape::Step),
    ]);
    let total = 175_000;
    let reference = live(&map, 0.0, &[512], total);
    for (f, &b) in reference.beats.iter().take(128_000).enumerate() {
        assert!((b - on_the_cells(f as u64)).abs() < 1e-10, "frame {f}");
    }
    for blocks in [
        vec![1],
        vec![64],
        vec![100],
        vec![1_000],
        vec![4_096],
        vec![77, 513, 3, 1, 300, 2_049],
    ] {
        let seen = live(&map, 0.0, &blocks, total);
        assert_eq!(seen.beats.len(), total);
        for (i, (a, b)) in seen.beats.iter().zip(&reference.beats).enumerate() {
            assert_eq!(a.to_bits(), b.to_bits(), "blocks {blocks:?}, frame {i}");
        }
    }
}

/// A ramp makes a change every cell, so a 512-frame block holds 3 or 4 (a
/// block of 512 crosses at most 5 cell edges, and the first cell is the
/// block's own transport) and runs whole; a device block of 4 096 reaches the
/// graph whole and needs 31, so the engine cuts it: no block carries more than
/// 8 changes, the cut blocks are full, every frame is rendered once, and the
/// beats are the ones a block of 1 gives (no change inside it).
///
/// Mutation (run): `limit` never set in `TempoWalk::ahead` (the changes past
/// the 8th dropped) -> the beats after the 8th cell stay at its tempo -> fails.
#[test]
fn a_ramp_block_with_more_than_8_cells_is_split_and_exact() {
    let map = down();
    let total = 20_480;
    let small = live(&map, 0.0, &[512], total);
    let ramp_blocks: Vec<_> = small.blocks.iter().filter(|b| b.1 > 0).collect();
    assert!(
        ramp_blocks.iter().all(|b| (3..=5).contains(&b.1)),
        "{:?}",
        small.blocks
    );
    assert!(
        small.blocks.iter().all(|b| b.0 == 512),
        "whole blocks: {:?}",
        small
            .blocks
            .iter()
            .filter(|b| b.0 != 512)
            .collect::<Vec<_>>()
    );

    let big = live(&map, 0.0, &[4_096], total);
    assert_eq!(big.beats.len(), total);
    assert_eq!(big.blocks.iter().map(|b| b.0).sum::<usize>(), total);
    assert!(
        big.blocks
            .iter()
            .all(|b| b.1 <= tutti_graph::MAX_TRANSPORT_CHANGES)
    );
    assert!(big.blocks.iter().any(|&(len, _)| len < 4_096));
    assert_eq!(
        big.blocks.iter().map(|b| b.1).max(),
        Some(tutti_graph::MAX_TRANSPORT_CHANGES)
    );
    let ones = live(&map, 0.0, &[1], total);
    assert!(ones.blocks.iter().all(|b| b.1 == 0));
    for (f, ((a, b), c)) in big
        .beats
        .iter()
        .zip(&ones.beats)
        .zip(&small.beats)
        .enumerate()
    {
        assert_eq!(a.to_bits(), b.to_bits(), "frame {f}");
        assert_eq!(a.to_bits(), c.to_bits(), "frame {f}");
        assert!((a - on_the_cells(f as u64)).abs() < 1e-10, "frame {f}");
    }
}

/// An offline render over the same ramp is the live render, frame for frame,
/// to the bit: both are the one walker, and the offline timeline cuts its own
/// blocks where more than 8 changes fall.
///
/// Mutation (run): the offline `advance` rolling with the closed form instead
/// of the walk -> the playhead ignores the ramp -> fails.
#[test]
fn an_offline_render_is_the_live_render_with_a_ramp() {
    let map = tempo_map(&[
        (0.0, 120.0, TempoShape::RampTime),
        (4.0, 60.0, TempoShape::RampBeats),
        (6.0, 150.0, TempoShape::Step),
    ]);
    let total = 175_000;
    let pattern = [512usize, 300, 77, 2_048];
    let live = live(&map, 0.0, &pattern, total);

    let log = Log::default();
    let (_ed, mut exec) = graph(&log);
    let timeline = OfflineTimeline::with_tempo_map(
        &OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm::of(120.0),
            sample_rate: SampleRate(SR),
            loop_range: None,
        },
        Arc::clone(&map),
    );
    let mut out = vec![0.0f32; 2_048];
    let mut done = 0;
    for &n in pattern.iter().cycle() {
        if done >= total {
            break;
        }
        let n = n.min(total - done);
        timeline.render_graph(&mut exec, n, &[], &mut [&mut out[..n]]);
        done += n;
    }
    let offline = log.lock().expect("log");
    assert_eq!(offline.beats.len(), total);
    assert!(
        offline
            .blocks
            .iter()
            .all(|b| b.1 <= tutti_graph::MAX_TRANSPORT_CHANGES)
    );
    assert!(
        offline
            .blocks
            .iter()
            .any(|b| b.1 == tutti_graph::MAX_TRANSPORT_CHANGES)
    );
    for (f, (a, b)) in offline.beats.iter().zip(&live.beats).enumerate() {
        assert_eq!(a.to_bits(), b.to_bits(), "frame {f}");
        if f < 128_000 {
            assert!((a - on_the_cells(f as u64)).abs() < 1e-10, "frame {f}");
        }
    }
    // The timeline stands where the render ended.
    let last = *live.beats.last().expect("frames");
    assert!(
        (timeline.beat().get() - last - 2.0 / SR).abs() < 1e-5,
        "one frame of 150 BPM past the last beat shown"
    );
}

/// A seek into the middle of the ramp seats on the exact beat and takes the
/// tempo of the cell it lands in: to beat 2 (second `s(2)` = `(sqrt(r0^2 + 2 a
/// 2) - r0) / a`, the first frame at or after it) the beat on the seat frame is
/// 2.0 to the bit, the first frames run at the chord of the cell holding that
/// frame, and the first cell edge after the seat is on the map's own beat.
///
/// Mutation (run): `apply_pending_seek` seating the clock at the beat of the
/// map frame (`frame_at_or_after`) instead of the exact beat -> the first beat
/// is not 2.0 -> fails; the walk not placed after the seat (`self.walk.seat`
/// replaced by `clock.seat`) -> the clock keeps the tempo of the old position
/// -> fails.
#[test]
fn a_seek_into_the_middle_of_a_ramp_seats_on_the_exact_beat() {
    let transport = Transport::new(SampleRate::new(SR));
    let log = Log::default();
    let (mut ed, exec) = graph(&log);
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings.set_tempo_map(down());
    let m = &transport.motion;
    m.try_send(MotionEvent::play_from(Beat(0.0))).expect("room");
    render(&engine, 1_000);
    m.try_send(MotionEvent::Locate {
        beat: Beat(2.0),
        fade: FadeOut::Immediate,
        then: Then::Keep,
    })
    .expect("room");
    render(&engine, 6_000);
    let seen = log.lock().expect("log");
    let after = &seen.beats[1_000..];

    let f0 = first_frame_at_or_after(seconds_of(2.0) * SR) as u64;
    assert_eq!(after[0], 2.0, "the exact beat, to the bit");
    // The first cell's own chord: the slope until the cell's edge.
    let k = f0 / GRID;
    let (s0, s1) = ((k * GRID) as f64 / SR, ((k + 1) * GRID) as f64 / SR);
    let per_frame = (beat_at(s1) - beat_at(s0)) / GRID as f64;
    let edge = ((k + 1) * GRID - f0) as usize;
    for (n, &b) in after.iter().enumerate().take(edge) {
        let want = 2.0 + n as f64 * per_frame;
        assert!((b - want).abs() < 1e-10, "frame {n} of the first cell");
    }
    // The edge: the map's beat, not the line carried on.
    let map_beat = beat_at(s1);
    assert!(
        (after[edge] - map_beat).abs() < 1e-10,
        "{} vs {map_beat}",
        after[edge]
    );
    // Then the cells of the map, within the playhead's seat offset (under a
    // frame of beats) of the curve.
    let tolerance = 2.0 / SR + chord_bound();
    for (n, &b) in after.iter().enumerate().take(5_000) {
        let exact = beat_at((f0 + n as u64) as f64 / SR);
        assert!((b - exact).abs() <= tolerance, "frame {n}: {b} vs {exact}");
    }
    for g in (edge..5_000).filter(|g| (f0 + *g as u64).is_multiple_of(GRID)) {
        let exact = beat_at((f0 + g as u64) as f64 / SR);
        assert!(
            (after[g] - exact).abs() < 1e-10,
            "cell start after {g} frames"
        );
    }
}

/// A loop over beats 1 to 3 of the ramp, started at 1.5: the wrap is a seek
/// to the loop start. It lands on the frame the playhead reaches beat 3 (the
/// curve's, within the seat's frame: `F(b) = s(b) x 48 000`, the first pass
/// from 1.5 has `F(3) - F(1.5)` frames and every later one `F(3) - F(1)`, each
/// within 2), at beat 1.0 to the bit and at the cell's tempo there (1.80
/// beats/s, not the 1.32 of beat 3), and every pass after is the same.
///
/// Mutation (run): `TempoWalk::roll` skipping `place` after a wrap -> the
/// playhead goes on at the loop end's tempo and the slope after the wrap is
/// 1.32 -> fails.
#[test]
fn a_loop_wrap_inside_a_ramp_takes_the_cell_where_it_lands() {
    let transport = Transport::new(SampleRate::new(SR));
    let log = Log::default();
    let (mut ed, exec) = graph(&log);
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings.set_tempo_map(down());
    transport
        .motion
        .schedule(
            At::NextBlock,
            LoopEdit::over(LoopRange::new(Beat::new(1.0), Beat::new(3.0)).expect("ordered")),
        )
        .expect("room");
    transport
        .motion
        .try_send(MotionEvent::play_from(Beat(1.5)))
        .expect("room");
    let total = 320_000;
    let mut done = 0;
    while done < total {
        let n = 512.min(total - done);
        render(&engine, n);
        done += n;
    }
    let seen = log.lock().expect("log");
    assert_eq!(seen.beats.len(), total);
    let frame_of = |b: f64| seconds_of(b) * SR;
    let wraps: Vec<usize> = (1..total)
        .filter(|&n| seen.beats[n] < seen.beats[n - 1])
        .collect();
    assert!(wraps.len() >= 4, "{wraps:?}");
    let first = (frame_of(3.0) - frame_of(1.5)).round() as i64;
    assert!(
        (wraps[0] as i64 - first).abs() <= 2,
        "{} vs {first}",
        wraps[0]
    );
    let pass = (frame_of(3.0) - frame_of(1.0)).round() as i64;
    for w in wraps.windows(2) {
        assert!(((w[1] - w[0]) as i64 - pass).abs() <= 2, "{:?}", wraps);
    }
    let f1 = first_frame_at_or_after(frame_of(1.0)) as u64;
    // The wrap keeps the overshoot past the loop end (under a frame of beats
    // at the loop end's 1.32 beats/s), as `FrameClock::advance_to_wrap` does.
    let frame_of_end = 1.4 / SR;
    for &w in &wraps {
        let over = seen.beats[w] - 1.0;
        assert!(
            (0.0..frame_of_end).contains(&over),
            "wrap {w}: {over} past 1.0"
        );
        let rate_after = (seen.beats[w + 100] - seen.beats[w]) * SR / 100.0;
        assert!((rate_after - 1.80).abs() < 0.02, "{rate_after} beats/s");
        // Every pass is on the curve from beat 1 (the seat frame, and the
        // overshoot, each under a frame of beats).
        let len = wraps
            .iter()
            .find(|&&x| x > w)
            .map_or(1_000, |x| x - w)
            .min(60_000);
        for n in 0..len {
            let exact = beat_at((f1 + n as u64) as f64 / SR);
            assert!(
                (seen.beats[w + n] - exact).abs() <= 4.0 / SR + chord_bound(),
                "frame {n} of the pass at {w}"
            );
        }
    }
}
