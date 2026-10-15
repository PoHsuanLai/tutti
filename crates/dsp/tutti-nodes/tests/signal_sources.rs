//! The signal-driven modulation sources (doc 014 §6): `EnvelopeFollowerNode`
//! and `NoteEnvelopeNode`, alone and through a `ControlLaneNode`, against
//! analytic figures and invariants of the render (never another reference
//! render).
//!
//! Each test names the mutations it was seen to fail under. The serial ==
//! parallel and no-allocation gates are in `rt_no_alloc_modulation.rs`,
//! beside the gated pool they share.

use std::sync::Arc;

use tutti_core::{OfflineTimeline, OfflineTimelineConfig};
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, Executor, ForkByClone, ForkMode,
    ForkTarget, Io, Node, Offset, ParamFrom, ParamIn, ParamRange, ParamSegment, ParamShaping,
    Prepare, Shape, Status, Transport, TransportChanges,
};
use tutti_mod::{
    Adsr, Amount, ControlProgram, CurveType, InputLayer, LayerId, Polarity, SignalKind, SourceId,
    SourceNode,
};
use tutti_nodes::automation::{ControlLaneNode, CONTROL_STEP};
use tutti_nodes::{EnvelopeFollowerNode, NoteEnvelopeNode};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{
    Amplitude, AtomicF32, Beat, Bpm, ChannelLayout, Depth, FrameClock, Hz, NodeKey,
    OfflineTransport, ParamKey, SampleRate, Samples, Seconds, UnitParam,
};

const RATE: f64 = 48_000.0;
const TEMPO: Bpm = Bpm(120.0);
/// 24 000 frames a beat.
const FPB: f64 = 24_000.0;
const STEP: usize = CONTROL_STEP as usize;

// ---------------------------------------------------------------- nodes --

/// Audio on the executor's own clock: `level` on frames `[from, to)` of
/// each `(from, to, level)`, else 0; channel `c` at `1 / (c + 1)` of it.
#[derive(Clone)]
struct Pulse {
    width: ChannelLayout,
    on: Vec<(u64, u64, f32)>,
}

impl Node for Pulse {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.width)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let base = cx.env.frame.get();
        for c in 0..io.output_count() {
            let scale = 1.0 / (c as f32 + 1.0);
            for (i, x) in io.output(c).iter_mut().enumerate() {
                let f = base + i as u64;
                *x = self
                    .on
                    .iter()
                    .find(|(a, b, _)| (*a..*b).contains(&f))
                    .map_or(0.0, |o| o.2 * scale);
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A transport-locked kick (a clip): while rolling, a decaying burst on
/// the first tenth of every beat, `1 − frac · 10`, the right channel at
/// half of it. While stopped it plays `bleed` (a monitored input before
/// the take: what makes a follower's play-start reset matter).
#[derive(Clone)]
struct Kick {
    bleed: f32,
}

impl Node for Kick {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        for i in 0..n {
            let t = cx
                .env
                .transport_at(Offset::new(i, cx.env.block_len).expect("in"));
            let v = if t.playing {
                let frac = t.beat().get().rem_euclid(1.0);
                if frac < 0.1 {
                    (1.0 - frac * 10.0) as f32
                } else {
                    0.0
                }
            } else {
                self.bleed
            };
            io.output(0)[i] = v;
            io.output(1)[i] = v * 0.5;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn note_on(note: u32) -> [u32; 4] {
    [0x2090_0000 | (note << 8) | 100, 0, 0, 0]
}

fn note_off(note: u32) -> [u32; 4] {
    [0x2080_0000 | (note << 8), 0, 0, 0]
}

/// A MIDI 2.0 note-on (group 0, channel 0) at 16-bit `velocity`.
fn note_on_2(note: u32, velocity: u32) -> [u32; 4] {
    [0x4090_0000 | (note << 8), velocity << 16, 0, 0]
}

/// A MIDI 2.0 note-off (group 0, channel 0).
fn note_off_2(note: u32) -> [u32; 4] {
    [0x4080_0000 | (note << 8), 0, 0, 0]
}

/// A MIDI clip: while rolling, note 60 on at every beat and off half a
/// beat later. While stopped, note 64 on at executor frame `stopped_on` (a
/// key pressed before the take, never released).
#[derive(Clone)]
struct Seq {
    stopped_on: Option<u64>,
}

impl Node for Seq {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = cx.env.block_len;
        for i in 0..n.get() {
            let at = Offset::new(i, n).expect("in");
            let t = cx.env.transport_at(at);
            let words = if t.playing {
                let f = t.beat().get().rem_euclid(1.0) * FPB;
                if f < 0.5 {
                    Some(note_on(60))
                } else if (f - FPB / 2.0).abs() < 0.5 {
                    Some(note_off(60))
                } else {
                    None
                }
            } else {
                (Some(cx.env.frame.get() + i as u64) == self.stopped_on).then(|| note_on(64))
            };
            if let Some(w) = words {
                io.event_out(0).push(Event::midi(at, w)).expect("room");
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// MIDI on its planned executor frames (up to 1024 a block).
#[derive(Clone)]
struct Midi {
    plan: Vec<(u64, [u32; 4])>,
}

impl Node for Midi {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(1024)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (base, n) = (cx.env.frame.get(), cx.env.block_len);
        for &(at, w) in &self.plan {
            if let Some(o) = at
                .checked_sub(base)
                .and_then(|o| Offset::new(o as usize, n))
            {
                io.event_out(0).push(Event::midi(o, w)).expect("room");
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A source's segments as a lane takes them: output 0 is the value per
/// frame (`ParamSegment::raw_value_after` from the latest segment's
/// frame, 0 before any), and on each segment's frame output 1 is 1, output
/// 2 its start and output 3 its end (0 elsewhere).
#[derive(Clone, Default)]
struct Tap {
    last: Option<(ParamSegment, u64)>,
}

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(4)).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let base = cx.env.frame.get();
        let events: Vec<(usize, ParamSegment)> = io
            .events(0)
            .as_slice()
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::Segment(s) => Some((e.offset.index(), s)),
                _ => None,
            })
            .collect();
        let mut out = vec![vec![0.0f32; n]; 4];
        let mut k = 0;
        #[allow(clippy::needless_range_loop, reason = "four outputs at one frame")]
        for i in 0..n {
            while let Some(&(_, s)) = events.get(k).filter(|e| e.0 == i) {
                self.last = Some((s, base + i as u64));
                let id = match s.addr() {
                    tutti_types::ParamAddr::Id(id) => id,
                    a => panic!("a source sends foreign segments, got {a:?}"),
                };
                out[1][i] = 1.0;
                out[2][i] = s.foreign_start(id).expect("foreign");
                out[3][i] = s.foreign_target(id).expect("foreign");
                k += 1;
            }
            out[0][i] = self.last.map_or(0.0, |(s, at)| {
                s.raw_value_after(Samples((base + i as u64 - at) as usize))
            });
        }
        for (c, v) in out.iter().enumerate() {
            io.output(c).copy_from_slice(v);
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.last = None;
    }
}

/// Declares `Cutoff` (base 0) and writes the value its port reads.
#[derive(Clone)]
struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_params(&[UnitParam::Cutoff])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match io.param(0).frames() {
            Some(v) => {
                let v: Vec<f32> = v.to_vec();
                io.output(0).copy_from_slice(&v);
            }
            None => io.output(0).fill(0.0),
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (port == 0).then_some(0.0)
    }
}

// ---------------------------------------------------------------- rigs ---

fn out(node: u64, port: u16) -> Source {
    Source::Node(OutPort {
        node: NodeKey(node),
        port,
    })
}

fn editor(max: usize) -> (Editor, Executor) {
    Editor::new(Prepare::new(SampleRate(RATE), Samples(max)))
}

/// Audio output `port` of `from` into input `port` of `to`, for each port.
fn wire_audio(ed: &mut Editor, from: u64, to: u64, ports: u16) {
    for port in 0..ports {
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(to),
                port,
            },
            Edge::Direct(out(from, port)),
        );
    }
}

fn wire_events(ed: &mut Editor, from: u64, to: u64) {
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(to),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(from),
            port: 0,
        }),
    );
}

/// A `Tap` at `tap` on `from`'s events, its four outputs appended.
fn tap(ed: &mut Editor, from: u64, tap: u64) {
    ed.insert(NodeKey(tap), "tap", ForkByClone(Tap::default()));
    wire_events(ed, from, tap);
    ed.spec_mut()
        .topology
        .outputs
        .extend((0..4).map(|p| out(tap, p)));
}

fn cutoff(node: u64) -> ParamIn {
    ParamIn {
        node: NodeKey(node),
        param: UnitParam::Cutoff,
    }
}

/// A probe at `probe` (cutoff ranged `0..=1`) fed by a lane at `lane`
/// reading source `source` at `depth`, knob `knob`, whose input is
/// `from`'s events; the probe's output appended. Commits first, so the
/// range cell exists.
fn lane_to_probe(
    ed: &mut Editor,
    from: u64,
    source: u32,
    depth: f32,
    knob: f32,
    lane: u64,
    probe: u64,
) {
    ed.insert(NodeKey(probe), "probe", ForkByClone(Probe));
    ed.spec_mut().topology.outputs.push(out(probe, 0));
    ed.set_param_range(cutoff(probe), ParamRange::new(0.0, 1.0))
        .expect("a probe");
    ed.commit().expect("commits");
    let input = InputLayer {
        id: LayerId(1),
        source: SourceId(source),
        amount: Amount {
            depth: Depth(depth),
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
        },
    };
    let program = ControlProgram::new(None, vec![], vec![input]).expect("valid");
    let node = ControlLaneNode::for_param(
        ParamKey::<Hz>::CUTOFF,
        program,
        Some(Arc::new(AtomicF32::new(knob))),
        Some(ed.param_range_cell(cutoff(probe)).expect("committed")),
    );
    ed.insert(NodeKey(lane), "lane", node);
    ed.spec_mut().connect_param(
        cutoff(probe),
        ParamFrom::Events(EventOut {
            node: NodeKey(lane),
            port: 0,
        }),
        ParamShaping::Identity,
    );
    wire_events(ed, from, lane);
}

/// One block of `n` frames: every output channel's frames.
fn block(
    ed: &mut Editor,
    exec: &mut Executor,
    t: &Transport,
    ch: &TransportChanges,
    n: usize,
) -> Vec<Vec<f32>> {
    let outs = ed.spec().topology.outputs.len();
    let mut bufs = vec![vec![0.0f32; n]; outs];
    {
        let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
        exec.process_with_changes(n, t, ch, &[], &mut refs);
    }
    ed.collect();
    bufs
}

fn append(all: &mut Vec<Vec<f32>>, got: Vec<Vec<f32>>) {
    all.resize(got.len().max(all.len()), Vec::new());
    for (a, g) in all.iter_mut().zip(got) {
        a.extend(g);
    }
}

/// Stopped blocks of the sizes `sizes` cycles through, to `total` frames.
fn render_stopped(
    ed: &mut Editor,
    exec: &mut Executor,
    sizes: &[usize],
    total: usize,
) -> Vec<Vec<f32>> {
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let (mut all, mut done, mut k) = (Vec::new(), 0, 0);
    while done < total {
        let n = sizes[k % sizes.len()].min(total - done);
        append(
            &mut all,
            block(ed, exec, &stopped, &TransportChanges::NONE, n),
        );
        done += n;
        k += 1;
    }
    all
}

/// A ragged block schedule up to `max`, summing to `total`.
fn ragged(total: usize, max: usize) -> Vec<usize> {
    let mut v = Vec::new();
    let (mut sum, mut i) = (0, 0usize);
    while sum < total {
        let n = [1, max, 7, 64, max - 1, 33, 2, 100][i % 8]
            .min(max)
            .min(total - sum);
        v.push(n);
        sum += n;
        i += 1;
    }
    v
}

fn offline() -> OfflineTransport {
    OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(RATE),
        ..Default::default()
    })))
}

/// The host's playhead: a frame clock from beat 0 at `TEMPO`.
struct Host {
    clock: FrameClock,
}

impl Host {
    fn new() -> Self {
        Self {
            clock: FrameClock::new(Beat(0.0), TEMPO, SampleRate(RATE)),
        }
    }

    fn rolling(&self) -> Transport {
        Transport::counted(true, TEMPO, self.clock.origin(), None)
    }

    fn roll(&mut self, ed: &mut Editor, exec: &mut Executor, n: usize, all: &mut Vec<Vec<f32>>) {
        let t = self.rolling();
        append(all, block(ed, exec, &t, &TransportChanges::NONE, n));
        self.clock.advance(Samples(n), None);
    }
}

/// The one-pole coefficient the follower uses: `exp(−1 / (τ · rate))` in
/// `f64` (as `libm::exp` computes it, to the last bit or so).
fn pole(tau: f32) -> f64 {
    (-1.0 / (f64::from(tau) * RATE)).exp()
}

fn follower(source: u32, attack: f32, release: f32, width: ChannelLayout) -> EnvelopeFollowerNode {
    EnvelopeFollowerNode::new(SourceId(source), Seconds(attack), Seconds(release), width)
}

fn adsr(a: f32, d: f32, s: f32, r: f32) -> Adsr {
    Adsr {
        attack: Seconds(a),
        decay: Seconds(d),
        sustain: Amplitude(s),
        release: Seconds(r),
    }
}

/// MIDI `plan` into a note envelope of `adsr` into a `Tap` (outputs 0..4),
/// committed, at most `max` frames a block.
fn envelope_rig(max: usize, plan: Vec<(u64, [u32; 4])>, adsr: Adsr) -> (Editor, Executor) {
    let (mut ed, exec) = editor(max);
    ed.insert(NodeKey(1), "midi", ForkByClone(Midi { plan }));
    ed.insert(
        NodeKey(2),
        "envelope",
        ForkByClone(NoteEnvelopeNode::new(SourceId(1), adsr)),
    );
    wire_events(&mut ed, 1, 2);
    tap(&mut ed, 2, 3);
    ed.commit().expect("commits");
    (ed, exec)
}

/// The frames a `Tap` saw a segment on.
fn marks(tap: &[Vec<f32>]) -> Vec<usize> {
    (0..tap[1].len()).filter(|&f| tap[1][f] != 0.0).collect()
}

/// The `(start, end)` of the segment a `Tap` saw on frame `f`.
fn piece(tap: &[Vec<f32>], f: usize) -> (f32, f32) {
    (tap[2][f], tap[3][f])
}

// ---------------------------------------------------------------- tests --

/// **The follower runs one grid step late, whatever the blocks.** A mono
/// 1.0 from executor frame 100 (the play clock counts from the executor's
/// first frame, stopped) into a follower of attack 2 ms: on every grid
/// frame `g` it sends one piece, `env(g − 32) → env(g)`, where `env(g) =
/// 1 − c^(g − 100)` counts the audio before `g` only (analytic, within
/// 1e-5). Before 128 it sends nothing (silent), and the first piece, at
/// 128, starts at exactly 0. The same input in blocks of 1, 7, 32, 100,
/// 128 and a ragged schedule sends the same events, bit for bit.
///
/// Mutations (run), each fails: the frame's level taken before its grid
/// point is sent (`follow` moved above the grid check) → `env(g)` counts
/// frame `g` and is off by one frame (c^61 for c^60); the grid counted
/// from the block (`i % 32` for the play count) → other block sizes send
/// on other frames; the silent skip dropped → a piece at frame 0.
#[test]
fn a_follower_lags_one_step_at_any_block_size() {
    let render = |sizes: &[usize]| {
        let (mut ed, mut exec) = editor(128);
        ed.insert(
            NodeKey(1),
            "pulse",
            ForkByClone(Pulse {
                width: ChannelLayout::MONO,
                on: vec![(100, u64::MAX, 1.0)],
            }),
        );
        ed.insert(
            NodeKey(2),
            "follower",
            ForkByClone(follower(5, 0.002, 0.05, ChannelLayout::MONO)),
        );
        wire_audio(&mut ed, 1, 2, 1);
        tap(&mut ed, 2, 3);
        ed.commit().expect("commits");
        render_stopped(&mut ed, &mut exec, sizes, 2_000)
    };
    let one = render(&[128]);
    let c = pole(0.002);
    let env = |g: usize| {
        if g <= 100 {
            0.0
        } else {
            1.0 - c.powi(g as i32 - 100)
        }
    };
    let marks: Vec<usize> = (0..2_000).filter(|&f| one[1][f] != 0.0).collect();
    let grid: Vec<usize> = (128..2_000).step_by(STEP).collect();
    assert_eq!(
        marks, grid,
        "a piece on every grid frame from the first moving one"
    );
    assert_eq!(one[2][128].to_bits(), 0, "the first piece starts at 0");
    for &g in &grid {
        let (from, to) = (f64::from(one[2][g]), f64::from(one[3][g]));
        assert!(
            (from - env(g - STEP)).abs() < 1e-5,
            "frame {g}: from {from}"
        );
        assert!(
            (to - env(g)).abs() < 1e-5,
            "frame {g}: to {to} vs {}",
            env(g)
        );
    }
    // One step at 160 in full: from 1 − c^28 to 1 − c^60.
    assert!((f64::from(one[3][160]) - (1.0 - c.powi(60))).abs() < 1e-6);
    for sizes in [&[1][..], &[7], &[32], &[100], &ragged(2_000, 128)] {
        let other = render(sizes);
        for (k, (a, b)) in one.iter().zip(&other).enumerate() {
            for f in 0..2_000 {
                assert_eq!(
                    a[f].to_bits(),
                    b[f].to_bits(),
                    "{sizes:?}: channel {k}, frame {f}"
                );
            }
        }
    }
}

/// **Both reset at play start, on its frame.** A constant 1.0 into a
/// follower (attack 1 ms) and a note held from frame 10 into a note
/// envelope, stopped for three blocks, so both are up (the envelope at its
/// sustain, 0.6). The transport starts on frame 37 of the fourth block (s
/// = 421): there the follower sends a piece from 0 to 0 and the note
/// envelope a 0, and both read 0 from that frame. Then the follower
/// climbs again from 0, `1 − c^32` at s + 32 (analytic, within 1e-6), and
/// the note envelope stays at 0: its note was forgotten, so its note-off at
/// s + 100 sends nothing.
///
/// Mutations (run), each fails: the follower's reset dropped (`idle` not
/// called on an epoch change) → its piece at s ends near 1, not 0; the
/// note envelope's dropped (`starts` never answering) → no segment on
/// the start's frame.
#[test]
fn a_follower_and_a_note_envelope_reset_at_play_start() {
    let (mut ed, mut exec) = editor(128);
    ed.insert(
        NodeKey(1),
        "pulse",
        ForkByClone(Pulse {
            width: ChannelLayout::MONO,
            on: vec![(0, u64::MAX, 1.0)],
        }),
    );
    ed.insert(
        NodeKey(2),
        "follower",
        ForkByClone(follower(5, 0.001, 0.05, ChannelLayout::MONO)),
    );
    wire_audio(&mut ed, 1, 2, 1);
    tap(&mut ed, 2, 3);
    let s = 3 * 128 + 37;
    ed.insert(
        NodeKey(4),
        "midi",
        ForkByClone(Midi {
            plan: vec![(10, note_on(60)), (s as u64 + 100, note_off(60))],
        }),
    );
    ed.insert(
        NodeKey(5),
        "envelope",
        ForkByClone(NoteEnvelopeNode::new(
            SourceId(6),
            adsr(0.001, 0.002, 0.6, 0.01),
        )),
    );
    wire_events(&mut ed, 4, 5);
    tap(&mut ed, 5, 6);
    ed.commit().expect("commits");

    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let mut all = Vec::new();
    for _ in 0..3 {
        append(
            &mut all,
            block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128),
        );
    }
    let host = Host::new();
    let mut ch = TransportChanges::NONE;
    ch.push(Offset::new(37, Samples(128)).expect("in"), host.rolling())
        .expect("room");
    append(&mut all, block(&mut ed, &mut exec, &stopped, &ch, 128));
    for _ in 0..4 {
        let t = host.rolling();
        append(
            &mut all,
            block(&mut ed, &mut exec, &t, &TransportChanges::NONE, 128),
        );
    }
    let (follow, env) = (&all[..4], &all[4..]);
    assert!(
        follow[0][s - 1] > 0.9,
        "the follower was up: {}",
        follow[0][s - 1]
    );
    assert_eq!(env[0][s - 1], 0.6, "the envelope was at its sustain");
    for (name, ch) in [("follower", follow), ("envelope", env)] {
        assert_eq!(ch[1][s], 1.0, "{name}: a segment on the start's frame");
        assert_eq!((ch[2][s], ch[3][s]), (0.0, 0.0), "{name}: to 0");
        assert_eq!(ch[0][s], 0.0, "{name}: idle on the frame");
    }
    let c = pole(0.001);
    assert!((f64::from(follow[3][s + STEP]) - (1.0 - c.powi(32))).abs() < 1e-6);
    assert_eq!(follow[2][s + STEP], 0.0, "climbing from 0");
    assert!(
        env[0][s..].iter().all(|&v| v == 0.0),
        "the note was forgotten"
    );
    assert!(
        env[1][s + 1..].iter().all(|&v| v == 0.0),
        "and its note-off sends nothing"
    );
}

/// **The note envelope's stages land on their frames.** 48 kHz, attack
/// 10 ms (480 frames), decay 20 ms (960), sustain 0.5, release 30 ms
/// (1440); linear stages, so every figure below is exact in `f32`:
///
/// - a note on at 1000 reads 0 there, 0.5 at 1240 and 1.0 at 1480 (the
///   peak, on its frame), 0.75 at 1960 and the sustain 0.5 from 2440;
///   off at 5000, it reads 0.25 at 5720 and 0 from 6440;
/// - **mid-attack**: on at 10 000, off at 10 120 (a quarter up) releases
///   from 0.25: 0.125 at 10 840, 0 from 11 560;
/// - **retrigger, last-note priority**: A on at 20 000 (sustain from
///   21 440), B on at 22 000 restarts the attack from 0.5 (0.75 at 22 240,
///   1.0 at 22 480); A off at 23 000 changes nothing (B is held), so the
///   decay runs on; B off at 25 000 releases from the sustain, and a second
///   A off at 26 000 does nothing.
///
/// The segments are sent on exactly 1000, 1480, 5000, 10 000, 10 120,
/// 20 000, 20 480, 22 000, 22 480 and 25 000, in blocks of 1, 37 and a
/// ragged schedule alike.
///
/// Mutations (run), each fails: the attack's length a frame short
/// (`attack - 1`) → the decay's segment at 1479, not 1480; a release from
/// 1.0 rather than the level it starts at → the release at 5000 starts at
/// 1, not the sustain; a note-off releasing while another note is held
/// (`holding == 0` dropped) → a segment at A's note-off, 23 000; a
/// retrigger from 0 rather than the current level → B's attack starts at
/// 0 on 22 000.
#[test]
fn a_note_envelopes_stages_land_on_their_frames() {
    let (a, b) = (60, 62);
    let plan = vec![
        (1_000, note_on(a)),
        (5_000, note_off(a)),
        (10_000, note_on(a)),
        (10_120, note_off(a)),
        (20_000, note_on(a)),
        (22_000, note_on(b)),
        (23_000, note_off(a)),
        (25_000, note_off(b)),
        (26_000, note_off(a)),
    ];
    let render = |sizes: &[usize]| {
        let (mut ed, mut exec) = editor(128);
        ed.insert(NodeKey(1), "midi", ForkByClone(Midi { plan: plan.clone() }));
        let decl = SourceNode {
            id: SourceId(3),
            kind: SignalKind::NoteEnvelope {
                adsr: adsr(0.01, 0.02, 0.5, 0.03),
            },
        };
        let node = NoteEnvelopeNode::from_decl(&decl).expect("a note envelope");
        assert_eq!(node.source(), SourceId(3));
        ed.insert(NodeKey(2), "envelope", ForkByClone(node));
        wire_events(&mut ed, 1, 2);
        tap(&mut ed, 2, 3);
        ed.commit().expect("commits");
        render_stopped(&mut ed, &mut exec, sizes, 28_000)
    };
    let all = render(&[128]);
    let v = &all[0];
    let marks: Vec<usize> = (0..28_000).filter(|&f| all[1][f] != 0.0).collect();
    assert_eq!(
        marks,
        [1_000, 1_480, 5_000, 10_000, 10_120, 20_000, 20_480, 22_000, 22_480, 25_000]
    );
    let at = |f: usize| v[f];
    assert!(v[..1_000].iter().all(|&x| x == 0.0));
    for (f, want) in [
        (1_000, 0.0),
        (1_240, 0.5),
        (1_479, 479.0 / 480.0),
        (1_480, 1.0),
        (1_960, 0.75),
        (2_440, 0.5),
        (4_999, 0.5),
        (5_000, 0.5),
        (5_720, 0.25),
        (6_440, 0.0),
        (9_999, 0.0),
        (10_120, 0.25),
        (10_840, 0.125),
        (11_560, 0.0),
        (21_440, 0.5),
        (22_000, 0.5),
        (22_240, 0.75),
        (22_480, 1.0),
        (23_440, 0.5),
        (25_000, 0.5),
        (26_440, 0.0),
        (27_999, 0.0),
    ] {
        assert_eq!(at(f), want, "frame {f}");
    }
    // A's note-off at 23 000 changed nothing: the decay from 22 480 runs on.
    let decay = |f: usize| 1.0 + (0.5 - 1.0) * ((f - 22_480) as f32 / 960.0);
    assert_eq!(at(23_000), decay(23_000));
    // The second A off at 26 000 sent nothing (in the marks above).
    for sizes in [&[1][..], &[37], &ragged(28_000, 128)] {
        let other = render(sizes);
        for (k, (x, y)) in all.iter().zip(&other).enumerate() {
            for f in 0..28_000 {
                assert_eq!(
                    x[f].to_bits(),
                    y[f].to_bits(),
                    "{sizes:?}: channel {k}, frame {f}"
                );
            }
        }
    }
}

/// **`from_decl` builds the kind it is given, and refuses the other.**
///
/// Mutation (run): `EnvelopeFollowerNode::from_decl` answering every kind
/// (the `NoteEnvelope` arm `Some`) → fails.
#[test]
fn from_decl_builds_its_own_kind() {
    let f = SourceNode {
        id: SourceId(9),
        kind: SignalKind::Follower {
            attack: Seconds(0.01),
            release: Seconds(0.1),
        },
    };
    let n = SourceNode {
        id: SourceId(4),
        kind: SignalKind::NoteEnvelope {
            adsr: adsr(0.01, 0.1, 0.5, 0.2),
        },
    };
    let node = EnvelopeFollowerNode::from_decl(&f, ChannelLayout::STEREO).expect("a follower");
    assert_eq!(node.source(), SourceId(9));
    assert_eq!(node.shape().audio_in, ChannelLayout::STEREO);
    assert!(EnvelopeFollowerNode::from_decl(&n, ChannelLayout::MONO).is_none());
    assert_eq!(
        NoteEnvelopeNode::from_decl(&n).map(|n| n.source()),
        Some(SourceId(4))
    );
    assert!(NoteEnvelopeNode::from_decl(&f).is_none());
}

/// **Pad dips with the kick, end to end.** A 50 ms stereo burst from
/// frame 4 800, 0.8 on its right channel and 0.4 on its left, into a
/// follower (attack 1 ms, release 50 ms), into a lane on a probe's cutoff
/// (knob 0.8, range 0..1, depth −0.6, bipolar linear): the follower reads
/// the peak over channels, 0.8, and the probe reads `0.8 − 0.6 · x(f)`
/// within 1e-4 at every frame, where `x` is the follower's analytic
/// envelope one step late, linear between grid frames (`env(g) = 0.8 ·
/// (1 − c_a^(g − 4800))` during the burst, `env(7200) · c_r^(g − 7200)`
/// after). It dips below 0.33 during the burst and is back within 1e-3 of
/// 0.8 half a second after it.
///
/// Mutations (run), each fails: the level read from the first channel
/// only (0.4); the mean over channels (0.6); the sum (1.2, clamped to 1);
/// attack and release swapped (`level < env` choosing the attack) → the
/// burst's rise lags; the frame's level read before its grid point is sent
/// → the first piece at 4 800 already moves.
#[test]
fn a_kick_ducks_the_pad_through_a_lane() {
    let (mut ed, mut exec) = editor(128);
    ed.insert(
        NodeKey(1),
        "kick",
        ForkByClone(Pulse {
            width: ChannelLayout::STEREO,
            on: vec![(4_800, 7_200, 0.8)],
        }),
    );
    ed.insert(
        NodeKey(2),
        "follower",
        ForkByClone(follower(7, 0.001, 0.05, ChannelLayout::STEREO)),
    );
    // Crossed: the pulse's full channel (0) on the follower's right, so
    // the peak is not the first channel.
    for (from, to) in [(0, 1), (1, 0)] {
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(2),
                port: to,
            },
            Edge::Direct(out(1, from)),
        );
    }
    lane_to_probe(&mut ed, 2, 7, -0.6, 0.8, 3, 4);
    ed.commit().expect("commits");
    let total = 48_000;
    let all = render_stopped(&mut ed, &mut exec, &ragged(total, 128), total);
    let (ca, cr) = (pole(0.001), pole(0.05));
    let env = |g: usize| -> f64 {
        if g <= 4_800 {
            0.0
        } else if g <= 7_200 {
            0.8 * (1.0 - ca.powi(g as i32 - 4_800))
        } else {
            0.8 * (1.0 - ca.powi(2_400)) * cr.powi(g as i32 - 7_200)
        }
    };
    let x = |f: usize| -> f64 {
        let g = f / STEP * STEP;
        if g < STEP {
            return 0.0;
        }
        let (a, b) = (env(g - STEP), env(g));
        a + (b - a) * (f - g) as f64 / STEP as f64
    };
    let probe = &all[0];
    for (f, &v) in probe.iter().enumerate() {
        let want = 0.8 - 0.6 * x(f);
        assert!(
            (f64::from(v) - want).abs() < 1e-4,
            "frame {f}: {v} vs {want}"
        );
    }
    let low = probe.iter().copied().fold(f32::INFINITY, f32::min);
    assert!(low < 0.33, "the pad dips: {low}");
    assert!((probe[7_200 + 24_000] - 0.8).abs() < 1e-3, "and recovers");
    assert_eq!(probe[4_800], 0.8, "nothing before the kick is heard");
}

/// **Export == live for a follower and a note envelope**, through lanes,
/// bit for bit from the play start: a transport-locked kick (stereo, the
/// right channel at half) into a follower and a MIDI clip into a note
/// envelope, each into a lane on a probe. Live runs stopped first, the
/// kick bleeding 0.8 into the follower and a key pressed at frame 50
/// holding the note envelope up (so both are up at the start), then starts
/// inside a block on frame 421 (not a grid multiple of the executor's
/// frame count) and renders ragged blocks. The export is an offline fork
/// taken before the start, in 1024-frame blocks.
///
/// Mutations (run), each fails: the follower's play-start reset dropped →
/// live's follower starts from the bleed's level; the note envelope's
/// dropped → live's envelope starts in its sustain, holding the key; the
/// follower's grid on the executor's frame rather than the play clock
/// (`env.frame + i` for `r.play`) → live's grid is 421 frames off the
/// export's.
#[test]
fn an_export_is_the_live_render_for_signal_sources() {
    let (mut ed, mut exec) = editor(128);
    ed.insert(NodeKey(1), "kick", ForkByClone(Kick { bleed: 0.8 }));
    ed.insert(
        NodeKey(2),
        "follower",
        ForkByClone(follower(3, 0.002, 0.08, ChannelLayout::STEREO)),
    );
    wire_audio(&mut ed, 1, 2, 2);
    lane_to_probe(&mut ed, 2, 3, -0.6, 0.8, 3, 4);
    ed.insert(
        NodeKey(5),
        "clip",
        ForkByClone(Seq {
            stopped_on: Some(50),
        }),
    );
    ed.insert(
        NodeKey(6),
        "envelope",
        ForkByClone(NoteEnvelopeNode::new(
            SourceId(4),
            adsr(0.002, 0.003, 0.4, 0.2),
        )),
    );
    wire_events(&mut ed, 5, 6);
    lane_to_probe(&mut ed, 6, 4, 0.5, 0.2, 7, 8);
    ed.commit().expect("commits");

    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let mut before = Vec::new();
    for _ in 0..3 {
        append(
            &mut before,
            block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128),
        );
    }
    assert!(
        before[0][383] < 0.5,
        "the bleed ducked live's pad before the start"
    );
    assert!(
        before[1][383] > 0.3,
        "the held key raised live's envelope target"
    );
    let offline = offline();
    let (mut fed, mut fexec) = ed
        .fork(
            ForkTarget::Master,
            ForkMode::Offline(&offline),
            Prepare::new(SampleRate(RATE), Samples(1024)),
        )
        .expect("forks");
    let total = 60_000;

    let mut host = Host::new();
    let mut ch = TransportChanges::NONE;
    ch.push(Offset::new(37, Samples(128)).expect("in"), host.rolling())
        .expect("room");
    let mut live = block(&mut ed, &mut exec, &stopped, &ch, 128);
    for c in &mut live {
        c.drain(..37);
    }
    host.clock.advance(Samples(128 - 37), None);
    for n in ragged(total - (128 - 37), 128) {
        host.roll(&mut ed, &mut exec, n, &mut live);
    }

    let (mut host, mut export) = (Host::new(), Vec::new());
    while export.first().map_or(0, Vec::len) < total {
        host.roll(&mut fed, &mut fexec, 1024, &mut export);
    }
    for (c, (l, e)) in live.iter().zip(&export).enumerate() {
        assert!(
            l.iter().any(|&v| v != l[0]),
            "channel {c} moves: the comparison walks pieces"
        );
        for f in 0..total {
            assert_eq!(
                l[f].to_bits(),
                e[f].to_bits(),
                "channel {c}, frame {f} of play: live {} vs export {}",
                l[f],
                e[f]
            );
        }
    }
}

/// **A fresh note envelope resets at a start inside its first block.** A
/// fresh node, stopped, takes a note-on at frame 10 (attack 10 ms, 480
/// frames), and the transport starts on frame 50 of that same first
/// block: there it sends a 0 and reads 0 to the end of the render (its
/// note is forgotten). Only a start on a fresh node's very first frame is
/// none (it is idle already).
///
/// Mutation (run): the old first-block skip (no starts read while the
/// node is fresh: `walk.starts` given a length of 0 then) → no segment at
/// 50, and it attacks on (0.083 at 50) → fails.
#[test]
fn a_fresh_note_envelope_resets_at_a_start_in_its_first_block() {
    let (mut ed, mut exec) =
        envelope_rig(128, vec![(10, note_on(60))], adsr(0.01, 0.01, 0.5, 0.01));
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let host = Host::new();
    let mut ch = TransportChanges::NONE;
    ch.push(Offset::new(50, Samples(128)).expect("in"), host.rolling())
        .expect("room");
    let mut all = Vec::new();
    append(&mut all, block(&mut ed, &mut exec, &stopped, &ch, 128));
    for _ in 0..4 {
        let t = host.rolling();
        append(
            &mut all,
            block(&mut ed, &mut exec, &t, &TransportChanges::NONE, 128),
        );
    }
    assert_eq!(marks(&all), [10, 50]);
    assert_eq!(piece(&all, 10), (0.0, 1.0), "the attack");
    assert_eq!(piece(&all, 50), (0.0, 0.0), "idle on the start's frame");
    assert!(all[0][50..].iter().all(|&v| v == 0.0), "and from it on");
    assert!(all[0][11..50].iter().all(|&v| v > 0.0), "it was attacking");
}

/// **MIDI 2.0 notes, and a note-on at velocity 0.** Attack and decay 5 ms
/// (240 frames), sustain 0.5, release 10 ms: a MIDI 2.0 note-on at 100
/// attacks (0 → 1, the peak at 340, decay 1 → 0.5), a MIDI 2.0 note-off at
/// 1000 releases (0.5 → 0). A MIDI 2.0 note-on at velocity 0 is a note-on
/// (MIDI 2.0 has no velocity-0 note-off): at 3000 it attacks again. A MIDI
/// 1.0 note-on at velocity 0 is a note-off, and the same group, channel
/// and key across the two protocols is one note: at 4000 it releases
/// from the sustain.
///
/// Mutations (run), each fails: the MIDI 2.0 note-off not read (`(0x2,
/// 0x8)` only) → no release at 1000; MIDI 2.0 note-on not read → nothing
/// at 100; a MIDI 1.0 velocity 0 read as a note-on → an attack at 4000.
#[test]
fn a_note_envelope_reads_midi_2_and_velocity_0() {
    let plan = vec![
        (100, note_on_2(60, 0x8000)),
        (1_000, note_off_2(60)),
        (3_000, note_on_2(60, 0)),
        (4_000, [0x2090_3c00, 0, 0, 0]),
    ];
    let (mut ed, mut exec) = envelope_rig(128, plan, adsr(0.005, 0.005, 0.5, 0.01));
    let all = render_stopped(&mut ed, &mut exec, &[128], 6_000);
    assert_eq!(marks(&all), [100, 340, 1_000, 3_000, 3_240, 4_000]);
    assert_eq!(piece(&all, 100), (0.0, 1.0));
    assert_eq!(piece(&all, 340), (1.0, 0.5));
    assert_eq!(piece(&all, 1_000), (0.5, 0.0));
    assert_eq!(piece(&all, 3_000), (0.0, 1.0));
    assert_eq!(piece(&all, 3_240), (1.0, 0.5));
    assert_eq!(piece(&all, 4_000), (0.5, 0.0));
    assert_eq!(all[0][220], 0.5, "half way up the attack");
    assert_eq!(all[0][4_480], 0.0, "released");
}

/// **Zero-length stages are skipped on their frame, and the sustain is
/// clamped to 1.**
///
/// - A = D = R = 0, S = 0.7: a note-on at 100 is one step to 0.7 on its
///   frame (no attack to 1 first), held; its note-off at 500 one step to
///   0 on its frame.
/// - D = 0, S = 0 (attack 5 ms): the attack peaks at 340 and steps to 0
///   on that frame; the note-off at 1000 releases from 0, so it reads 0.
/// - S = 1.5 reads as 1: the decay holds at 1 and the level never passes
///   it.
///
/// Mutations (run), each fails: the zero-length skip dropped (a stage of
/// length 0 sent as a segment) → the first rig sends other segments
/// from 100; the clamp dropped (`s.min(1.0)` → `s`) → the third rises to 1.5.
#[test]
fn zero_length_stages_and_the_sustain_clamp() {
    let plan = vec![(100, note_on(60)), (500, note_off(60))];
    let (mut ed, mut exec) = envelope_rig(128, plan, adsr(0.0, 0.0, 0.7, 0.0));
    let a = render_stopped(&mut ed, &mut exec, &[128], 1_000);
    assert_eq!(marks(&a), [100, 500]);
    assert_eq!(piece(&a, 100), (0.7, 0.7), "straight to the sustain");
    assert!(a[0][..100].iter().all(|&v| v == 0.0));
    assert!(a[0][100..500].iter().all(|&v| v == 0.7), "held");
    assert_eq!(piece(&a, 500), (0.0, 0.0), "straight to 0");
    assert!(a[0][500..].iter().all(|&v| v == 0.0));

    let plan = vec![(100, note_on(60)), (1_000, note_off(60))];
    let (mut ed, mut exec) = envelope_rig(128, plan, adsr(0.005, 0.0, 0.0, 0.01));
    let b = render_stopped(&mut ed, &mut exec, &[128], 2_000);
    assert_eq!(&marks(&b)[..2], [100, 340]);
    assert_eq!(b[0][339], 239.0 / 240.0, "the attack up to its last frame");
    assert_eq!(piece(&b, 340), (0.0, 0.0), "the decay skipped to 0");
    assert!(
        b[0][340..].iter().all(|&v| v == 0.0),
        "the release is from 0"
    );

    let plan = vec![(100, note_on(60)), (2_000, note_off(60))];
    let (mut ed, mut exec) = envelope_rig(128, plan, adsr(0.005, 0.005, 1.5, 0.01));
    let c = render_stopped(&mut ed, &mut exec, &[128], 3_000);
    assert_eq!(piece(&c, 340), (1.0, 1.0), "the decay holds at 1");
    assert!(c[0][340..2_000].iter().all(|&v| v == 1.0));
    assert!(c[0].iter().all(|&v| v <= 1.0), "never past 1");
}

/// **A segment the event buffer refuses is re-sent, resumed.** 400 note
/// events in one 512-frame block (on at even frames, off at odd, from 0)
/// are 400 segments, past the capacity of 256: the envelope sends the
/// first 256 and nothing more that block. At the next block's first frame
/// (512) it re-sends where it stands, the release that began at 399
/// resumed 113 frames in, so from 512 on it reads bit for bit what the
/// same input reads in 1-frame blocks (where nothing is refused).
///
/// Mutation (run): the re-send dropped (`owed` never read) → no segment
/// at 512, and the tap holds frame 255's segment, not the release → fails.
#[test]
fn a_refused_segment_is_resent_resumed() {
    let plan: Vec<(u64, [u32; 4])> = (0..400u64)
        .map(|f| {
            (
                f,
                if f % 2 == 0 {
                    note_on(60)
                } else {
                    note_off(60)
                },
            )
        })
        .collect();
    let shape = adsr(0.02, 0.02, 0.5, 0.05);
    let (mut ed, mut exec) = envelope_rig(512, plan.clone(), shape);
    let big = render_stopped(&mut ed, &mut exec, &[512], 4_096);
    let (mut ed, mut exec) = envelope_rig(512, plan, shape);
    let one = render_stopped(&mut ed, &mut exec, &[1], 4_096);
    assert_eq!(
        marks(&big)[..256],
        (0..256).collect::<Vec<_>>()[..],
        "the first 256 segments"
    );
    assert_eq!(marks(&big)[256..], [512], "then the re-send");
    assert_eq!(marks(&one), (0..400).collect::<Vec<_>>(), "nothing refused");
    assert!(one[0][512] > 0.0, "the release is still in flight at 512");
    for f in 512..4_096 {
        assert_eq!(big[0][f].to_bits(), one[0][f].to_bits(), "frame {f}");
    }
}

/// **A long time constant decays at its own rate.** Full scale for 4800
/// frames (attack 0: followed exactly), then silence into a release of
/// 1000 s: 48 000 frames later the envelope is `exp(−48 000 / 48e6) =
/// exp(−0.001)`, within 1e-6. In `f32` the coefficient `exp(−1/48e6)`
/// rounds to 1, and the envelope would never decay.
///
/// Mutation (run): the coefficient rounded through `f32` (`as f32 as
/// f64`) → still 1.0 there → fails.
#[test]
fn a_long_release_decays_at_its_own_rate() {
    let (mut ed, mut exec) = editor(128);
    ed.insert(
        NodeKey(1),
        "pulse",
        ForkByClone(Pulse {
            width: ChannelLayout::MONO,
            on: vec![(0, 4_800, 1.0)],
        }),
    );
    ed.insert(
        NodeKey(2),
        "follower",
        ForkByClone(follower(1, 0.0, 1_000.0, ChannelLayout::MONO)),
    );
    wire_audio(&mut ed, 1, 2, 1);
    tap(&mut ed, 2, 3);
    ed.commit().expect("commits");
    let all = render_stopped(&mut ed, &mut exec, &[128], 53_000);
    assert_eq!(all[3][4_800], 1.0, "full scale at the burst's end");
    let g = 4_800 + 48_000;
    let want = (-0.001f64).exp();
    let got = f64::from(all[3][g]);
    assert!((got - want).abs() < 1e-6, "{got} vs {want}");
}
