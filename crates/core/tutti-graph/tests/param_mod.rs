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
//! - the executor and the reference interpreter agree bit for bit on random
//!   modulated graphs, across recompiles, base moves and regenerations.
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
    Cx, Editor, Event, EventOut, Executor, Io, Node, ParamFrom, ParamIn, ParamInput, ParamRamp,
    ParamRange, ParamShaping, Prepare, Reference, Shape, ShapeLut, Status, Transport,
    PARAM_DECLICK,
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
    ed.insert(NodeKey(PROBE), "probe", Probe::new(cutoff, q));
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
    ed.insert(NodeKey(1), "sig", Signal { seed: 7 });
    ed.insert(NodeKey(2), "sig", Signal { seed: 11 });
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
        Step {
            at: u64::MAX,
            height: 0.0,
        },
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
    ed.insert(NodeKey(1), "one", Step { at: 0, height: 1.0 });
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
            ed.insert(NodeKey(1), "step", Step { at: k, height: 2.0 });
            ed.insert(
                NodeKey(2),
                "ramps",
                Ramps {
                    plan: vec![(k, UnitParam::Q, 3.0, 0), (k + 20, UnitParam::Q, 5.0, 8)],
                },
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
    ed.insert(NodeKey(PROBE), "probe", probe);
    ed.spec_mut().topology.outputs = (0..3).map(|c| Source::Node(port(PROBE, c))).collect();
    ed.insert(NodeKey(1), "one", Step { at: 0, height: 1.0 });
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
    ed.insert(NodeKey(1), "sig", Signal { seed: 1 });
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
            prop::collection::vec(1usize..=MAX_BLOCK, steps),
        )
            .prop_map(
                |(signals, ramps, probes, edges, moves, regens, blocks)| Gen {
                    signals,
                    ramps,
                    probes,
                    edges,
                    moves,
                    regens,
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
        ed.insert(k, "sig", Signal { seed });
        fresh.insert(k, Box::new(Signal { seed }));
    }
    for (i, plan) in g.ramps.iter().enumerate() {
        let k = NodeKey(200 + i as u64);
        ed.insert(k, "ramps", Ramps { plan: plan.clone() });
        fresh.insert(k, Box::new(Ramps { plan: plan.clone() }));
    }
    for (i, &lag) in g.probes.iter().enumerate() {
        let k = NodeKey(300 + i as u64);
        ed.insert(k, "probe", probe(i, 0));
        fresh.insert(k, Box::new(probe(i, 1)));
        for c in 0..3 {
            outputs.push(Source::Node(OutPort { node: k, port: c }));
        }
        if lag > 0 {
            let s = NodeKey(400 + i as u64);
            ed.insert(s, "lag", Lag::new(lag));
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
        for &(s, p) in &g.regens {
            if s == step && step > 0 {
                // `insert` at a key is a new generation.
                let k = NodeKey(300 + p as u64);
                ed.insert(k, "probe", probe(p, 0));
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
    /// connections made and broken mid-stream (declicks), base moves and
    /// regenerated probes — and on every param source's PDC delay.
    ///
    /// Mutations (run), each diverges: in the reference, compare sources by
    /// their `from` only (a shaping change does not declick); in the
    /// executor, reset the ramps of a port whose sources did not change; in
    /// `compile`, leave a param source out of the node's arrival; in the
    /// reference, keep a regenerated probe's param state.
    #[test]
    fn modulated_graphs_match_the_reference(g in gen()) {
        run_differential(&g);
    }
}
