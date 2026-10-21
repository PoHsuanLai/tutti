//! Compiler-owned param ports:
//! a node declares its params in its `Shape`, a spec drives each from at
//! most one event output (a control lane's, doc 014), and the compiler fuses
//! the source's ramps and segments, the base and the clamp into one step of
//! the node's op.
//!
//! What is pinned here, each with the mutation it was seen to fail under:
//!
//! - an unconnected param reads its base, never 0;
//! - an event source's value is clamped to the port's range, frame by frame;
//! - an event source replaces the base from its first event (a first ramp
//!   from the base); connecting a second one replaces the first;
//! - a base move under a source that has not spoken ramps across the block;
//! - connecting and disconnecting a source at runtime crossfades instead of
//!   stepping;
//! - a step or a `ParamRamp` lands on its frame;
//! - a node that cannot say its base is never driven;
//! - a unit's first block (an insert, a replace, a fork) is not declicked;
//! - an event source's held value survives an edit to its node's other
//!   ports, and a PDC delay appearing on (or growing on) its path;
//! - a fork whose source sends its value on its first block (as a control
//!   lane does) reads that value from frame 0;
//! - a curve segment (`ParamSegment`) hits its analytic value at every
//!   frame across blocks of any length, hands over on its frame when
//!   interrupted, and is clamped by a range without being rewritten;
//! - a NaN range is refused; removing a driven node leaves the graph
//!   committable;
//! - the executor and the reference interpreter agree bit for bit on random
//!   driven graphs (steps, ramps and curve segments among their sources),
//!   across recompiles, base moves, latency changes and regenerations.
//!
//! Audio-rate param sources, several sources summed on one port and their
//! shaping LUTs are gone (doc 014 §10, §11): a lane sums and shapes before it
//! sends, and audio-rate modulation is a node's own audio input. The
//! sample-accuracy contract row is `tests/contract.rs`'s `param_echo`, and
//! the allocation gate `tests/rt_no_alloc.rs`'s
//! `modulated_params_are_allocation_free`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use proptest::prelude::*;
use tutti_graph::{
    CommitError, Cx, Editor, Event, EventOut, Executor, ForkByClone, ForkMode, ForkTarget,
    GraphInvalid, Io, Node, ParamIn, ParamInput, ParamRamp, ParamRange, ParamSegment, Prepare,
    Reference, SegmentShape, Shape, Status, Transport, Unforkable, PARAM_DECLICK,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{
    ChannelLayout, Hz, Latency, NodeKey, ParamKey, SamplePosition, SampleRate, Samples, UnitParam,
    Q,
};

const RATE: SampleRate = SampleRate(48_000.0);

fn prepare(max: usize) -> Prepare {
    Prepare::new(RATE, Samples(max))
}

/// A base cell, as a node's `Param<U>` would hand it out.
#[derive(Clone, Default)]
struct Cell(Arc<AtomicU32>);

impl Cell {
    fn new(v: f32) -> Self {
        Self(Arc::new(AtomicU32::new(v.to_bits())))
    }
    fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Acquire))
    }
    fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Release);
    }
}

/// Declares `Cutoff` and `Q`. Writes param 0's value at every frame to
/// output 0 and param 1's to output 1 (a param reading its base writes the
/// base as it stands, one load per block), and on output 2 which params read
/// frames (bit `k`). Its one audio input is ignored: it is there for a PDC
/// sibling. Returns `Modified`, so it is never skipped.
#[derive(Clone)]
struct Probe {
    bases: [Cell; 2],
    /// Answer `param_base` (a node that cannot say is never driven).
    answers: bool,
    /// The params it declares, in port order (`Cutoff, Q` unless
    /// [`reversed`](Self::reversed)).
    declared: [UnitParam; 2],
}

impl Probe {
    fn new(cutoff: &Cell, q: &Cell) -> Self {
        Self {
            bases: [cutoff.clone(), q.clone()],
            answers: true,
            declared: [UnitParam::Cutoff, UnitParam::Q],
        }
    }

    /// Declaring `Q, Cutoff`: port 0 is `Q`, read from `q`, and written to
    /// output 0.
    fn reversed(cutoff: &Cell, q: &Cell) -> Self {
        Self {
            bases: [q.clone(), cutoff.clone()],
            answers: true,
            declared: [UnitParam::Q, UnitParam::Cutoff],
        }
    }
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::from_count(3)).with_params(&self.declared)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut framed = 0u32;
        for k in 0..2 {
            let p = io.param(k);
            match p {
                ParamInput::Base => {
                    let b = self.bases[k].get();
                    io.output(k).fill(b);
                }
                ParamInput::Frames(v) => {
                    io.output(k).copy_from_slice(v);
                    framed |= 1 << k;
                }
            }
        }
        io.output(2).fill(framed as f32);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (self.answers && port < 2).then(|| self.bases[port].get())
    }
}

/// A step of `param` to `value` (a zero-length ramp) at `offset`.
fn step(offset: tutti_graph::Offset, param: UnitParam, value: f32) -> Event {
    let ramp = match param {
        UnitParam::Q => ParamRamp::new(ParamKey::<Q>::Q, Q(value), Samples(0)),
        _ => ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(value), Samples(0)),
    };
    Event::ramp(offset, ramp)
}

/// A deterministic signal in `[-1, 1]` of the absolute frame, stepping
/// every frame (so a mistimed read shows).
fn signal_at(seed: u64, frame: u64) -> f32 {
    let h = frame.wrapping_add(1).wrapping_mul(seed | 1).rotate_left(17) ^ seed;
    (h % 201) as f32 / 100.0 - 1.0
}

/// A source stepping `Cutoff` and `Q` to `signal_at(seed, frame)` on every
/// frame, and writing the same signal to its one audio output (what a PDC
/// sibling is fed).
#[derive(Clone)]
struct Signal {
    seed: u64,
}

impl Node for Signal {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_events(0, 1)
            .with_event_capacity(256)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for i in 0..io.frames() {
            let v = signal_at(self.seed, start + i as u64);
            io.output(0)[i] = v;
            let o = io.offset(i).expect("inside");
            for p in [UnitParam::Cutoff, UnitParam::Q] {
                let _ = io.event_out(0).push(step(o, p, v));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A source stepping `Cutoff` and `Q` to `value` at the start of every block
/// (a constant a port hears whenever it starts listening).
#[derive(Clone)]
struct Hold {
    value: f32,
}

impl Node for Hold {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(4)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let o = io.offset(0).expect("a block has a frame");
        for p in [UnitParam::Cutoff, UnitParam::Q] {
            let _ = io.event_out(0).push(step(o, p, self.value));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A pure delay of `n` frames, declared as processing latency: the PDC
/// sibling.
struct Lag {
    n: usize,
    ring: Vec<f32>,
    pos: usize,
}

impl Lag {
    fn new(n: usize) -> Self {
        Self {
            n,
            ring: vec![0.0; n.max(1)],
            pos: 0,
        }
    }
}

impl Node for Lag {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(self.n)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..io.frames() {
            let x = io.input(0)[i];
            if self.n == 0 {
                io.output(0)[i] = x;
                continue;
            }
            io.output(0)[i] = self.ring[self.pos];
            self.ring[self.pos] = x;
            self.pos = (self.pos + 1) % self.n;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
    }
}

/// Sends one `ParamRamp` per `(absolute frame, param, target, length)`, and
/// each `(absolute frame, segment)`, on its frame, in frame order whatever
/// the plans' order.
#[derive(Clone)]
struct Ramps {
    plan: Vec<(u64, UnitParam, f32, usize)>,
    /// Curve segments, each on its absolute frame (after the frame's
    /// ramps).
    segs: Vec<(u64, ParamSegment)>,
}

impl Ramps {
    /// Steps (zero-length ramps) only.
    fn steps(steps: &[(u64, UnitParam, f32)]) -> Self {
        Self {
            plan: steps.iter().map(|&(at, p, v)| (at, p, v, 0)).collect(),
            segs: Vec::new(),
        }
    }
}

impl Node for Ramps {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(64)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        let inside = |at: u64| at >= start && at < start + io.frames() as u64;
        let mut events: Vec<Event> = Vec::new();
        for &(at, param, target, len) in &self.plan {
            if !inside(at) {
                continue;
            }
            let ramp = match param {
                UnitParam::Q => ParamRamp::new(ParamKey::<Q>::Q, Q(target), Samples(len)),
                _ => ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(target), Samples(len)),
            };
            let offset = io.offset((at - start) as usize).expect("inside");
            events.push(Event::ramp(offset, ramp));
        }
        for &(at, seg) in &self.segs {
            if inside(at) {
                let offset = io.offset((at - start) as usize).expect("inside");
                events.push(Event::segment(offset, seg));
            }
        }
        // In frame order (stably: on one frame, ramps before segments, each
        // in plan order), as an event output must be written.
        events.sort_by_key(|e| e.offset);
        for e in events {
            let _ = io.event_out(0).push(e);
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn port(node: u64, port: u16) -> OutPort {
    OutPort {
        node: NodeKey(node),
        port,
    }
}

/// Event output 0 of `node`.
fn ev(node: u64) -> EventOut {
    EventOut {
        node: NodeKey(node),
        port: 0,
    }
}

fn param(node: u64, p: UnitParam) -> ParamIn {
    ParamIn {
        node: NodeKey(node),
        param: p,
    }
}

const PROBE: u64 = 10;

/// An editor with a `Probe` at [`PROBE`] whose three outputs are the graph's.
fn probe_graph(max: usize, cutoff: &Cell, q: &Cell) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(prepare(max));
    ed.insert(NodeKey(PROBE), "probe", Unforkable(Probe::new(cutoff, q)));
    ed.spec_mut().topology.outputs = (0..3).map(|c| Source::Node(port(PROBE, c))).collect();
    (ed, exec)
}

/// Render `frames` in blocks of `block`, returning the three outputs.
fn render(exec: &mut Executor, ed: &mut Editor, frames: usize, block: usize) -> [Vec<f32>; 3] {
    let mut out: [Vec<f32>; 3] = Default::default();
    let mut done = 0;
    while done < frames {
        let n = block.min(frames - done);
        let mut b = [vec![0.0; n], vec![0.0; n], vec![0.0; n]];
        {
            let [a, c, d] = &mut b;
            exec.process(
                n,
                &Transport::default(),
                &[],
                &mut [&mut a[..], &mut c[..], &mut d[..]],
            );
        }
        ed.collect();
        for (o, x) in out.iter_mut().zip(b) {
            o.extend(x);
        }
        done += n;
    }
    out
}

/// An unconnected param reads its base: `Io::param` says `Base`, and the
/// node's own control is what sounds — never 0. A range with no source is
/// still no modulation.
///
/// Mutation (run): in `ParamState::inputs`, hand every port its buffer
/// (`if true` for `if self.framed[k]`) → the probe reads the scratch's
/// zeros and flags frames → fails.
#[test]
fn an_unconnected_param_reads_its_base_never_zero() {
    let (cutoff, q) = (Cell::new(440.0), Cell::new(0.7));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.spec_mut()
        .set_param_range(param(PROBE, UnitParam::Cutoff), ParamRange::new(0.0, 100.0));
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 300, 64);
    assert!(out[0].iter().all(|&x| x == 440.0), "the base, unclamped");
    assert!(out[1].iter().all(|&x| x == 0.7));
    assert!(out[2].iter().all(|&x| x == 0.0), "no param read frames");
}

/// An event source's value reaches the param sample for sample, from the
/// port's first frame, and is clamped to the port's range: an unbounded
/// port reads the source exactly, a bounded one reads it clamped.
///
/// Ported from `modulators_sum_on_the_base_then_clamp`:
/// the base, several audio sources summed in source order and a shaping LUT
/// are gone (§10, §11: one event source per port replaces the base; a lane
/// sums and shapes before it sends). The clamp and the per-frame timing are
/// the properties kept.
///
/// Mutation (run): in `ParamState::port`, skip the clamp → the Q port reads
/// past 0.5 → fails. Write the source one frame late (`render_ramps` starts
/// events at `offset < i`) → frame 0 reads the base → fails.
#[test]
fn an_event_sources_value_is_clamped_to_the_range() {
    let (cutoff, q) = (Cell::new(100.0), Cell::new(1.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: 7 }));
    {
        let s = ed.spec_mut();
        s.connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
        s.connect_param(param(PROBE, UnitParam::Q), ev(1));
        s.set_param_range(param(PROBE, UnitParam::Q), ParamRange::new(-0.5, 0.5));
    }
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 500, 64);
    #[allow(clippy::needless_range_loop, reason = "three outputs at one frame")]
    for i in 0..500 {
        let a = signal_at(7, i as u64);
        assert_eq!(out[0][i], a, "frame {i}: the source, unbounded");
        assert_eq!(out[1][i], a.clamp(-0.5, 0.5), "frame {i}: clamped");
        assert_eq!(out[2][i], 3.0, "both params read frames");
    }
    assert!(
        out[1].contains(&0.5) && out[1].contains(&-0.5),
        "the clamp was exercised at both ends"
    );
}

/// A base moved under a source that has not spoken yet ramps linearly
/// across the next block, landing exactly on the new base at its last
/// frame — no zipper step.
///
/// Mutation (run): in `ParamState::port`, ramp from the new base (`from =
/// b1`) → the block starts at the new base → fails.
#[test]
fn a_base_move_under_modulation_ramps_across_the_block() {
    let (cutoff, q) = (Cell::new(100.0), Cell::new(1.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "quiet", Unforkable(Ramps::steps(&[])));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 512, 64);
    cutoff.set(200.0);
    let out = render(&mut exec, &mut ed, 64, 64);
    for (i, &x) in out[0].iter().enumerate() {
        let want = if i == 63 {
            200.0
        } else {
            100.0 + 100.0 * ((i + 1) as f32 / 64.0)
        };
        assert_eq!(x, want, "frame {i} of the ramp");
    }
}

/// Connecting a source at full swing, and disconnecting it, crossfade over
/// `PARAM_DECLICK` frames: no frame-to-frame step larger than the fade's
/// slope, and a disconnected param is back on its base (`Base`) once the
/// fade is over.
///
/// Mutation (run): in `ParamState::port`, skip the declick (`fading =
/// false`) → a step of 1.0 on connect → fails. Hold nothing on disconnect
/// (return at once when `source.is_none()`) → a step of 1.0 back → fails.
#[test]
fn connecting_and_disconnecting_at_runtime_does_not_click() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "one", Unforkable(Hold { value: 1.0 }));
    ed.commit().expect("commits");
    let mut all = render(&mut exec, &mut ed, 128, 64)[0].clone();

    let at = param(PROBE, UnitParam::Cutoff);
    ed.spec_mut().connect_param(at, ev(1));
    ed.commit().expect("connects");
    let on = render(&mut exec, &mut ed, 1024, 64);
    assert_eq!(*on[0].last().expect("rendered"), 1.0, "fully driven");
    all.extend(&on[0]);

    assert!(ed.spec_mut().disconnect_param(at, ev(1)));
    ed.commit().expect("disconnects");
    let off = render(&mut exec, &mut ed, 1024, 64);
    assert_eq!(*off[0].last().expect("rendered"), 0.0, "back on the base");
    assert_eq!(*off[2].last().expect("rendered"), 0.0, "reading Base again");
    all.extend(&off[0]);

    let slope = 1.0 / PARAM_DECLICK.get() as f32 + 1e-6;
    for (i, w) in all.windows(2).enumerate() {
        assert!(
            (w[1] - w[0]).abs() <= slope,
            "a step of {} at frame {}: a click",
            w[1] - w[0],
            i + 1
        );
    }
}

/// A step lands on its frame: a zero-length `ParamRamp` at frame `k`
/// reaches the param at `k` exactly, under block sizes that put `k`
/// anywhere in its block; a ramp of `len` frames lands on its target at
/// `k + len - 1`.
///
/// Mutation (run): in `render_ramps`, start a ramp on the frame after its
/// event (`offset < i`) → the step cases fail. In `Ramp::step`,
/// interpolate one frame behind (`(done - 1) / len`) → the ramp's first
/// step fails.
#[test]
fn a_modulation_step_lands_on_its_frame() {
    for block in [1usize, 7, 63, 64, 65, 128] {
        for k in [300u64, 301, 383, 384, 400] {
            let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
            let (mut ed, mut exec) = probe_graph(128, &cutoff, &q);
            ed.insert(
                NodeKey(2),
                "ramps",
                Unforkable(Ramps {
                    plan: vec![
                        (k, UnitParam::Cutoff, 2.0, 0),
                        (k, UnitParam::Q, 3.0, 0),
                        (k + 20, UnitParam::Q, 5.0, 8),
                    ],
                    segs: Vec::new(),
                }),
            );
            {
                let s = ed.spec_mut();
                s.connect_param(param(PROBE, UnitParam::Cutoff), ev(2));
                s.connect_param(param(PROBE, UnitParam::Q), ev(2));
            }
            ed.commit().expect("commits");
            let out = render(&mut exec, &mut ed, 512, block);
            let k = k as usize;
            let ctx = format!("block {block}, step at {k}");
            assert_eq!(out[0][k - 1], 0.0, "{ctx}: before the step");
            assert_eq!(out[0][k], 2.0, "{ctx}: the step on its frame");
            assert_eq!(out[1][k - 1], 0.0, "{ctx}: before the ramp");
            assert_eq!(out[1][k], 3.0, "{ctx}: a zero-length ramp on its frame");
            assert_eq!(
                out[1][k + 20],
                3.0 + 2.0 / 8.0,
                "{ctx}: the ramp's first step"
            );
            assert!(out[1][k + 26] < 5.0, "{ctx}: not there early");
            assert_eq!(out[1][k + 27], 5.0, "{ctx}: on target on its last frame");
        }
    }
}

/// A node that cannot say its base is never driven, whatever is connected:
/// it keeps reading `Base`, so a param is never driven from a base of 0.
///
/// Mutation (run): in `ParamState::port`, treat a `None` base as 0 → the
/// probe reads frames → fails.
#[test]
fn a_node_without_a_base_is_never_modulated() {
    let (cutoff, q) = (Cell::new(440.0), Cell::new(1.0));
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let mut probe = Probe::new(&cutoff, &q);
    probe.answers = false;
    ed.insert(NodeKey(PROBE), "probe", Unforkable(probe));
    ed.spec_mut().topology.outputs = (0..3).map(|c| Source::Node(port(PROBE, c))).collect();
    ed.insert(NodeKey(1), "one", Unforkable(Hold { value: 1.0 }));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 256, 64);
    assert!(out[0].iter().all(|&x| x == 440.0));
    assert!(out[2].iter().all(|&x| x == 0.0));
}

/// A param the node does not declare, or a source port the node does not
/// have, is refused at compile, naming it.
///
/// Mutation (run): drop the `from.port >= shape.event_out` check from
/// `compile` → the second commit is accepted → fails.
#[test]
fn undeclared_params_and_missing_sources_are_refused() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, _exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: 1 }));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Drive), ev(1));
    let err = ed.commit().expect_err("Drive is not declared");
    assert!(format!("{err}").contains("Drive"), "{err}");
    ed.spec_mut().params.clear();
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        EventOut {
            node: NodeKey(1),
            port: 3,
        },
    );
    assert!(
        matches!(
            ed.commit(),
            Err(CommitError::Compile(
                tutti_graph::CompileError::ParamSourceOutOfRange { .. }
            ))
        ),
        "the signal has no event output 3"
    );
}

/// Removing a node drops every param edge that names it: the ports it
/// drove lose their source, and the ports other nodes drive keep theirs —
/// so the value never names a node the graph no longer holds.
///
/// Mutation (run): in `Editor::remove`, keep sources from the removed key
/// (drop the `m.source = None`) → the commit is refused as naming an
/// unknown node → fails.
#[test]
fn removing_a_node_drops_its_param_edges() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "a", Unforkable(Signal { seed: 3 }));
    ed.insert(NodeKey(2), "b", Unforkable(Signal { seed: 5 }));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Q), ev(2));
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 64, 64);

    ed.remove(NodeKey(1));
    let params = &ed.spec().params;
    assert_eq!(
        params[&param(PROBE, UnitParam::Cutoff)].source,
        None,
        "the removed node's source is gone"
    );
    assert_eq!(
        params[&param(PROBE, UnitParam::Q)].source,
        Some(ev(2)),
        "the other stays"
    );
    ed.commit().expect("the value names no removed node");
    // Past the cutoff's declick back to its base.
    let out = render(&mut exec, &mut ed, PARAM_DECLICK.get() + 64, 64);
    assert_eq!(*out[2].last().expect("rendered"), 2.0, "Q is still driven");
}

/// A crossfade keeps its key's param state: the two units must declare the
/// same params, and one that declares others is refused (`FadeShape`), as a
/// fade between other ports is. A fade between two that agree keeps the
/// held value through it, with no declick (the source did not change): the
/// source spoke once, at frame 0, so a unit that faded in with fresh state
/// would read its base.
///
/// Mutation (run): drop `shape.params == running.params` from
/// `Editor::replace`'s check → the replace is accepted (and the fade's
/// incoming unit runs on state sized for other params) → fails.
#[test]
fn a_fade_keeps_the_params_it_declares() {
    struct CutoffOnly;
    impl Node for CutoffOnly {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::MONO, ChannelLayout::from_count(3))
                .with_params(&[UnitParam::Cutoff])
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
            Status::Silent
        }
        fn reset(&mut self) {}
    }

    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(
        NodeKey(1),
        "once",
        Unforkable(Ramps::steps(&[(0, UnitParam::Cutoff, 1.0)])),
    );
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 512, 64);

    let fade = tutti_graph::Fade::new(Samples(128), tutti_graph::CrossfadeCurve::EqualAmplitude);
    assert!(
        matches!(
            ed.replace(NodeKey(PROBE), Unforkable(CutoffOnly), fade),
            Err(tutti_graph::CommitError::FadeShape { .. })
        ),
        "a unit declaring other params cannot fade in"
    );
    let (c2, q2) = (Cell::new(0.0), Cell::new(0.0));
    ed.replace(NodeKey(PROBE), Unforkable(Probe::new(&c2, &q2)), fade)
        .expect("the same params fade");
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 256, 64);
    assert!(
        out[0].iter().all(|&x| x == 1.0),
        "held all through the fade, no declick: {:?}",
        &out[0][..8]
    );
}

/// A unit's first block is not a change of sources: a probe inserted with a
/// source already connected, and one hard-replaced at its key, read the
/// source's value from their first frame — no fade in from the base.
///
/// Mutation (run): in `ParamState::port`, treat a fresh port as a change
/// (drop the `st.fresh` branch) → the first frames fade in from the base →
/// fails.
#[test]
fn a_units_first_block_is_not_declicked() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "one", Unforkable(Hold { value: 1.0 }));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 128, 64);
    assert!(
        out[0].iter().all(|&x| x == 1.0),
        "driven from the first frame: {:?}",
        &out[0][..4]
    );
    // A new generation at the key: a new unit, so a first block again.
    ed.insert(NodeKey(PROBE), "probe", Unforkable(Probe::new(&cutoff, &q)));
    ed.commit().expect("replaces");
    let out = render(&mut exec, &mut ed, 128, 64);
    assert!(out[0].iter().all(|&x| x == 1.0), "{:?}", &out[0][..4]);
}

/// An event source's held value belongs to the port, not to the plan: a
/// port holding +500 from a one-off step stays at 500 through a commit that
/// changes its node's other port (Q gains a source), and through one that
/// changes nothing of it (a range on Q).
///
/// Ported from
/// `an_event_sources_held_value_survives_another_source_joining`: a second
/// source can no longer join a port (one source, replacing the base), and
/// shaping is gone (§10), so the reshape refusal went with it. The edit
/// now changes the node's port list instead.
///
/// Mutation (run): in `ParamState::port`, reset every port's ramp when
/// any one port's source changes (a port-wide reset where only the
/// changed port's is due) → the cutoff drops to its base (0) → fails.
#[test]
fn an_event_sources_held_value_survives_an_edit_to_its_node() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(
        NodeKey(2),
        "once",
        Unforkable(Ramps::steps(&[(10, UnitParam::Cutoff, 500.0)])),
    );
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: 9 }));
    let at = param(PROBE, UnitParam::Cutoff);
    ed.spec_mut().connect_param(at, ev(2));
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 512, 64);
    assert_eq!(out[0][511], 500.0, "holding the step's value");

    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Q), ev(1));
    ed.commit().expect("Q gains a source");
    let out = render(&mut exec, &mut ed, 512, 64);
    assert!(
        out[0].iter().all(|&x| x == 500.0),
        "still 500 with Q driven beside it: {:?}",
        out[0].iter().find(|&&x| x != 500.0)
    );

    ed.spec_mut()
        .set_param_range(param(PROBE, UnitParam::Q), ParamRange::new(-0.5, 0.5));
    ed.commit().expect("a range on Q");
    let out = render(&mut exec, &mut ed, 512, 64);
    assert!(out[0].iter().all(|&x| x == 500.0), "the port holds");
}

/// The port rule (doc 014 §5.6): an event source **replaces** the base.
/// A knob at 1000 with a step to 500 at frame 10 reads 1000 until frame
/// 10 and exactly 500.0 from it on (automation replaces the knob); a knob
/// move under it changes nothing; a first ramp starts from the base at its
/// frame; and a disconnect hands the port back to the base through the
/// declick.
///
/// Mutation (run): in `ParamState::port`, add the base under a started
/// event source (`out[i] + b1`, the old offset rule) → frame 10 reads 1500
/// → fails. Mutation (run): in `render_ramps`, write 0 before the first
/// event instead of the base → frame 0 reads 0 → fails. Mutation (run): in
/// `render_ramps`, leave a first ramp's start at 0 (skip `ramp.value =
/// base(i)`) → the Q ramp's first frame reads 9.99 → fails.
#[test]
fn an_event_source_replaces_the_base_from_its_first_event() {
    let (cutoff, q) = (Cell::new(1000.0), Cell::new(2000.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(
        NodeKey(2),
        "ramps",
        Unforkable(Ramps {
            // A step to 500 at frame 10; a 100-frame ramp to 1000 from frame
            // 20 on the other param, whose base is 2000.
            plan: vec![
                (10, UnitParam::Cutoff, 500.0, 0),
                (20, UnitParam::Q, 1000.0, 100),
            ],
            segs: Vec::new(),
        }),
    );
    for p in [UnitParam::Cutoff, UnitParam::Q] {
        ed.spec_mut().connect_param(param(PROBE, p), ev(2));
    }
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 128, 64);
    assert!(
        out[0][..10].iter().all(|&x| x == 1000.0),
        "the knob, before"
    );
    assert!(
        out[0][10..].iter().all(|&x| x == 500.0),
        "the automation, not knob + automation: {}",
        out[0][10]
    );
    assert!(out[1][..20].iter().all(|&x| x == 2000.0));
    assert_eq!(out[1][20], 2000.0 + (1000.0 - 2000.0) * (1.0 / 100.0));
    assert_eq!(out[1][119], 1000.0, "landed");
    cutoff.set(3000.0);
    let out = render(&mut exec, &mut ed, 64, 64);
    assert!(
        out[0].iter().all(|&x| x == 500.0),
        "a knob move is overridden"
    );

    assert!(ed
        .spec_mut()
        .disconnect_param(param(PROBE, UnitParam::Cutoff), ev(2)));
    ed.commit().expect("disconnects");
    let d = PARAM_DECLICK.get();
    let out = render(&mut exec, &mut ed, 2 * d, 64);
    // From 500 to the knob (3000) through the declick: frame `j` of the
    // fade is `3000 + (500 - 3000) · (1 - (j + 1) / d)`.
    let g = 1.0 - 1.0 / d as f32;
    assert_eq!(out[0][0], 3000.0 + (500.0 - 3000.0) * g);
    assert!(out[0][d..].iter().all(|&x| x == 3000.0), "back on the knob");
}

/// A port takes one event source: connecting a second replaces the first
/// (and hands it back), the port follows the new one after the declick,
/// and disconnecting the one no longer there is a no-op.
///
/// Ported from `a_second_event_source_is_refused`: the
/// refusal (`TooManyEventParamSources`) is gone because a port's source is
/// one `Option<EventOut>`, so two cannot be represented (§10).
///
/// Mutation (run): `GraphSpec::connect_param` keeping the first source
/// (`get_or_insert`) → the port still follows node 2 → fails.
#[test]
fn a_second_event_source_replaces_the_first() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(2), "two", Unforkable(Hold { value: 2.0 }));
    ed.insert(NodeKey(3), "three", Unforkable(Hold { value: 3.0 }));
    let at = param(PROBE, UnitParam::Cutoff);
    assert_eq!(ed.spec_mut().connect_param(at, ev(2)), None);
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 64, 64);
    assert!(out[0].iter().all(|&x| x == 2.0));

    assert_eq!(
        ed.spec_mut().connect_param(at, ev(3)),
        Some(ev(2)),
        "the first is handed back"
    );
    assert!(
        !ed.spec_mut().disconnect_param(at, ev(2)),
        "node 2 no longer drives the port"
    );
    assert_eq!(ed.spec().params[&at].source, Some(ev(3)));
    ed.commit().expect("one source");
    let out = render(&mut exec, &mut ed, PARAM_DECLICK.get() + 64, 64);
    assert_eq!(*out[0].last().expect("rendered"), 3.0, "the new source");
}

/// A PDC delay that appears on a param source's path, or grows, never
/// drops the port to its base: a probe held at +1 by a one-off step stays
/// at +1 when a 100-frame sibling moves its arrival (a new delay on the
/// source's events), and again when the sibling grows to 150 (the delay
/// retuned longer). The held value is the port's, not the delay line's.
///
/// Ported: the source was an audio step whose delay ring
/// had to be filled with its last value; audio sources are gone (§10), and
/// an event delay carries only events, so the property is now that the
/// port keeps what it holds.
///
/// Mutation (run): in `compile`, never lower a param source's delay
/// (`if d.is_zero()` → `if true`: no `ParamEvent` delay) → the delay
/// assertion fails.
#[test]
fn a_delay_appearing_on_a_param_source_holds_its_last_value() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(
        NodeKey(1),
        "once",
        Unforkable(Ramps::steps(&[(0, UnitParam::Cutoff, 1.0)])),
    );
    ed.insert(NodeKey(4), "feed", Unforkable(Signal { seed: 2 }));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 256, 64);

    for lag in [100, 150] {
        ed.insert(NodeKey(3), "lag", Unforkable(Lag::new(lag)));
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(3),
                port: 0,
            },
            Edge::Direct(Source::Node(port(4, 0))),
        );
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(PROBE),
                port: 0,
            },
            Edge::Direct(Source::Node(port(3, 0))),
        );
        ed.commit().expect("the sibling moves the arrival");
        let delay = ed
            .base()
            .expect("a plan")
            .delays()
            .iter()
            .find(|d| matches!(d.key, tutti_graph::DelayKey::ParamEvent { .. }))
            .map(|d| d.len);
        assert_eq!(delay, Some(Samples(lag)), "the param source is delayed");
        let out = render(&mut exec, &mut ed, 512, 64);
        assert!(
            out[0].iter().all(|&x| x == 1.0),
            "lag {lag}: a dropout at frame {:?}",
            out[0].iter().position(|&x| x != 1.0)
        );
    }
}

/// A source of [`Hold`]'s steps that declares processing latency: its
/// events are about frames `latency` earlier than the block's.
struct LatentHold {
    value: f32,
    latency: usize,
}

impl Node for LatentHold {
    fn shape(&self) -> Shape {
        Hold { value: self.value }
            .shape()
            .with_latency(Latency::new(Samples(self.latency)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        Hold { value: self.value }.process(cx, io)
    }
    fn reset(&mut self) {}
}

/// A param source counts toward its target's arrival in both solves: a
/// latent source holds its target back by its latency, so the compiler's
/// total latency is the source's and the source's events are not delayed,
/// and `latency::plan` over the whole spec (`GraphSpec`'s `LatencyGraph`,
/// what a latency readout folds) reports the same total.
///
/// Added in doc 014 §12.6: the differential's reference shares the
/// compiler's arrival, so nothing pinned the spec's fold of param sources
/// (`compile_passes.rs` pins its event sources).
///
/// Mutation (run): in `GraphSpec`'s `other_sources`, leave param sources
/// out → the spec's plan totals 0 against the compiler's 37 → fails. In
/// `compile`, leave a param source out of its target's `preds` → the
/// compiler's total is 0 → fails.
#[test]
fn a_param_source_counts_toward_arrival_in_both_solves() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(
        NodeKey(1),
        "latent",
        Unforkable(LatentHold {
            value: 2.0,
            latency: 37,
        }),
    );
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.commit().expect("commits");
    let plan = ed.base().expect("a plan");
    assert_eq!(plan.total_latency(), Latency::new(Samples(37)));
    assert_eq!(
        tutti_types::latency::plan(ed.spec()).total(),
        Samples(37),
        "the spec's own fold agrees"
    );
    assert!(
        !plan
            .delays()
            .iter()
            .any(|d| matches!(d.key, tutti_graph::DelayKey::ParamEvent { .. })),
        "the target waits for the source; the source is not delayed"
    );
    let out = render(&mut exec, &mut ed, 128, 64);
    assert_eq!(out[0][127], 2.0, "the source drives the port");
}

/// A stand-in for a control lane: a source that sends its value (a step on
/// `Cutoff` and on `Q`) on its first block after `prepare`, and on no other.
/// A fork prepares its copy, so a forked one speaks at the fork's frame 0:
/// the fork requirement (doc 014 §10).
#[derive(Clone)]
struct OnFirstBlock {
    cutoff: f32,
    q: f32,
    pending: bool,
}

impl Node for OnFirstBlock {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(4)
    }
    fn prepare(&mut self, _: &Prepare) {
        self.pending = true;
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if std::mem::take(&mut self.pending) {
            let o = io.offset(0).expect("a block has a frame");
            let _ = io
                .event_out(0)
                .push(step(o, UnitParam::Cutoff, self.cutoff));
            let _ = io.event_out(0).push(step(o, UnitParam::Q, self.q));
        }
        Status::Silent
    }
    fn reset(&mut self) {
        self.pending = true;
    }
}

/// A lane-fed fork reads its value at frame 0: the live graph's ports hold
/// what their source sent once, long ago; the fork's source is a fresh,
/// prepared copy that sends its value on its first block, and the fork's
/// ports (fresh too) take it from their first frame, undeclicked.
///
/// Ported from `a_fork_carries_its_event_sources_ramps`:
/// the fork no longer seeds its ports from a tap of the live ones (the
/// seeds are gone, §10); a fresh source speaks instead. The lane itself is
/// held to this in `tutti-nodes` (`ControlLaneNode`'s fork test).
///
/// Mutations (run): in `ParamState::port`, treat a fresh port as a change
/// (drop the `st.fresh` branch) → the fork's first frames fade in from the
/// base → fails. A source that does not speak on the fork's first block
/// (`OnFirstBlock` cloned with `pending: false`, re-armed by neither
/// `prepare` nor `reset`) → the fork's ports read their bases (0) → fails.
#[test]
fn a_lane_fed_fork_reads_its_value_at_frame_0() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(
        NodeKey(PROBE),
        "probe",
        ForkByClone(Probe::new(&cutoff, &q)),
    );
    ed.spec_mut().topology.outputs = (0..3).map(|c| Source::Node(port(PROBE, c))).collect();
    ed.insert(
        NodeKey(2),
        "lane",
        ForkByClone(OnFirstBlock {
            cutoff: 500.0,
            q: 3.0,
            pending: true,
        }),
    );
    for p in [UnitParam::Cutoff, UnitParam::Q] {
        ed.spec_mut().connect_param(param(PROBE, p), ev(2));
    }
    ed.commit().expect("commits");
    let live = render(&mut exec, &mut ed, 600, 64);
    assert_eq!((live[0][599], live[1][599]), (500.0, 3.0), "live, held");

    let (mut fed, mut fexec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    let out = render(&mut fexec, &mut fed, 10, 10);
    #[allow(clippy::needless_range_loop, reason = "two outputs at one frame")]
    for i in 0..10 {
        assert_eq!(out[0][i], 500.0, "frame {i}: the cutoff, from frame 0");
        assert_eq!(out[1][i], 3.0, "frame {i}: the Q, from frame 0");
    }
}

/// A NaN bound is refused when the spec is validated, naming the port —
/// never handed to the audio thread, where `f32::clamp` panics on it.
///
/// Mutation (run): drop the NaN check from `GraphSpec::validate` → the
/// commit is accepted → fails.
#[test]
fn a_nan_range_is_refused() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, _exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: 1 }));
    let at = param(PROBE, UnitParam::Cutoff);
    ed.spec_mut().connect_param(at, ev(1));
    for range in [
        ParamRange::new(f32::NAN, 1.0),
        ParamRange::new(0.0, f32::NAN),
    ] {
        ed.spec_mut().set_param_range(at, range);
        match ed.commit() {
            Err(CommitError::Invalid(errs)) => assert!(
                errs.iter()
                    .any(|e| matches!(e, GraphInvalid::BadParamRange { at: a, .. } if *a == at)),
                "{errs:?}"
            ),
            other => panic!("a NaN range was not refused: {other:?}"),
        }
    }
    ed.spec_mut()
        .set_param_range(at, ParamRange::new(f32::NEG_INFINITY, 1.0));
    ed.commit().expect("an infinite bound is no bound");
}

/// Removing a driven node — the target of a param edge — takes its ports'
/// entries with it, so the next commit (and every one after) goes through
/// instead of naming a node that is gone; and the executor hands the
/// retired unit's param state back rather than freeing it on the audio
/// thread.
///
/// Mutation (run): in `Editor::remove`, keep the target's entries (drop
/// the `at.node != key` half of the retain) → the commit is refused with
/// `UnknownParamNode` → fails. In `Executor::apply`, drop a retired unit's
/// param state instead of pushing it to `spare_params` → `ParamState`'s
/// drop check panics on the audio thread (a debug build) → fails.
#[test]
fn removing_a_modulated_target_leaves_the_graph_committable() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: 3 }));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.spec_mut()
        .set_param_range(param(PROBE, UnitParam::Q), ParamRange::new(0.0, 1.0));
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 64, 64);

    ed.remove(NodeKey(PROBE));
    assert!(
        ed.spec().params.keys().all(|at| at.node != NodeKey(PROBE)),
        "no entry names the removed target"
    );
    ed.commit().expect("the graph without the target commits");
    let _ = render(&mut exec, &mut ed, 64, 64);
    ed.insert(NodeKey(2), "sig", Unforkable(Signal { seed: 4 }));
    ed.commit().expect("and so does the next edit");
}

/// A re-prepare checks every unit out; a driven unit's param state comes
/// back with it in the box, freed on the control side, and the resumed
/// unit is driven again from its first block.
///
/// Mutation (run): in `Executor::apply`'s suspend, drop the checked-out
/// unit's param state instead of pushing it to `spare_params` →
/// `ParamState`'s drop check panics on the audio thread → fails.
#[test]
fn a_reprepare_hands_param_state_back() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "one", Unforkable(Hold { value: 1.0 }));
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 128, 64);
    ed.reprepare(prepare(128)).expect("reprepares");
    exec.apply_pending();
    ed.collect();
    exec.apply_pending();
    ed.collect();
    let out = render(&mut exec, &mut ed, 256, 128);
    assert!(out[0].iter().all(|&x| x == 1.0), "{:?}", &out[0][..4]);
}

// ---- curve segments ---------------------------------------------------------

/// The block schedules a segment test renders under: one frame, odd and
/// even fixed sizes, the maximum, and a ragged one.
fn segment_schedules(total: usize) -> Vec<Vec<usize>> {
    let fixed = |n: usize| {
        let mut v = vec![n; total / n];
        if !total.is_multiple_of(n) {
            v.push(total % n);
        }
        v
    };
    let mut ragged = Vec::new();
    let (mut sum, mut i) = (0, 0usize);
    while sum < total {
        let n = [1, 128, 7, 64, 127, 33, 2][i % 7].min(total - sum);
        ragged.push(n);
        sum += n;
        i += 1;
    }
    vec![fixed(1), fixed(7), fixed(64), fixed(65), fixed(128), ragged]
}

/// A probe whose `Cutoff` (base 0) is driven by one `Ramps` source sending
/// `plan` and `segs`, rendered under `blocks`: the cutoff per frame.
fn render_segments(
    plan: Vec<(u64, UnitParam, f32, usize)>,
    segs: Vec<(u64, ParamSegment)>,
    blocks: &[usize],
) -> Vec<f32> {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(128, &cutoff, &q);
    ed.insert(NodeKey(1), "ramps", Unforkable(Ramps { plan, segs }));
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        EventOut {
            node: NodeKey(1),
            port: 0,
        },
    );
    ed.commit().expect("commits");
    let mut out = Vec::new();
    for &n in blocks {
        out.extend(render(&mut exec, &mut ed, n, n)[0].iter().copied());
    }
    out
}

/// A segment's analytic value `j` frames after its event, in `f64`, from
/// the shapes' closed forms written out here (not `SegmentShape::eval`).
fn analytic(from: f64, to: f64, shape: SegmentShape, position: f64, length: f64, j: u64) -> f64 {
    let t = (position + j as f64) / length;
    if t >= 1.0 || t.is_nan() {
        return to;
    }
    let g = match shape {
        SegmentShape::Linear => t,
        SegmentShape::Hold => 0.0,
        SegmentShape::PowerIn(p) => t.powf(f64::from(p)),
        SegmentShape::PowerOut(p) => 1.0 - (1.0 - t).powf(f64::from(p)),
        SegmentShape::PowerInOut(p) if t < 0.5 => (2.0 * t).powf(f64::from(p)) / 2.0,
        SegmentShape::PowerInOut(p) => 1.0 - (2.0 - 2.0 * t).powf(f64::from(p)) / 2.0,
        SegmentShape::Exp(k) => {
            let k = f64::from(k);
            ((k * t).exp() - 1.0) / (k.exp() - 1.0)
        }
    };
    from + (to - from) * g
}

/// A curve segment hits its analytic value at every frame — within a
/// block, across block boundaries (one spans eight 128-frame blocks), under
/// one-frame, odd, even and ragged blocks — and holds its end value once
/// there. Three shapes: an integer power (IEEE-exact), an exponential
/// (libm: hence a tolerance rather than bits), and an ease-out resumed at a
/// fractional position along a fractional length (a breakpoint between
/// frames).
///
/// Mutations (run), each fails: `SegmentShape::eval` returning `t` for
/// `PowerIn` (linearised: frame 599 reads 0.5 for 0.25). In `Ramp::step`,
/// not advancing a segment's `done` (every frame at its event frame's
/// progress). In `ParamState::port`, holding an event source's first frame
/// for the whole block (`buf.fill(buf[0])` after `render_ramps`: evaluated
/// at block starts only). In `render_ramps`, starting events one frame
/// late (`offset < i`).
#[test]
fn a_segment_hits_its_analytic_values_per_frame() {
    const TOL: f64 = 1e-5;
    let key = ParamKey::<Hz>::CUTOFF;
    let a = (SegmentShape::PowerIn(2.0), 0.0, 1.0, 1000usize);
    let b = (SegmentShape::Exp(4.0), 2.0, -1.0, 300usize);
    let c = (SegmentShape::PowerOut(3.0), 1.0, 5.0, 2.5f64, 40.5f64);
    let segs = vec![
        (
            100,
            ParamSegment::new(key, Hz(a.1), Hz(a.2), Samples(a.3), a.0),
        ),
        (
            1300,
            ParamSegment::new(key, Hz(b.1), Hz(b.2), Samples(b.3), b.0),
        ),
        (
            1700,
            ParamSegment::new(key, Hz(c.1), Hz(c.2), Samples(0), c.0)
                .resumed(SamplePosition(c.3), SamplePosition(c.4)),
        ),
    ];
    let want = |f: u64| -> f64 {
        match f {
            0..100 => 0.0,
            100..1300 => analytic(a.1.into(), a.2.into(), a.0, 1.0, a.3 as f64, f - 100),
            1300..1700 => analytic(b.1.into(), b.2.into(), b.0, 1.0, b.3 as f64, f - 1300),
            _ => analytic(c.1.into(), c.2.into(), c.0, c.3, c.4, f - 1700),
        }
    };
    for blocks in segment_schedules(2_000) {
        let out = render_segments(Vec::new(), segs.clone(), &blocks);
        let ctx = format!("blocks {:?}…", &blocks[..blocks.len().min(4)]);
        for (f, &v) in out.iter().enumerate() {
            let w = want(f as u64);
            assert!(
                (f64::from(v) - w).abs() <= TOL,
                "{ctx}: frame {f}: {v} vs analytic {w}"
            );
        }
        // Where linear and the curve differ most: halfway along `a` the
        // quadratic is a quarter of the way, not half.
        assert!((f64::from(out[599]) - 0.25).abs() <= TOL, "{ctx}");
        assert_eq!(out[1099], 1.0, "{ctx}: `a` lands on its end value");
        assert_eq!(out[1299], 1.0, "{ctx}: and holds it");
        assert_eq!(out[1999], 5.0, "{ctx}: `c` lands on its end value");
    }
}

/// A segment interrupted mid-block: a second segment takes over on its own
/// frame at its own (absolute) value, and a ramp after it moves on from
/// the value the segment reached — under every block schedule, the
/// interruptions landing mid-block in most.
///
/// Mutations (run): in `render_ramps`, start a segment from the value it
/// interrupts (`from: ramp.value`) → frame 437 reads near `a`'s value →
/// fails. In `Ramp::start`, keep the segment's curve (`self.curve` not
/// cleared) → the ramp after it follows the old shape → fails.
#[test]
fn an_interrupted_segment_hands_over_on_the_frame() {
    const TOL: f64 = 1e-5;
    let key = ParamKey::<Hz>::CUTOFF;
    let segs = vec![
        (
            100,
            ParamSegment::new(
                key,
                Hz(0.0),
                Hz(1.0),
                Samples(1000),
                SegmentShape::PowerIn(2.0),
            ),
        ),
        (
            437,
            ParamSegment::new(
                key,
                Hz(2.0),
                Hz(3.0),
                Samples(50),
                SegmentShape::PowerOut(3.0),
            ),
        ),
    ];
    let plan = vec![(611, UnitParam::Cutoff, 0.0, 10)];
    for blocks in segment_schedules(700) {
        let out = render_segments(plan.clone(), segs.clone(), &blocks);
        let ctx = format!("blocks {:?}…", &blocks[..blocks.len().min(4)]);
        let near = |f: usize, w: f64| {
            assert!(
                (f64::from(out[f]) - w).abs() <= TOL,
                "{ctx}: frame {f}: {} vs {w}",
                out[f]
            );
        };
        let a = |f: u64| analytic(0.0, 1.0, SegmentShape::PowerIn(2.0), 1.0, 1000.0, f - 100);
        let b = |f: u64| analytic(2.0, 3.0, SegmentShape::PowerOut(3.0), 1.0, 50.0, f - 437);
        near(436, a(436));
        near(437, b(437));
        near(460, b(460));
        assert_eq!(out[486], 3.0, "{ctx}: `b` lands on its last frame");
        assert_eq!(out[610], 3.0, "{ctx}: and holds");
        // The ramp from the held 3.0: a tenth of the way to 0 per frame.
        near(611, 2.7);
        near(615, 3.0 - 3.0 * 0.5);
        assert_eq!(out[620], 0.0, "{ctx}: the ramp lands");
    }
}

/// A param's range clamps a segment's value when the port reads it, and
/// never rewrites the segment: narrowed mid-segment, the port sits inside
/// the range; widened again, it is back on the curve where the segment has
/// run on to.
///
/// Mutations (run): in `ParamState::port`, ignore the live range (clamp to
/// the unbounded range) → the narrowed blocks read the curve → fails. After
/// the clamp, clamp each event source's stored target to the range too →
/// the widened block lands at 0.25 → fails.
#[test]
fn a_range_clamps_a_segment_without_rewriting_it() {
    const TOL: f64 = 1e-5;
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    let shape = SegmentShape::PowerOut(2.0);
    let seg = ParamSegment::new(
        ParamKey::<Hz>::CUTOFF,
        Hz(0.0),
        Hz(2.0),
        Samples(640),
        shape,
    );
    ed.insert(
        NodeKey(1),
        "ramps",
        Unforkable(Ramps {
            plan: Vec::new(),
            segs: vec![(0, seg)],
        }),
    );
    let at = param(PROBE, UnitParam::Cutoff);
    ed.spec_mut().connect_param(
        at,
        EventOut {
            node: NodeKey(1),
            port: 0,
        },
    );
    ed.commit().expect("commits");
    let curve = |f: u64| analytic(0.0, 2.0, shape, 1.0, 640.0, f);
    let mut frame = 0u64;
    let mut check = |out: &[f32], (lo, hi): (f64, f64), what: &str| {
        for (i, &v) in out.iter().enumerate() {
            let w = curve(frame + i as u64).clamp(lo, hi);
            assert!(
                (f64::from(v) - w).abs() <= TOL,
                "{what}, frame {}: {v} vs {w}",
                frame + i as u64
            );
        }
        frame += out.len() as u64;
    };
    let unbounded = (f64::NEG_INFINITY, f64::INFINITY);
    check(&block(&mut exec, &mut ed, 64)[0], unbounded, "on the curve");
    ed.set_param_range(at, ParamRange::new(0.0, 0.25))
        .expect("a range");
    for _ in 0..3 {
        let out = block(&mut exec, &mut ed, 64);
        assert!(out[0].iter().all(|&v| v == 0.25), "pinned to the bound");
        check(&out[0], (0.0, 0.25), "narrowed");
    }
    ed.set_param_range(at, ParamRange::UNBOUNDED)
        .expect("a range");
    let out = block(&mut exec, &mut ed, 64);
    assert!(
        out[0][0] > 1.0,
        "back on the curve, mid-flight: {}",
        out[0][0]
    );
    check(&out[0], unbounded, "widened");
}

/// A segment replaces the base, frame by frame: before the segment's first
/// event the port is the base, from it on the segment, never both.
///
/// Changed in doc 014 §12.2 (was `a_segment_sums_with_other_modulators_through_its_shaping`):
/// the segment went through a `-2x` LUT and summed with the base; an event
/// source now replaces the base and takes no shaping. Changed in doc 014
/// §12.6 (was `a_segment_replaces_the_base_and_audio_offsets_add`): the audio
/// modulator added beside it is gone with audio-rate param sources (§10,
/// §11).
///
/// Mutation (run): in `ParamState::port`, add the base under the event
/// source (the old offset rule) → every frame from 10 on is 0.5 high →
/// fails. Mutation (run): in `render_ramps`, write 0 before the first event
/// instead of the base → frames 0..10 read 0.5 low → fails.
#[test]
fn a_segment_replaces_the_base() {
    const TOL: f64 = 1e-5;
    let (cutoff, q) = (Cell::new(0.5), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    let shape = SegmentShape::PowerInOut(2.0);
    let seg = ParamSegment::new(
        ParamKey::<Hz>::CUTOFF,
        Hz(0.0),
        Hz(0.5),
        Samples(200),
        shape,
    );
    ed.insert(
        NodeKey(1),
        "ramps",
        Unforkable(Ramps {
            plan: Vec::new(),
            segs: vec![(10, seg)],
        }),
    );
    ed.spec_mut()
        .connect_param(param(PROBE, UnitParam::Cutoff), ev(1));
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 300, 37);
    for (f, &v) in out[0].iter().enumerate() {
        let w = if f < 10 {
            0.5
        } else {
            analytic(0.0, 0.5, shape, 1.0, 200.0, f as u64 - 10)
        };
        assert!((f64::from(v) - w).abs() <= TOL, "frame {f}: {v} vs {w}");
    }
}

// ---- a range is a control ---------------------------------------------------

/// Render one block of `n` frames through the probe graph's three outputs.
fn block(exec: &mut Executor, ed: &mut Editor, n: usize) -> [Vec<f32>; 3] {
    render(exec, ed, n, n)
}

/// The fused step's value for a source that stepped to `x`, under range
/// `(lo, hi)`: `clamp(x)`, the step's own arithmetic, so the figure is
/// exact.
fn stepped(x: f32, (lo, hi): (f32, f32)) -> f32 {
    x.clamp(lo, hi)
}

/// A range moved with `Editor::set_param_range` sends no commit — the plan
/// the executor runs is the one it had — and clamps the very next block,
/// every frame of it, while the block before it ran on the old range. A
/// range edited through the spec instead lands with a commit that, the edit
/// being only a range, sends nothing either.
///
/// Mutations (run): in `Editor::set_param_range`, skip the cell store (the
/// running unit keeps the spec's range as of the last commit) → the next
/// block is unclamped → fails. In `ParamState::port`, clamp with the range
/// the port read on its *previous* block (a `PortState` field updated after
/// the clamp) → the first block after the move is unclamped → fails. In
/// `Editor::commit`, compare specs with `==` again (a range is plan) → the
/// spec edit sends a new plan → fails. In `Editor::commit`'s range-only
/// path, skip `sync_ranges` → the spec edit never reaches the running unit
/// → fails.
#[test]
fn a_range_move_lands_on_the_next_block_without_a_commit() {
    const SEED: u64 = 23;
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: SEED }));
    let at = param(PROBE, UnitParam::Cutoff);
    ed.spec_mut().connect_param(at, ev(1));
    ed.spec_mut()
        .set_param_range(at, ParamRange::new(-2.0, 2.0));
    ed.commit().expect("commits");
    let plan = Arc::clone(ed.base().expect("a plan"));
    let mut frame = 0u64;
    let check = |out: &[Vec<f32>; 3], frame: &mut u64, range: (f32, f32), what: &str| {
        for (i, &v) in out[0].iter().enumerate() {
            let want = stepped(signal_at(SEED, *frame + i as u64), range);
            assert_eq!(
                v.to_bits(),
                want.to_bits(),
                "{what}, frame {i}: {v} vs {want}"
            );
        }
        *frame += out[0].len() as u64;
    };

    for _ in 0..2 {
        let out = block(&mut exec, &mut ed, 64);
        check(&out, &mut frame, (-2.0, 2.0), "the spec's range");
    }
    // The signal reaches past ±0.25 in a block, so the clamp shows.
    ed.set_param_range(at, ParamRange::new(-0.25, 0.25))
        .expect("a range");
    let out = block(&mut exec, &mut ed, 37);
    assert!(out[0].contains(&0.25) && out[0].contains(&-0.25));
    check(
        &out,
        &mut frame,
        (-0.25, 0.25),
        "the next block, every frame",
    );
    // Crossed bounds are ordered, as the spec's always were.
    ed.set_param_range(at, ParamRange::new(0.5, -0.1))
        .expect("a range");
    let out = block(&mut exec, &mut ed, 64);
    check(&out, &mut frame, (-0.1, 0.5), "crossed bounds");
    assert!(
        Arc::ptr_eq(&plan, ed.base().expect("a plan")),
        "no plan was sent for a range move"
    );
    assert_eq!(ed.in_flight(), 0);
    assert_eq!(
        ed.spec().params[&at].range,
        ParamRange::new(0.5, -0.1),
        "the spec records the move"
    );
    ed.commit().expect("a commit after a move");
    assert!(
        Arc::ptr_eq(&plan, ed.base().expect("a plan")),
        "a commit after a move sends nothing: the spec already says it"
    );

    // Through the spec: the edit waits for the commit, which sends no plan.
    ed.spec_mut()
        .set_param_range(at, ParamRange::new(-0.75, 0.0));
    let out = block(&mut exec, &mut ed, 64);
    check(
        &out,
        &mut frame,
        (-0.1, 0.5),
        "a spec edit waits for its commit",
    );
    ed.commit().expect("a range-only commit");
    assert!(
        Arc::ptr_eq(&plan, ed.base().expect("a plan")),
        "a range-only commit sends no plan"
    );
    assert_eq!(ed.in_flight(), 0, "nor anything else");
    let out = block(&mut exec, &mut ed, 64);
    check(
        &out,
        &mut frame,
        (-0.75, 0.0),
        "the commit's range, next block",
    );

    // A plan the range rode along with: a unit placed by a commit starts
    // with the spec's range.
    ed.insert(NodeKey(PROBE), "probe", Unforkable(Probe::new(&cutoff, &q)));
    ed.set_param_range(at, ParamRange::new(-0.5, 0.5))
        .expect("a range");
    ed.commit().expect("a regenerated probe");
    let out = block(&mut exec, &mut ed, 64);
    check(&out, &mut frame, (-0.5, 0.5), "a placed unit's first block");
}

/// A crossfade keeps the key's running param state, and with it the range
/// cells that state reads: once the fade is over, a range move reaches the
/// unit that faded in, on the next block.
///
/// Mutation (run): in `Editor::send`, give every placed unit fresh range
/// cells (`*c = ParamRanges::new(list)` unconditionally) → the editor
/// writes cells the kept state does not read → the move never lands →
/// fails.
#[test]
fn a_range_move_reaches_a_unit_that_faded_in() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    // Once, at frame 0: the fade keeps the port state that holds it.
    ed.insert(
        NodeKey(1),
        "once",
        Unforkable(Ramps::steps(&[(0, UnitParam::Cutoff, 1.0)])),
    );
    let at = param(PROBE, UnitParam::Cutoff);
    ed.spec_mut().connect_param(at, ev(1));
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 128, 64);

    let fade = tutti_graph::Fade::new(Samples(128), tutti_graph::CrossfadeCurve::EqualAmplitude);
    let (c2, q2) = (Cell::new(0.0), Cell::new(0.0));
    ed.replace(NodeKey(PROBE), Unforkable(Probe::new(&c2, &q2)), fade)
        .expect("the same params fade");
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 256, 64);
    assert!(
        out[0].iter().all(|&x| x == 1.0),
        "unclamped through the fade"
    );
    assert_eq!(ed.fades_in_flight(), 0, "the fade is over");

    ed.set_param_range(at, ParamRange::new(-1.0, 0.5))
        .expect("a range");
    let out = render(&mut exec, &mut ed, 64, 64);
    assert!(
        out[0].iter().all(|&x| x == 0.5),
        "the faded-in unit clamps on the next block: {:?}",
        &out[0][..4]
    );
}

/// A unit at a key declaring the same params in another order gets range
/// cells of its own: each of its ports clamps to its own param's range,
/// from its first block and after a move — never to the range of the
/// param the replaced unit had at that port.
///
/// Mutation (run): in `Editor::send`, keep a key's cells whatever the new
/// unit declares (drop the `c.declared() != list` replacement) → the
/// reversed probe's port 0 (`Q`) reads cell 0 (`Cutoff`'s range) → fails.
#[test]
fn a_reordered_param_list_clamps_each_port_to_its_own_range() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    // Every block: the hard swap's fresh ports hear it from their first.
    ed.insert(NodeKey(1), "one", Unforkable(Hold { value: 1.0 }));
    let (cut_at, q_at) = (param(PROBE, UnitParam::Cutoff), param(PROBE, UnitParam::Q));
    for at in [cut_at, q_at] {
        ed.spec_mut().connect_param(at, ev(1));
    }
    ed.set_param_range(cut_at, ParamRange::new(-1.0, 0.25))
        .expect("a range");
    ed.set_param_range(q_at, ParamRange::new(-1.0, 0.75))
        .expect("a range");
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 64, 64);
    assert!(out[0].iter().all(|&x| x == 0.25), "Cutoff on port 0");
    assert!(out[1].iter().all(|&x| x == 0.75), "Q on port 1");

    // A hard swap to a probe declaring `Q, Cutoff`: its output 0 is `Q`.
    ed.insert(
        NodeKey(PROBE),
        "probe",
        Unforkable(Probe::reversed(&cutoff, &q)),
    );
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 64, 64);
    assert!(
        out[0].iter().all(|&x| x == 0.75),
        "Q, now on port 0, clamps to Q's range: {:?}",
        &out[0][..4]
    );
    assert!(
        out[1].iter().all(|&x| x == 0.25),
        "Cutoff, now on port 1, clamps to Cutoff's range"
    );
    ed.set_param_range(q_at, ParamRange::new(-1.0, 0.5))
        .expect("a range");
    let out = render(&mut exec, &mut ed, 64, 64);
    assert!(out[0].iter().all(|&x| x == 0.5), "a move reaches Q's port");
    assert!(out[1].iter().all(|&x| x == 0.25), "and only Q's");
}

/// `Editor::set_param_range` refuses what a commit would, and changes
/// nothing when it does.
///
/// Mutation (run): drop the NaN check from `Editor::set_param_range` → the
/// NaN is recorded → fails.
#[test]
fn a_bad_range_move_is_refused() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, _exec) = probe_graph(64, &cutoff, &q);
    let at = param(PROBE, UnitParam::Cutoff);
    for range in [
        ParamRange::new(f32::NAN, 1.0),
        ParamRange::new(0.0, f32::NAN),
    ] {
        assert!(matches!(
            ed.set_param_range(at, range),
            Err(GraphInvalid::BadParamRange { at: a, .. }) if a == at
        ));
    }
    assert!(ed.spec().params.is_empty(), "nothing recorded");
    let gone = param(99, UnitParam::Cutoff);
    assert!(matches!(
        ed.set_param_range(gone, ParamRange::new(0.0, 1.0)),
        Err(GraphInvalid::UnknownParamNode { missing, .. }) if missing == NodeKey(99)
    ));
    assert!(ed.spec().params.is_empty(), "nothing recorded");
    ed.commit().expect("the graph is untouched");
}

/// A range that shrinks around an event ramp in flight (which replaces the
/// base, starting from it) pins the value to its bound, and rewrites
/// neither the base nor the ramp: when it widens again, the value is where
/// the ramp has got to, frame for frame — having run on, unclamped, the
/// whole time.
///
/// Changed in doc 014 §12.2: the ramp ran from 0 and the port read base +
/// ramp; an event source now replaces the base, and its first ramp starts
/// from the base.
///
/// Mutation (run): in `ParamState::port`, write the clamped value back into
/// an event source's ramp (`s.ramp.value`, the value the next frame steps
/// from, clamped with the sum) → after the widen the ramp resumes from the
/// bound → fails.
#[test]
fn a_shrinking_range_rewrites_neither_the_base_nor_a_ramp() {
    const BASE: f32 = 0.5;
    const TARGET: f32 = 2.0;
    const LEN: usize = 640;
    let (cutoff, q) = (Cell::new(BASE), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(
        NodeKey(1),
        "ramps",
        Unforkable(Ramps {
            plan: vec![(0, UnitParam::Cutoff, TARGET, LEN)],
            segs: Vec::new(),
        }),
    );
    let at = param(PROBE, UnitParam::Cutoff);
    ed.spec_mut().connect_param(
        at,
        EventOut {
            node: NodeKey(1),
            port: 0,
        },
    );
    ed.commit().expect("commits");
    // The ramp's value at absolute frame `f`: `Ramp::step`'s arithmetic,
    // from the base (the port's value before its first event).
    let ramp = |f: u64| {
        let done = f as usize + 1;
        if done >= LEN {
            TARGET
        } else {
            BASE + (TARGET - BASE) * (done as f32 / LEN as f32)
        }
    };
    let unbounded = (f32::NEG_INFINITY, f32::INFINITY);
    let mut frame = 0u64;
    let check = |out: &[Vec<f32>; 3], frame: &mut u64, range: (f32, f32), what: &str| {
        for (i, &v) in out[0].iter().enumerate() {
            // The ramp alone, the (empty) sum of offsets added: the
            // step's own arithmetic.
            let want = (ramp(*frame + i as u64) + -0.0f32).clamp(range.0, range.1);
            assert_eq!(
                v.to_bits(),
                want.to_bits(),
                "{what}, frame {i}: {v} vs {want}"
            );
        }
        *frame += out[0].len() as u64;
    };
    let out = block(&mut exec, &mut ed, 64);
    check(&out, &mut frame, unbounded, "ramping");
    // Shrink below the base itself: the value sits on the upper bound.
    ed.set_param_range(at, ParamRange::new(0.0, 0.25))
        .expect("a range");
    for _ in 0..4 {
        let out = block(&mut exec, &mut ed, 64);
        assert!(out[0].iter().all(|&v| v == 0.25), "pinned to the bound");
        check(&out, &mut frame, (0.0, 0.25), "shrunk");
    }
    assert_eq!(cutoff.get(), BASE, "the base control is the node's own");
    ed.set_param_range(at, ParamRange::UNBOUNDED)
        .expect("a range");
    let out = block(&mut exec, &mut ed, 64);
    assert!(
        out[0][0] > ramp(63) && out[0][0] < TARGET,
        "back on the ramp, mid-flight: {}",
        out[0][0]
    );
    check(&out, &mut frame, unbounded, "widened");
    for _ in 0..6 {
        let out = block(&mut exec, &mut ed, 64);
        check(&out, &mut frame, unbounded, "landed");
    }
}

// ---- differential: the executor against the reference ----------------------

/// One generated graph: per-frame signals and ramp sources driving probes,
/// some behind a latent sibling (so their param sources are delayed by
/// PDC).
#[derive(Clone, Debug)]
struct Gen {
    signals: Vec<u64>,
    ramps: Vec<Vec<(u64, UnitParam, f32, usize)>>,
    /// Per ramp source: the curve segments it sends as well (every shape,
    /// some resumed part-way), interleaved with its ramps.
    segs: Vec<Vec<(u64, ParamSegment)>>,
    /// Per probe: the latency of the sibling on its audio input (0: none).
    probes: Vec<usize>,
    /// `(probe, param, source, range)` connections, applied at step `when`
    /// (replacing the port's source) and removed at `until` (if still the
    /// port's).
    edges: Vec<GenEdge>,
    /// `(step, probe, param, new base)`.
    moves: Vec<(usize, usize, usize, f32)>,
    /// `(step, probe)`: re-insert the probe (a new generation).
    regens: Vec<(usize, usize)>,
    /// `(step, probe, latency)`: re-insert the probe's sibling at another
    /// latency, so a param source's delay appears, grows, shrinks or goes.
    lags: Vec<(usize, usize, usize)>,
    /// `(step, probe, param, range)`: move a port's range with
    /// `Editor::set_param_range` after the step's commit, no commit of its
    /// own (`None`: unbounded).
    ranges: Vec<(usize, usize, usize, Option<(f32, f32)>)>,
    blocks: Vec<usize>,
}

#[derive(Clone, Debug)]
struct GenEdge {
    probe: usize,
    param: usize,
    /// `Ok(signal)` or `Err(ramp source)`.
    source: Result<usize, usize>,
    range: Option<(f32, f32)>,
    when: usize,
    until: usize,
}

const PARAMS: [UnitParam; 2] = [UnitParam::Cutoff, UnitParam::Q];
const MAX_BLOCK: usize = 96;

/// A segment shape for generator code `k`: every variant, the libm ones
/// (a non-integer power, both signs of `Exp`) included.
fn gen_shape(k: u8) -> SegmentShape {
    match k % 8 {
        0 => SegmentShape::Linear,
        1 => SegmentShape::Hold,
        2 => SegmentShape::PowerIn(2.0),
        3 => SegmentShape::PowerOut(3.0),
        4 => SegmentShape::PowerInOut(2.0),
        5 => SegmentShape::PowerIn(1.7),
        6 => SegmentShape::Exp(3.0),
        _ => SegmentShape::Exp(-2.5),
    }
}

/// One generated segment: its frame (inside the frames a run renders, at
/// most six blocks of `MAX_BLOCK`) and the segment.
fn gen_segment() -> impl Strategy<Value = (u64, ParamSegment)> {
    (
        0u64..500,
        0usize..2,
        -3.0f32..3.0,
        -3.0f32..3.0,
        0usize..60,
        any::<u8>(),
        prop::option::of((0.0f64..30.0, 0.5f64..80.0)),
    )
        .prop_map(|(at, p, from, to, len, k, resumed)| {
            let shape = gen_shape(k);
            let seg = match PARAMS[p] {
                UnitParam::Q => {
                    ParamSegment::new(ParamKey::<Q>::Q, Q(from), Q(to), Samples(len), shape)
                }
                _ => ParamSegment::new(
                    ParamKey::<Hz>::CUTOFF,
                    Hz(from),
                    Hz(to),
                    Samples(len),
                    shape,
                ),
            };
            let seg = match resumed {
                Some((pos, l)) => seg.resumed(SamplePosition(pos), SamplePosition(l)),
                None => seg,
            };
            (at, seg)
        })
}

fn gen() -> impl Strategy<Value = Gen> {
    (1usize..=3, 0usize..=2, 1usize..=3, 1usize..=6).prop_flat_map(|(ns, nr, np, steps)| {
        let signals = prop::collection::vec(1u64..10_000, ns);
        let ramps = prop::collection::vec(
            prop::collection::vec(
                (0u64..1_500, 0usize..2, -3.0f32..3.0, 0usize..40)
                    .prop_map(|(at, p, t, l)| (at, PARAMS[p], t, l)),
                0..6,
            ),
            nr,
        );
        let segs = prop::collection::vec(prop::collection::vec(gen_segment(), 0..4), nr);
        let probes = prop::collection::vec(prop_oneof![Just(0usize), 1usize..200], np);
        let edge = (
            0..np,
            0usize..2,
            prop_oneof![
                (0..ns).prop_map(Ok),
                (0..nr.max(1)).prop_map(move |r| if nr == 0 { Ok(0) } else { Err(r) })
            ],
            prop::option::of((-2.0f32..0.0, 0.0f32..2.0)),
            0..steps,
            0..steps + 1,
        )
            .prop_map(|(probe, param, source, range, when, until)| GenEdge {
                probe,
                param,
                source,
                range,
                when,
                until,
            });
        (
            signals,
            (ramps, segs),
            probes,
            prop::collection::vec(edge, 0..8),
            prop::collection::vec((0..steps, 0..np, 0usize..2, -1.0f32..1.0), 0..4),
            prop::collection::vec((0..steps, 0..np), 0..2),
            prop::collection::vec(
                (0..steps, 0..np, prop_oneof![Just(0usize), 1usize..200]),
                0..3,
            ),
            prop::collection::vec(1usize..=MAX_BLOCK, steps),
            prop::collection::vec(
                (
                    0..steps,
                    0..np,
                    0usize..2,
                    prop::option::of((-1.5f32..0.5, -0.5f32..1.5)),
                ),
                0..4,
            ),
        )
            .prop_map(
                |(signals, (ramps, segs), probes, edges, moves, regens, lags, blocks, ranges)| {
                    Gen {
                        signals,
                        ramps,
                        segs,
                        probes,
                        edges,
                        moves,
                        regens,
                        lags,
                        ranges,
                        blocks,
                    }
                },
            )
    })
}

/// Key layout: signals `100 + i`, ramp sources `200 + i`, probes `300 + i`,
/// siblings `400 + i`.
fn run_differential(g: &Gen) {
    let prep = prepare(MAX_BLOCK);
    let (mut ed, mut exec) = Editor::new(prep);
    let mut reference = Reference::new(prep);
    // One pair of base cells per interpreter per probe.
    let cells: Vec<[[Cell; 2]; 2]> = g
        .probes
        .iter()
        .map(|_| {
            [
                [Cell::new(0.5), Cell::new(-0.25)],
                [Cell::new(0.5), Cell::new(-0.25)],
            ]
        })
        .collect();
    let probe = |i: usize, side: usize| Probe::new(&cells[i][side][0], &cells[i][side][1]);

    let mut fresh: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
    let mut outputs = Vec::new();
    for (i, &seed) in g.signals.iter().enumerate() {
        let k = NodeKey(100 + i as u64);
        ed.insert(k, "sig", Unforkable(Signal { seed }));
        fresh.insert(k, Box::new(Signal { seed }));
    }
    for (i, plan) in g.ramps.iter().enumerate() {
        let k = NodeKey(200 + i as u64);
        let segs = g.segs[i].clone();
        ed.insert(
            k,
            "ramps",
            Unforkable(Ramps {
                plan: plan.clone(),
                segs: segs.clone(),
            }),
        );
        fresh.insert(
            k,
            Box::new(Ramps {
                plan: plan.clone(),
                segs,
            }),
        );
    }
    for (i, &lag) in g.probes.iter().enumerate() {
        let k = NodeKey(300 + i as u64);
        ed.insert(k, "probe", Unforkable(probe(i, 0)));
        fresh.insert(k, Box::new(probe(i, 1)));
        for c in 0..3 {
            outputs.push(Source::Node(OutPort { node: k, port: c }));
        }
        // Every probe has a sibling (of latency 0 when it adds none), so a
        // later latency change has a node to re-insert.
        {
            let s = NodeKey(400 + i as u64);
            ed.insert(s, "lag", Unforkable(Lag::new(lag)));
            fresh.insert(s, Box::new(Lag::new(lag)));
            // Fed by the first signal, so the sibling carries something.
            ed.spec_mut().topology.edges.insert(
                InPort { node: s, port: 0 },
                Edge::Direct(Source::Node(OutPort {
                    node: NodeKey(100),
                    port: 0,
                })),
            );
            ed.spec_mut().topology.edges.insert(
                InPort { node: k, port: 0 },
                Edge::Direct(Source::Node(OutPort { node: s, port: 0 })),
            );
        }
    }
    ed.spec_mut().topology.outputs = outputs.clone();

    let width = outputs.len();
    for (step, &n) in g.blocks.iter().enumerate() {
        // This step's edits, to both.
        for e in &g.edges {
            let at = ParamIn {
                node: NodeKey(300 + e.probe as u64),
                param: PARAMS[e.param],
            };
            let from = match e.source {
                Ok(s) => ev(100 + s as u64),
                Err(r) => ev(200 + r as u64),
            };
            if e.when == step {
                // One source per port (doc 014 §5.6): a later edge on the
                // port replaces whichever it had.
                ed.spec_mut().connect_param(at, from);
                if let Some((lo, hi)) = e.range {
                    ed.spec_mut().set_param_range(at, ParamRange::new(lo, hi));
                }
            }
            if e.until == step && e.until > e.when {
                ed.spec_mut().disconnect_param(at, from);
            }
        }
        for &(s, p, k, v) in &g.moves {
            if s == step {
                cells[p][0][k].set(v);
                cells[p][1][k].set(v);
            }
        }
        for &(s, p, lag) in &g.lags {
            if s == step && step > 0 {
                let k = NodeKey(400 + p as u64);
                ed.insert(k, "lag", Unforkable(Lag::new(lag)));
                fresh.insert(k, Box::new(Lag::new(lag)));
            }
        }
        for &(s, p) in &g.regens {
            if s == step && step > 0 {
                // `insert` at a key is a new generation.
                let k = NodeKey(300 + p as u64);
                ed.insert(k, "probe", Unforkable(probe(p, 0)));
                fresh.insert(k, Box::new(probe(p, 1)));
            }
        }
        ed.commit().expect("the generated graph compiles");
        let valid = ed.spec().validate().expect("valid");
        reference.set_graph(&valid, std::mem::take(&mut fresh));
        if let Some(plan) = ed.base() {
            for d in plan.delays() {
                assert_eq!(reference.delay(d.key), d.len, "PDC of {:?}", d.key);
            }
        }
        // Range moves, after the commit: a control each, to both.
        for &(s, p, k, r) in &g.ranges {
            if s == step {
                let at = ParamIn {
                    node: NodeKey(300 + p as u64),
                    param: PARAMS[k],
                };
                let r = r.map_or(ParamRange::UNBOUNDED, |(lo, hi)| ParamRange::new(lo, hi));
                ed.set_param_range(at, r).expect("a range");
                reference.set_param_range(at, r);
            }
        }

        let mut a = vec![vec![0.0f32; n]; width];
        let mut b = vec![vec![0.0f32; n]; width];
        {
            let mut ra: Vec<&mut [f32]> = a.iter_mut().map(|c| &mut c[..]).collect();
            exec.process(n, &Transport::default(), &[], &mut ra);
            let mut rb: Vec<&mut [f32]> = b.iter_mut().map(|c| &mut c[..]).collect();
            reference.process(n, &Transport::default(), &[], &mut rb);
        }
        ed.collect();
        for (c, (x, y)) in a.iter().zip(&b).enumerate() {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                assert_eq!(
                    p.to_bits(),
                    q.to_bits(),
                    "step {step}, channel {c}, frame {i}: executor {p} vs reference {q}\n{g:?}"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// The executor's fused param step and the reference's own derivation
    /// agree bit for bit on random driven graphs — per-frame step sources,
    /// ramp sources and curve segments (every shape, some resumed part-way,
    /// interrupting each other and ramps; one source per port, replacing
    /// its base, and replaced by the next one connected there), ranges,
    /// PDC-delayed sources, connections made and broken mid-stream
    /// (declicks), base moves, latency changes (param delays appearing,
    /// growing, shrinking), regenerated probes and ranges moved without a
    /// commit — and on every param source's PDC delay.
    ///
    /// Mutations (run after the collapse to one event
    /// source per port), each diverges: in the executor, reset the ramp of
    /// a port whose source did not change; in the reference, keep a
    /// regenerated probe's param state; in the reference, declick a port's
    /// first block; in the executor, ignore the live range (clamp to the
    /// unbounded range); in the reference, read the base before an event
    /// source's first event as 0. Leaving a param source out of the node's
    /// arrival does not diverge (the reference reads the compiler's
    /// arrival); `a_param_source_counts_toward_arrival_in_both_solves` pins it.
    #[test]
    fn modulated_graphs_match_the_reference(g in gen()) {
        run_differential(&g);
    }
}

/// `Editor::param_range_cell` hands out a read-only view of the very cell
/// the port clamps to: `set_param_range` is read back through it on the
/// spot, the next block clamps to it, and a later commit (which rewrites
/// every cell from the spec) leaves it reading the same range. A param the
/// node does not declare, or a key no commit has placed, has none.
///
/// Changed in review of doc 014 §12.2: the handle was an `Arc<RangeCell>`
/// and the test stored through it; a store there was undone by the next
/// commit, so the handle is read-only and the test writes through the
/// supported path, `set_param_range`, still asserting the handle sees it.
///
/// Mutations (run): `param_range_cell` returning a view of a fresh cell
/// holding the current range (`RangeCell::new` of the stored bounds) → the
/// handle misses the later `set_param_range` → fails. `ParamRanges::new`
/// sharing one cell between all ports → the range set on Cutoff clamps Q's
/// port, which reads past it → fails. `set_param_range` not updating the
/// spec (cell only) → the commit that follows rewrites the cell to the old
/// range → fails.
#[test]
fn a_range_cell_handle_is_the_ports_own() {
    const SEED: u64 = 29;
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: SEED }));
    let at = param(PROBE, UnitParam::Cutoff);
    let q_at = param(PROBE, UnitParam::Q);
    assert!(ed.param_range_cell(at).is_none(), "no commit has placed it");
    for p in [at, q_at] {
        ed.spec_mut().connect_param(p, ev(1));
    }
    ed.commit().expect("commits");
    assert!(ed.param_range_cell(param(PROBE, UnitParam::Wet)).is_none());
    assert!(ed.param_range_cell(param(1, UnitParam::Cutoff)).is_none());
    let cell = ed.param_range_cell(at).expect("a declared, committed port");
    assert!(cell.same_cell(&ed.param_range_cell(at).expect("again")));
    assert_eq!(cell.load(), ParamRange::UNBOUNDED);

    ed.set_param_range(at, ParamRange::new(-0.5, 0.5))
        .expect("a range");
    assert_eq!(cell.load(), ParamRange::new(-0.5, 0.5), "read back at once");

    let frame = block(&mut exec, &mut ed, 64)[0].len() as u64;
    ed.set_param_range(at, ParamRange::new(-0.25, 0.25))
        .expect("a range");
    assert_eq!(cell.load(), ParamRange::new(-0.25, 0.25));
    // An unrelated commit (another node joins): the cell is rewritten from
    // the spec, which holds the same range.
    ed.insert(NodeKey(3), "other", Unforkable(Signal { seed: 1 }));
    ed.commit().expect("commits");
    assert_eq!(
        cell.load(),
        ParamRange::new(-0.25, 0.25),
        "a commit keeps it"
    );
    let out = block(&mut exec, &mut ed, 64);
    assert!(out[0].contains(&0.25) && out[0].contains(&-0.25));
    for (i, &v) in out[0].iter().enumerate() {
        let want = signal_at(SEED, frame + i as u64).clamp(-0.25, 0.25);
        assert_eq!(v.to_bits(), want.to_bits(), "cutoff, frame {i}");
    }
    // Q's port keeps its own (unbounded) range.
    assert!(out[1].iter().any(|v| v.abs() > 0.25), "Q is not clamped");
}
