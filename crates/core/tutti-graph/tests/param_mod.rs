//! Compiler-owned param modulation (design doc 013, "Rewrite order" item 6):
//! a node declares modulatable params in its `Shape`, a spec drives them from
//! audio or event outputs, and the compiler fuses base + shaped offsets +
//! clamp into one step of the node's op.
//!
//! What is pinned here, each with the mutation it was seen to fail under:
//!
//! - an unconnected param reads its base, never 0;
//! - one and several modulators ride on the base, summed in source order and
//!   clamped once;
//! - a base move under modulation ramps across the block;
//! - connecting and disconnecting a source at runtime crossfades instead of
//!   stepping;
//! - a modulation step — an audio step or a `ParamRamp` — lands on its frame;
//! - a node that cannot say its base is never modulated;
//! - a unit's first block (an insert, a replace, a fork) is not declicked;
//! - an event source's held ramp survives another source joining the port;
//! - a PDC delay appearing on (or growing on) a param source holds the
//!   source's last value rather than dropping to 0;
//! - a fork starts its event sources where the live graph holds them;
//! - a NaN range is refused; removing a modulated node leaves the graph
//!   committable;
//! - the executor and the reference interpreter agree bit for bit on random
//!   modulated graphs, across recompiles, base moves, latency changes and
//!   regenerations.
//!
//! The per-curve shaping against the old `ParamShaperNode` lives in
//! `tutti-nodes` (`tests/param_mod_oracle.rs`), where `tutti_mod`'s curves
//! are; the sample-accuracy contract row is `tests/contract.rs`'s
//! `param_echo`, and the allocation gate `tests/rt_no_alloc.rs`'s
//! `modulated_params_are_allocation_free`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use proptest::prelude::*;
use tutti_graph::{
    CommitError, Cx, Editor, Event, EventOut, Executor, ForkByClone, ForkMode, ForkTarget,
    GraphInvalid, Io, Node, ParamFrom, ParamIn, ParamInput, ParamRamp, ParamRange, ParamShaping,
    Prepare, Reference, Shape, ShapeLut, Status, Transport, Unforkable, PARAM_DECLICK,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{
    ChannelLayout, Hz, Latency, NodeKey, ParamKey, SampleRate, Samples, UnitParam, Q,
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
    /// Answer `param_base` (a node that cannot say is never modulated).
    answers: bool,
}

impl Probe {
    fn new(cutoff: &Cell, q: &Cell) -> Self {
        Self {
            bases: [cutoff.clone(), q.clone()],
            answers: true,
        }
    }
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::from_count(3))
            .with_params(&[UnitParam::Cutoff, UnitParam::Q])
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

/// A modulator: a deterministic signal in `[-1, 1]` of the absolute frame,
/// stepping every frame (so a mistimed read shows).
struct Signal {
    seed: u64,
}

fn signal_at(seed: u64, frame: u64) -> f32 {
    let h = frame.wrapping_add(1).wrapping_mul(seed | 1).rotate_left(17) ^ seed;
    (h % 201) as f32 / 100.0 - 1.0
}

impl Node for Signal {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for (i, o) in io.output(0).iter_mut().enumerate() {
            *o = signal_at(self.seed, start + i as u64);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A step: 0 before absolute frame `at`, `height` from it on.
#[derive(Clone)]
struct Step {
    at: u64,
    height: f32,
}

impl Node for Step {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for (i, o) in io.output(0).iter_mut().enumerate() {
            *o = if start + i as u64 >= self.at {
                self.height
            } else {
                0.0
            };
        }
        Status::Modified
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

/// Sends one `ParamRamp` per `(absolute frame, param, target, length)`, on
/// its frame.
#[derive(Clone)]
struct Ramps {
    plan: Vec<(u64, UnitParam, f32, usize)>,
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
        for &(at, param, target, len) in &self.plan {
            if at < start || at >= start + io.frames() as u64 {
                continue;
            }
            let ramp = match param {
                UnitParam::Q => ParamRamp::new(ParamKey::<Q>::Q, Q(target), Samples(len)),
                _ => ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(target), Samples(len)),
            };
            let offset = io.offset((at - start) as usize).expect("inside");
            let _ = io.event_out(0).push(Event::ramp(offset, ramp));
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

/// One modulator rides on the base, sample for sample; several sum in
/// source order, each through its own shaping, and are clamped once to the
/// range. Checked after the connection's declick.
///
/// Mutation (run): in `ParamState::port`, clamp each offset rather than the
/// sum → the two-source case differs → fails. Drop the base from the sum →
/// fails the first assertion. Apply the LUT to the identity source too →
/// fails.
#[test]
fn modulators_sum_on_the_base_then_clamp() {
    let (cutoff, q) = (Cell::new(100.0), Cell::new(1.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: 7 }));
    ed.insert(NodeKey(2), "sig", Unforkable(Signal { seed: 11 }));
    let cube = ShapeLut::from_fn(|x| 0.5 * x * x * x);
    {
        let s = ed.spec_mut();
        s.connect_param(
            param(PROBE, UnitParam::Cutoff),
            ParamFrom::Audio(port(1, 0)),
            ParamShaping::Identity,
        );
        s.connect_param(
            param(PROBE, UnitParam::Q),
            ParamFrom::Audio(port(1, 0)),
            ParamShaping::Identity,
        );
        s.connect_param(
            param(PROBE, UnitParam::Q),
            ParamFrom::Audio(port(2, 0)),
            ParamShaping::Lut(cube.clone()),
        );
        s.set_param_range(param(PROBE, UnitParam::Q), ParamRange::new(0.5, 1.5));
    }
    ed.commit().expect("commits");
    let skip = PARAM_DECLICK.get();
    let out = render(&mut exec, &mut ed, skip + 500, 64);
    #[allow(clippy::needless_range_loop, reason = "three outputs at one frame")]
    for i in skip..skip + 500 {
        let f = i as u64;
        let (a, b) = (signal_at(7, f), signal_at(11, f));
        assert_eq!(
            out[0][i],
            100.0 + a,
            "frame {i}: base + one identity source"
        );
        let sum: f32 = [a, cube.eval(b)].iter().sum();
        let want = (1.0 + sum).clamp(0.5, 1.5);
        assert_eq!(out[1][i], want, "frame {i}: base + both, clamped once");
        assert_eq!(out[2][i], 3.0, "both params read frames");
    }
    assert!(
        out[1][skip..].contains(&1.5) && out[1][skip..].contains(&0.5),
        "the clamp was exercised at both ends"
    );
}

/// A base moved under modulation ramps linearly across the next block,
/// landing exactly on the new base at its last frame — no zipper step.
///
/// Mutation (run): in `ParamState::port`, ramp from the new base (`from =
/// b1`) → the block starts at the new base → fails.
#[test]
fn a_base_move_under_modulation_ramps_across_the_block() {
    let (cutoff, q) = (Cell::new(100.0), Cell::new(1.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(
        NodeKey(1),
        "zero",
        Unforkable(Step {
            at: u64::MAX,
            height: 0.0,
        }),
    );
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        ParamFrom::Audio(port(1, 0)),
        ParamShaping::Identity,
    );
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

/// Connecting a modulator at full swing, and disconnecting it, crossfade
/// over `PARAM_DECLICK` frames: no frame-to-frame step larger than the
/// fade's slope, and a disconnected param is back on its base (`Base`) once
/// the fade is over.
///
/// Mutation (run): in `ParamState::port`, skip the declick (`fading =
/// false`) → a step of 1.0 on connect → fails. Hold nothing on disconnect
/// (return `Base` at once when `sig == 0`) → a step of 1.0 back → fails.
#[test]
fn connecting_and_disconnecting_at_runtime_does_not_click() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "one", Unforkable(Step { at: 0, height: 1.0 }));
    ed.commit().expect("commits");
    let mut all = render(&mut exec, &mut ed, 128, 64)[0].clone();

    let at = param(PROBE, UnitParam::Cutoff);
    ed.spec_mut()
        .connect_param(at, ParamFrom::Audio(port(1, 0)), ParamShaping::Identity);
    ed.commit().expect("connects");
    let on = render(&mut exec, &mut ed, 1024, 64);
    assert_eq!(*on[0].last().expect("rendered"), 1.0, "fully modulated");
    all.extend(&on[0]);

    ed.spec_mut()
        .disconnect_param(at, ParamFrom::Audio(port(1, 0)));
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

/// A modulation step lands on its frame: an audio step at frame `k`, and a
/// zero-length `ParamRamp` at frame `k`, reach the param at `k` exactly,
/// under block sizes that put `k` anywhere in its block; a ramp of `len`
/// frames lands on its target at `k + len - 1`.
///
/// Mutation (run): in `render_ramps`, start a ramp on the frame after its
/// event (`offset < i`) → the ramp cases fail. In `Ramp::step`,
/// interpolate one frame behind (`(done - 1) / len`) → the ramp's first
/// step fails. In `ParamState::port`, write the audio source one frame late
/// (`out.iter_mut().skip(1)`) → the step case fails.
#[test]
fn a_modulation_step_lands_on_its_frame() {
    for block in [1usize, 7, 63, 64, 65, 128] {
        for k in [300u64, 301, 383, 384, 400] {
            let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
            let (mut ed, mut exec) = probe_graph(128, &cutoff, &q);
            ed.insert(NodeKey(1), "step", Unforkable(Step { at: k, height: 2.0 }));
            ed.insert(
                NodeKey(2),
                "ramps",
                Unforkable(Ramps {
                    plan: vec![(k, UnitParam::Q, 3.0, 0), (k + 20, UnitParam::Q, 5.0, 8)],
                }),
            );
            {
                let s = ed.spec_mut();
                s.connect_param(
                    param(PROBE, UnitParam::Cutoff),
                    ParamFrom::Audio(port(1, 0)),
                    ParamShaping::Identity,
                );
                s.connect_param(
                    param(PROBE, UnitParam::Q),
                    ParamFrom::Events(EventOut {
                        node: NodeKey(2),
                        port: 0,
                    }),
                    ParamShaping::Identity,
                );
            }
            ed.commit().expect("commits");
            let out = render(&mut exec, &mut ed, 512, block);
            let k = k as usize;
            let ctx = format!("block {block}, step at {k}");
            assert_eq!(out[0][k - 1], 0.0, "{ctx}: before the step");
            assert_eq!(out[0][k], 2.0, "{ctx}: the audio step on its frame");
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

/// A node that cannot say its base is never modulated, whatever is
/// connected: it keeps reading `Base`, so a param is never driven from a
/// base of 0.
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
    ed.insert(NodeKey(1), "one", Unforkable(Step { at: 0, height: 1.0 }));
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        ParamFrom::Audio(port(1, 0)),
        ParamShaping::Identity,
    );
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 256, 64);
    assert!(out[0].iter().all(|&x| x == 440.0));
    assert!(out[2].iter().all(|&x| x == 0.0));
}

/// A param the node does not declare, or a source that is not an output,
/// is refused at compile, naming it.
#[test]
fn undeclared_params_and_missing_sources_are_refused() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, _exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "sig", Unforkable(Signal { seed: 1 }));
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Drive),
        ParamFrom::Audio(port(1, 0)),
        ParamShaping::Identity,
    );
    let err = ed.commit().expect_err("Drive is not declared");
    assert!(format!("{err}").contains("Drive"), "{err}");
    ed.spec_mut().params.clear();
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        ParamFrom::Audio(port(1, 3)),
        ParamShaping::Identity,
    );
    assert!(ed.commit().is_err(), "the signal has no output 3");
}

/// Removing a node drops every param edge that names it: the ports it
/// modulated, and its outputs as a source of anyone else's — so the value
/// never names a node the graph no longer holds, and the other sources of a
/// port keep modulating it.
///
/// Mutation (run): in `Editor::remove`, keep sources from the removed key
/// (drop the `m.sources.retain`) → the commit is refused as naming an unknown
/// node → fails.
#[test]
fn removing_a_node_drops_its_param_edges() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "a", Unforkable(Signal { seed: 3 }));
    ed.insert(NodeKey(2), "b", Unforkable(Signal { seed: 5 }));
    for k in [1, 2] {
        ed.spec_mut().connect_param(
            param(PROBE, UnitParam::Cutoff),
            ParamFrom::Audio(port(k, 0)),
            ParamShaping::Identity,
        );
    }
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 64, 64);

    ed.remove(NodeKey(1));
    let m = &ed.spec().params[&param(PROBE, UnitParam::Cutoff)];
    assert_eq!(
        m.sources.iter().map(|s| s.from).collect::<Vec<_>>(),
        vec![ParamFrom::Audio(port(2, 0))],
        "the removed node's source is gone, the other stays"
    );
    ed.commit().expect("the value names no removed node");
    let out = render(&mut exec, &mut ed, 64, 64);
    assert_eq!(out[2][63], 1.0, "the param is still modulated");
}

/// A crossfade keeps its key's param state: the two units must declare the
/// same params, and one that declares others is refused (`FadeShape`), as a
/// fade between other ports is. A fade between two that agree keeps the
/// modulation running through it, with no declick (the sources did not
/// change).
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
    ed.insert(NodeKey(1), "one", Unforkable(Step { at: 0, height: 1.0 }));
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        ParamFrom::Audio(port(1, 0)),
        ParamShaping::Identity,
    );
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
        "modulated all through the fade, no declick: {:?}",
        &out[0][..8]
    );
}

/// A unit's first block is not a change of sources: a probe inserted with a
/// modulator already connected, and one hard-replaced at its key, read the
/// modulated value from their first frame — no fade in from the base.
///
/// Mutation (run): in `ParamState::port`, treat a fresh port as a change
/// (drop the `st.fresh` branch) → the first frames fade in from the base →
/// fails.
#[test]
fn a_units_first_block_is_not_declicked() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "one", Unforkable(Step { at: 0, height: 1.0 }));
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        ParamFrom::Audio(port(1, 0)),
        ParamShaping::Identity,
    );
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 128, 64);
    assert!(
        out[0].iter().all(|&x| x == 1.0),
        "modulated from the first frame: {:?}",
        &out[0][..4]
    );
    // A new generation at the key: a new unit, so a first block again.
    ed.insert(NodeKey(PROBE), "probe", Unforkable(Probe::new(&cutoff, &q)));
    ed.commit().expect("replaces");
    let out = render(&mut exec, &mut ed, 128, 64);
    assert!(out[0].iter().all(|&x| x == 1.0), "{:?}", &out[0][..4]);
}

/// An event source's held value belongs to the source, not to its slot in
/// the port: holding +500 from a `ParamRamp`, the port stays at 500 when
/// another source (a zero) joins it, and when this one is reshaped (the
/// declick then fades between two equal values).
///
/// Mutation (run): in `PortState::rekey`, start every source at rest (the
/// old reset of all ramps on a signature change) → the port drops towards
/// 0 → fails.
#[test]
fn an_event_sources_held_value_survives_another_source_joining() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(
        NodeKey(2),
        "ramps",
        Unforkable(Ramps {
            plan: vec![(10, UnitParam::Cutoff, 500.0, 0)],
        }),
    );
    ed.insert(
        NodeKey(1),
        "zero",
        Unforkable(Step {
            at: u64::MAX,
            height: 0.0,
        }),
    );
    let at = param(PROBE, UnitParam::Cutoff);
    let events = ParamFrom::Events(EventOut {
        node: NodeKey(2),
        port: 0,
    });
    ed.spec_mut()
        .connect_param(at, events, ParamShaping::Identity);
    ed.commit().expect("commits");
    let out = render(&mut exec, &mut ed, 512, 64);
    assert_eq!(out[0][511], 500.0, "holding the ramp's target");

    ed.spec_mut()
        .connect_param(at, ParamFrom::Audio(port(1, 0)), ParamShaping::Identity);
    ed.commit().expect("a second source joins");
    let out = render(&mut exec, &mut ed, 512, 64);
    assert!(
        out[0].iter().all(|&x| x == 500.0),
        "still 500 with a zero beside it: {:?}",
        out[0].iter().find(|&&x| x != 500.0)
    );

    ed.spec_mut().connect_param(
        at,
        events,
        ParamShaping::Lut(ShapeLut::from_fn(|x| 1000.0 * x)),
    );
    ed.commit().expect("reshaped");
    // A LUT clamps its input to `[-1, 1]`, so 500 through `1000·x` reads
    // 1000: the port moves there through the declick, from 500, not 0.
    let out = render(&mut exec, &mut ed, 512, 64);
    let first = 500.0 + 500.0 / PARAM_DECLICK.get() as f32;
    assert!((out[0][0] - first).abs() < 1e-3, "from 500: {}", out[0][0]);
    assert_eq!(out[0][511], 1000.0);
}

/// A PDC delay that appears on a param source holds the source's last
/// value for its length, and one that grows is padded with it, so the port
/// never drops to its base: a probe modulated by a constant +1 stays at +1
/// when a 100-frame sibling moves its arrival (a new delay on the source),
/// and again when the sibling grows to 150 (the delay retuned longer).
///
/// Mutation (run): in `Executor::rebuild`, start a new param delay silent
/// (`AudioRing::new`) → 100 frames at the base (0) → fails. Pad a grown one
/// with 0 (`retune`) → 50 frames at 0 → fails.
#[test]
fn a_delay_appearing_on_a_param_source_holds_its_last_value() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "one", Unforkable(Step { at: 0, height: 1.0 }));
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        ParamFrom::Audio(port(1, 0)),
        ParamShaping::Identity,
    );
    ed.commit().expect("commits");
    let _ = render(&mut exec, &mut ed, 256, 64);

    for lag in [100, 150] {
        ed.insert(NodeKey(3), "lag", Unforkable(Lag::new(lag)));
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(3),
                port: 0,
            },
            Edge::Direct(Source::Node(port(1, 0))),
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
            .find(|d| matches!(d.key, tutti_graph::DelayKey::ParamAudio { .. }))
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

/// A fork starts each event source where the live graph holds it: a
/// `ParamRamp` held at 500, and one under way, carry on in the fork from
/// its first frame instead of starting at 0. (The forked ramp source
/// replays its own plan from the fork's frame 0, so only the frames before
/// its first event say what was carried.)
///
/// Mutation (run): in `Editor::fork`, seed nothing (`seed_params` with an
/// empty map) → the fork's first frames read 0 → fails. In `TapPort::write`,
/// publish no source (`n` stays 0) → fails the same way.
#[test]
fn a_fork_carries_its_event_sources_ramps() {
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
        "ramps",
        ForkByClone(Ramps {
            plan: vec![
                (10, UnitParam::Cutoff, 500.0, 0),
                (20, UnitParam::Q, 3.0, 0),
                (400, UnitParam::Q, 7.0, 1000),
            ],
        }),
    );
    let events = ParamFrom::Events(EventOut {
        node: NodeKey(2),
        port: 0,
    });
    for p in [UnitParam::Cutoff, UnitParam::Q] {
        ed.spec_mut()
            .connect_param(param(PROBE, p), events, ParamShaping::Identity);
    }
    ed.commit().expect("commits");
    let live = render(&mut exec, &mut ed, 600, 64);
    assert_eq!(live[0][599], 500.0);

    let (mut fed, mut fexec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    let out = render(&mut fexec, &mut fed, 10, 10);
    // The Q ramp started at frame 400 and ran 200 of its 1000 frames live.
    let ramp = |done: u32| 3.0f32 + (7.0 - 3.0) * (done as f32 / 1000.0);
    assert_eq!(live[1][599], ramp(200), "live, mid-ramp");
    #[allow(clippy::needless_range_loop, reason = "two outputs at one frame")]
    for i in 0..10 {
        assert_eq!(out[0][i], 500.0, "frame {i}: the held value, carried");
        assert_eq!(
            out[1][i],
            ramp(201 + i as u32),
            "frame {i}: the ramp, carried on"
        );
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
    ed.spec_mut()
        .connect_param(at, ParamFrom::Audio(port(1, 0)), ParamShaping::Identity);
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

/// Removing a modulated node — the target of a param edge — takes its
/// ports' entries with it, so the next commit (and every one after) goes
/// through instead of naming a node that is gone; and the executor hands the
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
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        ParamFrom::Audio(port(1, 0)),
        ParamShaping::Identity,
    );
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

/// A re-prepare checks every unit out; a modulated unit's param state
/// comes back with it in the box, freed on the control side, and the
/// resumed unit is modulated again from its first block.
///
/// Mutation (run): in `Executor::apply`'s suspend, drop the checked-out
/// unit's param state instead of pushing it to `spare_params` →
/// `ParamState`'s drop check panics on the audio thread → fails.
#[test]
fn a_reprepare_hands_param_state_back() {
    let (cutoff, q) = (Cell::new(0.0), Cell::new(0.0));
    let (mut ed, mut exec) = probe_graph(64, &cutoff, &q);
    ed.insert(NodeKey(1), "one", Unforkable(Step { at: 0, height: 1.0 }));
    ed.spec_mut().connect_param(
        param(PROBE, UnitParam::Cutoff),
        ParamFrom::Audio(port(1, 0)),
        ParamShaping::Identity,
    );
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

// ---- differential: the executor against the reference ----------------------

/// One generated graph: signals and ramp sources modulating probes, some
/// behind a latent sibling (so their param sources are delayed by PDC).
#[derive(Clone, Debug)]
struct Gen {
    signals: Vec<u64>,
    ramps: Vec<Vec<(u64, UnitParam, f32, usize)>>,
    /// Per probe: the latency of the sibling on its audio input (0: none).
    probes: Vec<usize>,
    /// `(probe, param, source, shaping, range)` connections, applied at
    /// step `when` and removed at `until`.
    edges: Vec<GenEdge>,
    /// `(step, probe, param, new base)`.
    moves: Vec<(usize, usize, usize, f32)>,
    /// `(step, probe)`: re-insert the probe (a new generation).
    regens: Vec<(usize, usize)>,
    /// `(step, probe, latency)`: re-insert the probe's sibling at another
    /// latency, so a param source's delay appears, grows, shrinks or goes.
    lags: Vec<(usize, usize, usize)>,
    blocks: Vec<usize>,
}

#[derive(Clone, Debug)]
struct GenEdge {
    probe: usize,
    param: usize,
    /// `Ok(signal)` or `Err(ramp source)`.
    source: Result<usize, usize>,
    shaping: u8,
    range: Option<(f32, f32)>,
    when: usize,
    until: usize,
}

const PARAMS: [UnitParam; 2] = [UnitParam::Cutoff, UnitParam::Q];
const MAX_BLOCK: usize = 96;

fn shaping(k: u8) -> ParamShaping {
    match k % 4 {
        0 => ParamShaping::Identity,
        1 => ParamShaping::Lut(ShapeLut::from_fn(|x| x * x * x)),
        2 => ParamShaping::Lut(ShapeLut::from_fn(|x| 0.25 * x.abs())),
        _ => ParamShaping::Lut(ShapeLut::from_fn(|x| -2.0 * x)),
    }
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
        let probes = prop::collection::vec(prop_oneof![Just(0usize), 1usize..200], np);
        let edge = (
            0..np,
            0usize..2,
            prop_oneof![
                (0..ns).prop_map(Ok),
                (0..nr.max(1)).prop_map(move |r| if nr == 0 { Ok(0) } else { Err(r) })
            ],
            any::<u8>(),
            prop::option::of((-2.0f32..0.0, 0.0f32..2.0)),
            0..steps,
            0..steps + 1,
        )
            .prop_map(
                |(probe, param, source, shaping, range, when, until)| GenEdge {
                    probe,
                    param,
                    source,
                    shaping,
                    range,
                    when,
                    until,
                },
            );
        (
            signals,
            ramps,
            probes,
            prop::collection::vec(edge, 0..8),
            prop::collection::vec((0..steps, 0..np, 0usize..2, -1.0f32..1.0), 0..4),
            prop::collection::vec((0..steps, 0..np), 0..2),
            prop::collection::vec(
                (0..steps, 0..np, prop_oneof![Just(0usize), 1usize..200]),
                0..3,
            ),
            prop::collection::vec(1usize..=MAX_BLOCK, steps),
        )
            .prop_map(
                |(signals, ramps, probes, edges, moves, regens, lags, blocks)| Gen {
                    signals,
                    ramps,
                    probes,
                    edges,
                    moves,
                    regens,
                    lags,
                    blocks,
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
        ed.insert(k, "ramps", Unforkable(Ramps { plan: plan.clone() }));
        fresh.insert(k, Box::new(Ramps { plan: plan.clone() }));
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
        for (i, e) in g.edges.iter().enumerate() {
            let at = ParamIn {
                node: NodeKey(300 + e.probe as u64),
                param: PARAMS[e.param],
            };
            let from = match e.source {
                Ok(s) => ParamFrom::Audio(OutPort {
                    node: NodeKey(100 + s as u64),
                    port: 0,
                }),
                Err(r) => ParamFrom::Events(EventOut {
                    node: NodeKey(200 + r as u64),
                    port: 0,
                }),
            };
            if e.when == step {
                // A later edge on the same pair replaces the shaping; the
                // index keeps shapings apart.
                ed.spec_mut()
                    .connect_param(at, from, shaping(e.shaping.wrapping_add(i as u8)));
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
    /// agree bit for bit on random modulated graphs — audio and ramp
    /// sources, identity and LUT shapings, ranges, PDC-delayed sources,
    /// connections made and broken mid-stream (declicks), base moves,
    /// latency changes (param delays appearing, growing, shrinking) and
    /// regenerated probes — and on every param source's PDC delay.
    ///
    /// Mutations (run), each diverges: in the reference, compare sources by
    /// their `from` only (a shaping change does not declick); in the
    /// executor, reset the ramps of a port whose sources did not change; in
    /// `compile`, leave a param source out of the node's arrival; in the
    /// reference, keep a regenerated probe's param state; in the reference,
    /// fill a new param delay line with 0; in the reference, declick a
    /// port's first block; in the reference, reset every source's state
    /// when the port's sources change.
    #[test]
    fn modulated_graphs_match_the_reference(g in gen()) {
        run_differential(&g);
    }
}
