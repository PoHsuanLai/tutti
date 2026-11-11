//! The MIDI clock follows the published `TempoMap` (doc 020, stage 5): a real
//! render of a `ClockNode` into a `MidiOutNode` through the live engine, the
//! events read out of the out node's ring with their frame on the graph's
//! clock.
//!
//! The oracle is the map's definition, written here and never read back from
//! `tutti-tempo` or another render. A step segment of `bpm` takes `60 / bpm`
//! seconds a beat. A ramp linear in time from `r0` to `r1` beats per second
//! over `D` beats lasts `T = 2 D / (r0 + r1)`, has slope `a = (r1 - r0) / T`,
//! is at beat `r0 s + a s^2 / 2` after `s` seconds and reaches beat `b` at
//! `s = (sqrt(r0^2 + 2 a b) - r0) / a`. Timing Clock `k` is due at beat
//! `k / 24`, so on the first frame at or after `seconds(k / 24) x rate`.
//!
//! **The bound for a ramp** (doc 020 section 4): the engine runs a ramp as
//! constant-tempo cells of 128 frames whose beats are the chord of the curve
//! between the cell's ends, so the playhead is on the curve at every cell
//! start and within `|a| h^2 / 8` beats of it inside a cell (`h` = 128 /
//! rate). For 60 to 240 BPM over 8 beats at 48 kHz, `a` = 0.9375 beats/s^2 and
//! that is 8.3e-7 beats, which at the slowest rate of the ramp (1 beat/s) is
//! `E` = 0.04 frames. A tick is the first frame the playhead reaches its beat,
//! so it is `ceil(x + e)` with `|e| <= E` for the exact frame position `x`:
//! within one frame of `ceil(x)`, and equal to it unless `ceil(x) - x` is
//! within `E` of 0 or of 1.
//!
//! Each test names the mutation that fails it.

use std::sync::Arc;

use tutti_engine::Engine;
use tutti_graph::{Editor, EventEdge, EventIn, EventOut, Executor, NodeKey, Prepare};
use tutti_midi_runtime::{ClockNode, MidiOutControls, MidiOutNode};
use tutti_tempo::{Shape, TempoMap, TempoPoint};
use tutti_transport::{FadeOut, MotionEvent, Then, Transport};
use tutti_types::{Beat, Bpm, ChannelLayout, InterleavedMut, SampleRate, Samples};
use tutti_ump::{Message, System, U14, decode};

const SR: f64 = 48_000.0;
const TICKS_PER_BEAT: f64 = 24.0;
/// The most a tick may sit from `ceil(x)` on a ramp (see the module docs).
const E: f64 = 0.04;
const MAX_BLOCK: usize = 4_096;

/// A System message by its kind, whatever its data.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Clock,
    Start,
    Continue,
    SongPosition,
    TimeCode,
}

impl Kind {
    fn is(self, s: System) -> bool {
        matches!(
            (self, s),
            (Kind::Clock, System::TimingClock)
                | (Kind::Start, System::Start)
                | (Kind::Continue, System::Continue)
                | (Kind::SongPosition, System::SongPosition(_))
                | (Kind::TimeCode, System::TimeCode(_))
        )
    }
}

const CLOCK: Kind = Kind::Clock;
const START: Kind = Kind::Start;
const CONTINUE: Kind = Kind::Continue;
const SONG_POSITION: Kind = Kind::SongPosition;

/// One event out of the ring: the System message (a Song Position carries
/// its 14-bit payload) and its frame on the graph's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sent {
    system: System,
    frame: u64,
}

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

/// 120 BPM, then 90 from beat 7.7 (frame 184 800: in the middle of the cell
/// 1 443 of the grid, so the step is not on a cell edge), then 150 from 12.
fn steps() -> Arc<TempoMap> {
    map(&[
        (0.0, 120.0, Shape::Step),
        (7.7, 90.0, Shape::Step),
        (12.0, 150.0, Shape::Step),
    ])
}

/// 60 to 240 BPM over 8 beats, linear in time, then 240.
fn ramp() -> Arc<TempoMap> {
    map(&[(0.0, 60.0, Shape::RampTime), (8.0, 240.0, Shape::Step)])
}

/// The seconds of beat `b` of `steps()`: each segment takes `60 / bpm` a beat.
fn step_seconds(b: f64) -> f64 {
    let seg = |from: f64, to: f64, bpm: f64| (b.min(to) - from).max(0.0) * 60.0 / bpm;
    seg(0.0, 7.7, 120.0) + seg(7.7, 12.0, 90.0) + seg(12.0, f64::INFINITY, 150.0)
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

/// The exact frame position of beat `b`, `x` in the module docs.
fn frame_x(seconds: fn(f64) -> f64, b: f64) -> f64 {
    seconds(b) * SR
}

/// The first frame at or after `x`, with the 1e-6 of slack the engine's rule
/// has for a rate's rounding.
fn frame_after(x: f64) -> u64 {
    (x - 1e-6).ceil().max(0.0) as u64
}

struct Rig {
    engine: Engine,
    transport: Transport,
    out: MidiOutControls,
}

/// A clock (enabled, MTC per `mtc`) into an out node, on an engine over `map`.
fn rig(map: &Arc<TempoMap>, mtc: bool) -> Rig {
    let (mut ed, exec): (Editor, Executor) =
        Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    let master = ed.insert(NodeKey(1), "clock", ClockNode::new());
    master.set_enabled(true);
    master.set_send_mtc(mtc);
    let out = ed.insert(NodeKey(2), "out", MidiOutNode::new());
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(2),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 0,
        }),
    );
    ed.commit().expect("commits");
    let transport = Transport::new(SampleRate::new(SR));
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings.set_tempo_map(Arc::clone(map));
    Rig {
        engine,
        transport,
        out,
    }
}

impl Rig {
    fn send(&self, event: MotionEvent) {
        self.transport.motion.try_send(event).expect("room");
    }

    /// Render `frames` in blocks of `block`, draining the ring after each.
    fn render(&self, frames: usize, block: usize, sent: &mut Vec<Sent>) {
        let mut buf = vec![0.0f32; block];
        let mut ring = [(0_u32, tutti_ump::Ump([0; 4])); 256];
        let mut done = 0;
        while done < frames {
            let n = block.min(frames - done);
            self.engine
                .process(&mut InterleavedMut::new(&mut buf[..n], ChannelLayout::MONO));
            done += n;
            loop {
                let got = self.out.poll_into(&mut ring);
                sent.extend(ring[..got].iter().map(|&(frame, ump)| {
                    let Ok(Message::System(_, system)) = decode(ump) else {
                        panic!("the clock sends System messages: {ump:?}")
                    };
                    Sent {
                        system,
                        frame: u64::from(frame),
                    }
                }));
                if got < ring.len() {
                    break;
                }
            }
        }
        assert_eq!(self.out.dropped(), 0, "the ring held every event");
    }
}

/// A play from `start`, rendered for `frames` in `block`-sized blocks.
fn play(map: &Arc<TempoMap>, start: f64, frames: usize, block: usize) -> Vec<Sent> {
    let rig = rig(map, false);
    rig.send(MotionEvent::play_from(Beat(start)));
    let mut sent = Vec::new();
    rig.render(frames, block, &mut sent);
    sent
}

fn frames_of(sent: &[Sent], kind: Kind) -> Vec<u64> {
    sent.iter()
        .filter(|e| kind.is(e.system))
        .map(|e| e.frame)
        .collect()
}

/// **A step map: every Timing Clock is on the map's frame.** 120 BPM, 90 from
/// beat 7.7 (mid-cell) and 150 from beat 12: tick `k` is on exactly the first
/// frame at or after `seconds(k / 24) x 48 000`, piecewise linear (a step is
/// exact: the walker takes the segment's own tempo and re-seats on the map's
/// beat at the step). All 24 ticks of every beat are there, none twice.
///
/// Mutation (run): `ClockNode::process` ticking the block's first segment only
/// (`.take(1)` on `cx.env.segments()`) -> every tick after the first tempo
/// change of a block is placed at the old tempo or lost -> fails. A tick at
/// the block's first-segment tempo for the whole block is the same mutation.
#[test]
fn a_step_map_puts_every_tick_on_the_maps_frame() {
    let frames = 16 * 48_000; // 16 s: 24 + ... beats, past every step.
    let sent = play(&steps(), 0.0, frames, 512);
    let ticks = frames_of(&sent, CLOCK);
    let mut want = Vec::new();
    for k in 0.. {
        let f = frame_after(frame_x(step_seconds, f64::from(k) / TICKS_PER_BEAT));
        if f >= frames as u64 {
            break;
        }
        want.push(f);
    }
    assert!(want.len() > 400, "ticks across all three tempos");
    assert_eq!(ticks, want);
    // The tick spacing really is the three tempi: 1 000, 1 333.3, 800 frames.
    assert_eq!(ticks[1] - ticks[0], 1_000);
    assert_eq!(ticks[200] - ticks[199], 1_333);
    assert_eq!(ticks[400] - ticks[399], 800);
}

/// **A ramp: every tick within the doc 020 section 4 bound** (the module
/// docs: within one frame of `ceil(x)`, equal to it unless `ceil(x) - x` is
/// within 0.04 of 0 or 1), tick 0 and the ticks of the step after the ramp
/// exact. The ticks are not all exactly `ceil(x)` (the bound is reached), so
/// the test cannot pass on a clock that is the curve by another route; and
/// there are exactly 24 ticks to a beat across the ramp.
///
/// Mutation (run): `ClockNode::process` ticking only the first segment of a
/// block (`.take(1)` on `cx.env.segments()`) -> the later cells of a block
/// send nothing, ticks are lost -> fails (all five tests of this file fail).
/// `ClockNode::process` handing `tick` the block's `block_len` instead of the
/// segment's `seg.block_len` -> a segment's ticks run past it -> fails (all
/// five).
#[test]
fn a_ramp_puts_every_tick_within_the_grid_bound_of_the_maps_frame() {
    let frames = 5 * 48_000;
    let sent = play(&ramp(), 0.0, frames, 512);
    let ticks = frames_of(&sent, CLOCK);
    let mut exact = 0;
    let mut off_by_one = 0;
    let mut want_len = 0;
    for (k, &got) in ticks.iter().enumerate() {
        let x = frame_x(ramp_seconds, k as f64 / TICKS_PER_BEAT);
        let ceil = x.ceil();
        let d = ceil - x;
        let diff = got as f64 - ceil;
        assert!(diff.abs() <= 1.0, "tick {k}: frame {got}, the map says {x}");
        if d > E && d < 1.0 - E {
            assert_eq!(diff, 0.0, "tick {k}: frame {got}, the map says {x}");
        }
        exact += usize::from(diff == 0.0);
        off_by_one += usize::from(diff != 0.0);
        want_len = k + 1;
    }
    assert_eq!(ticks[0], 0);
    // 192 ticks in the ramp's 8 beats, then 24 a beat at 240 BPM: the whole
    // second renders `ticks` up to the last one before the end.
    let last = (ticks.len() - 1) as f64 / TICKS_PER_BEAT;
    assert!(
        ramp_seconds(last) * SR <= frames as f64
            && ramp_seconds(last + 1.0 / 24.0) * SR > frames as f64 - 1.5,
        "no tick skipped or added at the end: {want_len}"
    );
    assert!(exact > 150, "most ticks are exactly on the map: {exact}");
    // The bound is reached somewhere or the test is vacuous.
    assert!(off_by_one > 0, "some tick sits a frame off the exact one");
    // Past the ramp the map is a step at 240 BPM and the tick is exact.
    for (k, &got) in ticks.iter().enumerate().skip(192) {
        let f = frame_after(frame_x(ramp_seconds, k as f64 / TICKS_PER_BEAT));
        assert!(got.abs_diff(f) <= 1, "tick {k}");
    }
}

/// **No Song Position from a cell or a step boundary, and exactly 24 ticks a
/// beat.** From beat 0 a ramp and a step map send one Start and no Song
/// Position and no Continue in all their cells and blocks (a ramp has a cell
/// edge every 128 frames, a block of 512 holds 3 to 5, and the engine's cut
/// at 8 changes runs in the 4 096-frame block). From beat 2 the only Song
/// Position is the one of the start: one, on frame 0, of 8 sixteenths, with
/// its Continue. Ticks are counted exactly: `24 b + 1` ticks reach beat `b`.
///
/// Mutation (run): `SEEK_EPSILON_BEATS` = 0.0 (any difference between a
/// segment's start and the previous end is a locate) -> a Song Position on
/// ramp cell edges (the chord's end and the next start differ by an ulp) ->
/// fails (this test, and the locate test, which counts its Song Positions).
#[test]
fn a_ramp_or_step_boundary_is_not_a_locate() {
    for (name, m, secs, beats) in [
        ("ramp", ramp(), ramp_seconds as fn(f64) -> f64, 20.0),
        ("steps", steps(), step_seconds, 20.0),
    ] {
        for block in [512, 4_096] {
            let frames = (secs(beats) * SR) as usize;
            let sent = play(&m, 0.0, frames, block);
            assert_eq!(frames_of(&sent, START), [0], "{name} {block}: one Start");
            assert!(
                frames_of(&sent, SONG_POSITION).is_empty() && frames_of(&sent, CONTINUE).is_empty(),
                "{name} {block}: a boundary is not a locate: {sent:?}"
            );
            // Exactly 24 a beat: the tick of beat `beats` is on `frames`.
            assert_eq!(
                frames_of(&sent, CLOCK).len(),
                (beats * TICKS_PER_BEAT) as usize,
                "{name} {block}"
            );

            let sent = play(&m, 2.0, frames, block);
            let sp: Vec<_> = sent.iter().filter(|e| SONG_POSITION.is(e.system)).collect();
            assert_eq!(sp.len(), 1, "{name} {block}: {sent:?}");
            assert_eq!(
                (sp[0].frame, sp[0].system),
                (0, System::SongPosition(U14::of(8))),
                "{name} {block}"
            );
            assert_eq!(frames_of(&sent, CONTINUE), [0], "{name} {block}");
        }
    }
}

/// **The tick stream does not depend on the block size.** The ramp-then-step
/// map and the step map at blocks of 1, 64, 100, 512, 1 000 and 4 096 (the
/// last two reach the graph with more than 8 cell changes, so the engine
/// splits them): every event, Start and ticks, on the same absolute frame.
///
/// Mutation (run): `ClockNode::process` ticking only the first segment of a
/// block (`.take(1)`) -> fails, and with `seg.block_len` replaced by the whole
/// `block_len` -> fails (the 1-frame block is the reference's 512 only when
/// each segment is ticked for its own length).
#[test]
fn the_ticks_do_not_depend_on_the_block_size() {
    for (name, m, frames) in [("ramp", ramp(), 4 * 48_000), ("steps", steps(), 8 * 48_000)] {
        let reference = play(&m, 0.0, frames, 512);
        assert!(frames_of(&reference, CLOCK).len() > 100, "{name}");
        for block in [1, 64, 100, 1_000, 4_096] {
            assert_eq!(play(&m, 0.0, frames, block), reference, "{name} {block}");
        }
    }
}

/// **A locate inside a ramp: one Song Position at the seat, then the map's
/// ticks.** After 2 s of the ramp (past beat 2) the transport is sent to beat
/// 3 while rolling, unfaded, at a block start `F`. The playhead is seated on
/// beat 3 on frame `F` and follows the map from there, so tick `k` is on
/// `F + seconds(k / 24) x rate - seconds(3) x rate`, to within the ramp
/// bound and the frame `seconds(3) x rate` is short of one (2 frames in all).
/// Exactly one Song Position (of 12 sixteenths) is sent, on `F`, and the tick
/// of beat 3 is on `F` after it. A loop wrap inside a ramp is not
/// rendered here: it is the same seat on the wrap frame (the engine's
/// `tempo_ramps` tests pin the walker's seat), and `clock_master`'s unit
/// tests pin the wrap's Song Position at a constant tempo.
///
/// Mutation (run): `SEEK_EPSILON_BEATS` = 1e3 (no locate is ever seen) -> no
/// Song Position at the seat -> fails (only this test); `SEEK_EPSILON_BEATS` =
/// 0.0 -> Song Positions on cell edges -> fails.
#[test]
fn a_locate_inside_a_ramp_sends_one_song_position_then_the_maps_ticks() {
    let rig = rig(&ramp(), false);
    rig.send(MotionEvent::play_from(Beat(0.0)));
    let mut sent = Vec::new();
    rig.render(96_000, 512, &mut sent);
    rig.send(MotionEvent::Locate {
        beat: Beat(3.0),
        fade: FadeOut::Immediate,
        then: Then::Keep,
    });
    rig.render(96_000, 512, &mut sent);

    let sp: Vec<_> = sent.iter().filter(|e| SONG_POSITION.is(e.system)).collect();
    assert_eq!(sp.len(), 1, "only the locate: {sent:?}");
    assert_eq!(sp[0].system, System::SongPosition(U14::of(12)));
    assert_eq!(sp[0].frame, 96_000, "on the first frame of the block");
    let f = sp[0].frame;
    let after: Vec<u64> = frames_of(&sent, CLOCK)
        .into_iter()
        .filter(|&t| t >= f)
        .collect();
    assert_eq!(after[0], f, "the tick of beat 3 is on the locate's frame");
    let origin = ramp_seconds(3.0) * SR;
    for (i, &got) in after.iter().enumerate() {
        let k = 72.0 + i as f64;
        let want = f as f64 + ramp_seconds(k / TICKS_PER_BEAT) * SR - origin;
        assert!(
            (got as f64 - want).abs() <= 2.0,
            "tick {k}: frame {got}, the map says {want}"
        );
    }
    assert!(after.len() > 100);
}

/// **MTC rides wall-clock time, not the tempo.** With MTC on at 25 fps over a
/// ramp, quarter-frame `k` is on frame `floor(k x 48 000 / 100)` = `480 k`
/// whatever the tempo does (the grid is in frames: `rate / (4 fps)`), the
/// stream is the same at block sizes 64, 100 and 512 and a ramp sends no
/// Song Position.
///
/// Mutation (run): `ClockNode::process` ticking only the first segment of a
/// block (`.take(1)`) -> the quarter-frames of later cells are lost -> fails.
#[test]
fn mtc_keeps_wall_clock_time_across_a_ramp() {
    let frames = 4 * 48_000;
    let run = |block: usize| {
        let rig = rig(&ramp(), true);
        rig.send(MotionEvent::play_from(Beat(0.0)));
        let mut sent = Vec::new();
        rig.render(frames, block, &mut sent);
        sent
    };
    let reference = run(512);
    let qf = frames_of(&reference, Kind::TimeCode);
    assert_eq!(qf.len(), frames / 480);
    for (k, &f) in qf.iter().enumerate() {
        assert_eq!(f, 480 * k as u64, "quarter-frame {k}");
    }
    assert!(frames_of(&reference, SONG_POSITION).is_empty());
    for block in [64, 100] {
        assert_eq!(run(block), reference, "block {block}");
    }
}
