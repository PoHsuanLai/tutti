//! A tempo map of steps, followed by the live engine and by an offline render
//! (doc 020, stage 3).
//!
//! The oracle is arithmetic written here, apart from the walker: a point's
//! second is the sum of the segments before it, its frame is the first at or
//! after that second, and a frame in a segment is on the segment's line
//! (`beat + (second - start) x tempo / 60`). Nothing is compared against
//! another render except where the property *is* an equality of two renders
//! (block-size independence; live equals offline), and those are also held to
//! the arithmetic.
//!
//! Each test names the mutation that fails it.

use std::sync::{Arc, Mutex};
use tutti_types::MaxBlock;

use tutti_engine::Engine;
use tutti_graph::NodeKey;
use tutti_graph::{Cx, Editor, Executor, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_graph::{OutPort, Source};
use tutti_tempo::{Shape as TempoShape, TempoMap, TempoPoint};
use tutti_transport::{
    FadeOut, LoopEdit, MotionEvent, OfflineTimeline, OfflineTimelineConfig, Then, Transport,
};
use tutti_types::{
    At, Beat, Bpm, ChannelLayout, InterleavedMut, LoopRange, SampleRate, Samples, Tail,
    first_frame_at_or_after,
};

const SR: f64 = 48_000.0;

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
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(Tail::Unbounded)
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
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), MaxBlock::of(Samples(512))));
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

fn steps(points: &[(f64, f64)]) -> Arc<TempoMap> {
    Arc::new(
        TempoMap::new(points.iter().map(|&(beat, bpm)| TempoPoint {
            beat: Beat(beat),
            tempo: Bpm::of(bpm),
            shape: TempoShape::Step,
        }))
        .expect("a map"),
    )
}

/// The beat on `frame`, by the arithmetic of the map: the segment whose first
/// frame is the last at or before `frame`, and its line.
fn oracle(points: &[(f64, f64)], frame: u64) -> f64 {
    let mut sec = 0.0;
    let mut line = None;
    for (i, &(beat, bpm)) in points.iter().enumerate() {
        let first = first_frame_at_or_after(sec * SR).max(0) as u64;
        if first <= frame {
            line = Some((beat, bpm, sec));
        }
        if let Some(&(next, _)) = points.get(i + 1) {
            sec += (next - beat) * 60.0 / bpm;
        }
    }
    let (beat, bpm, start) = line.expect("the first point is at frame 0");
    beat + (frame as f64 / SR - start) * bpm / 60.0
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

fn assert_on_the_line(points: &[(f64, f64)], beats: &[f64], from: u64, what: &str) {
    for (i, &b) in beats.iter().enumerate() {
        let want = oracle(points, from + i as u64);
        assert!(
            (b - want).abs() < 1e-9,
            "{what}: frame {} is beat {b}, the map says {want}",
            from + i as u64
        );
    }
}

/// A step from 120 to 60 BPM at beat 4, at 48 kHz: beats 0 to 4 are 24 000
/// frames each, and beat 5 is on frame 4 x 24 000 + 48 000 = 144 000. The step
/// frame carries beat 4 to the bit.
///
/// Mutation (run): the walk never taking a cell's tempo (`TempoWalk::sync`
/// returning early) -> the beat keeps running at 120 and reaches 5 on frame
/// 120 000 -> fails.
#[test]
fn a_step_from_120_to_60_puts_beat_5_on_frame_144000() {
    let points = [(0.0, 120.0), (4.0, 60.0)];
    let seen = live(&steps(&points), 0.0, &[512], 160_000);
    assert_eq!(seen.beats.len(), 160_000);
    assert_eq!(seen.beats[96_000], 4.0, "the step frame, to the bit");
    assert_eq!(
        seen.beats.iter().position(|&b| b >= 5.0 - 1e-9),
        Some(4 * 24_000 + 48_000)
    );
    assert_on_the_line(&points, &seen.beats, 0, "120 to 60");
}

/// The same map at block sizes 1, 64, 100, 512, an odd 1000 (two graph blocks
/// per device block) and a ragged pattern: the beats of every frame are the
/// same to the bit, and on the line.
///
/// Mutation (run): roll a whole `advance` as one chunk (`chunk = frames -
/// rolled` in `TempoWalk::roll`, no cell edge) -> a step is taken at the end
/// of the call it falls in, so the beats depend on the block size -> fails.
#[test]
fn the_beats_do_not_depend_on_the_block_size() {
    let points = [(0.0, 120.0), (1.0, 60.0), (2.0, 180.0), (2.5, 90.0)];
    let map = steps(&points);
    let total = 60_000;
    let reference = live(&map, 0.0, &[512], total);
    assert_on_the_line(&points, &reference.beats, 0, "block 512");
    for blocks in [
        vec![1],
        vec![64],
        vec![100],
        vec![1000],
        vec![77, 513, 3, 1, 300],
    ] {
        let seen = live(&map, 0.0, &blocks, total);
        assert_eq!(seen.beats.len(), total);
        for (i, (a, b)) in seen.beats.iter().zip(&reference.beats).enumerate() {
            assert_eq!(a.to_bits(), b.to_bits(), "blocks {blocks:?}, frame {i}");
        }
    }
}

/// 41 points 0.001 beat apart (24 frames at 120 BPM): a 512-frame block holds
/// about 21 steps. The engine cuts it where the ninth change would be, no
/// block carries more than 8, and the render is the one a block of 1 gives
/// (which has no change inside it) and the one the map's arithmetic gives.
///
/// Mutation (run): `limit` never set in `TempoWalk::ahead` (the changes past
/// the 8th dropped) -> the beats after the 8th step in a block stay at the old
/// tempo -> fails.
#[test]
fn a_block_crossing_more_than_8_changes_is_split_and_exact() {
    let points: Vec<(f64, f64)> = (0..41)
        .map(|i| (f64::from(i) * 0.001, 120.0 + f64::from(i)))
        .collect();
    let map = steps(&points);
    let total = 2_048;
    let split = live(&map, 0.0, &[512], total);
    assert_eq!(split.beats.len(), total);
    assert_on_the_line(&points, &split.beats, 0, "split");
    assert_eq!(
        split.blocks.iter().map(|b| b.0).sum::<usize>(),
        total,
        "every frame rendered once"
    );
    assert!(
        split.blocks.iter().any(|&(len, _)| len < 512),
        "a 512-frame block was cut: {:?}",
        split.blocks
    );
    assert_eq!(
        split.blocks.iter().map(|b| b.1).max(),
        Some(tutti_graph::MAX_TRANSPORT_CHANGES),
        "and the cut blocks are full"
    );
    let ones = live(&map, 0.0, &[1], total);
    assert!(ones.blocks.iter().all(|b| b.1 == 0));
    for (i, (a, b)) in split.beats.iter().zip(&ones.beats).enumerate() {
        assert_eq!(a.to_bits(), b.to_bits(), "frame {i}");
    }
}

/// An offline render over the same map is the live render, frame for frame,
/// to the bit: both are the one walker. The offline timeline cuts its own
/// blocks where more than 8 changes fall (`OfflineTimeline::render_graph`).
///
/// Mutation (run): `OfflineTimeline::block` returning the block whole
/// (ignoring `limit`) -> the changes past the 8th are lost -> fails; the
/// offline `advance` rolling with the closed form instead of the walk -> the
/// playhead ignores the steps -> fails.
#[test]
fn an_offline_render_is_the_live_render_with_a_step_map() {
    let mut points: Vec<(f64, f64)> = (0..41)
        .map(|i| (f64::from(i) * 0.001, 120.0 + f64::from(i)))
        .collect();
    points.extend([(1.0, 75.0), (1.5, 200.0)]);
    let map = steps(&points);
    let total = 40_000;
    let live = live(&map, 0.0, &[512, 300, 77], total);

    let log = Log::default();
    let (_ed, mut exec) = graph(&log);
    let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo_map: Arc::clone(&map),
        sample_rate: SampleRate(SR),
        loop_range: None,
    });
    let mut out = vec![0.0f32; 512];
    let mut done = 0;
    for &n in [512usize, 300, 77].iter().cycle() {
        if done >= total {
            break;
        }
        let n = n.min(total - done);
        timeline.render_graph(&mut exec, n, &[], &mut [&mut out[..n]]);
        done += n;
    }
    let offline = log.lock().expect("log");
    assert_eq!(offline.beats.len(), total);
    assert!(offline.blocks.iter().any(|&(len, _)| len < 77));
    assert_on_the_line(&points, &offline.beats, 0, "offline");
    for (i, (a, b)) in offline.beats.iter().zip(&live.beats).enumerate() {
        assert_eq!(a.to_bits(), b.to_bits(), "frame {i}");
    }
    // The timeline stands where the render ended.
    let end = oracle(&points, total as u64);
    assert!((timeline.beat().get() - end).abs() < 1e-9);
}

/// A seek lands on its exact beat and the tempo of the step it lands in: back
/// to 3.9 (before the step at 4) the beat runs at 120 for 2 400 frames and then
/// at 60; to 4.5 (inside the step) and to 4.0 (on it) at 60 at once.
///
/// Mutation (run): `apply_pending_seek` seating the clock without the walk
/// (`self.clock.seat(target)` for a map) -> after the seek back the map frame
/// is still past the step, the tempo stays 60 -> fails.
#[test]
fn a_seek_seats_at_the_exact_beat_in_its_step() {
    let points = [(0.0, 120.0), (4.0, 60.0)];
    let transport = Transport::new(SampleRate::new(SR));
    let log = Log::default();
    let (mut ed, exec) = graph(&log);
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings.set_tempo_map(steps(&points));
    let m = &transport.motion;
    m.try_send(MotionEvent::play_from(Beat(0.0))).expect("room");
    render(&engine, 1_000);
    let locate = |beat: f64| MotionEvent::Locate {
        beat: Beat(beat),
        fade: FadeOut::Immediate,
        then: Then::Keep,
    };
    let mut at = 1_000;
    let mut check = |beat: f64, frames: usize, want: &dyn Fn(usize) -> f64| {
        m.try_send(locate(beat)).expect("room");
        render(&engine, frames);
        let seen = log.lock().expect("log");
        for n in 0..frames {
            let b = seen.beats[at + n];
            assert!(
                (b - want(n)).abs() < 1e-9,
                "after a seek to {beat}, frame {n}: {b}, expected {}",
                want(n)
            );
        }
        at += frames;
    };
    check(3.9, 4_000, &|n| {
        if n < 2_400 {
            3.9 + n as f64 / 24_000.0
        } else {
            4.0 + (n - 2_400) as f64 / 48_000.0
        }
    });
    check(4.5, 1_000, &|n| 4.5 + n as f64 / 48_000.0);
    check(4.0, 1_000, &|n| 4.0 + n as f64 / 48_000.0);
}

/// A loop over a step: [3, 5) with the step at 4 from 120 to 60, started at
/// 3.5. The wrap lands on the frame the beat reaches 5 (frame 60 000), at
/// beat 3 and at the tempo there (120), and every pass after is 72 000 frames.
///
/// Mutation (run): `TempoWalk::roll` skipping `place` after a wrap -> the
/// playhead goes on at 60 after the wrap -> fails.
#[test]
fn a_loop_wrap_takes_the_tempo_where_it_lands() {
    let points = [(0.0, 120.0), (4.0, 60.0)];
    let transport = Transport::new(SampleRate::new(SR));
    let log = Log::default();
    let (mut ed, exec) = graph(&log);
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings.set_tempo_map(steps(&points));
    transport
        .motion
        .schedule(
            At::NextBlock,
            LoopEdit::over(LoopRange::new(Beat::new(3.0), Beat::new(5.0)).expect("ordered")),
        )
        .expect("room");
    transport
        .motion
        .try_send(MotionEvent::play_from(Beat(3.5)))
        .expect("room");
    let total = 150_000;
    let mut done = 0;
    while done < total {
        let n = 512.min(total - done);
        render(&engine, n);
        done += n;
    }
    let seen = log.lock().expect("log");
    assert_eq!(seen.beats.len(), total);
    for (n, &b) in seen.beats.iter().enumerate() {
        let want = if n < 12_000 {
            3.5 + n as f64 / 24_000.0
        } else if n < 60_000 {
            4.0 + (n - 12_000) as f64 / 48_000.0
        } else {
            let m = (n - 60_000) % 72_000;
            if m < 24_000 {
                3.0 + m as f64 / 24_000.0
            } else {
                4.0 + (m - 24_000) as f64 / 48_000.0
            }
        };
        assert!((b - want).abs() < 1e-9, "frame {n}: {b}, expected {want}");
    }
}
