//! A node's blend with its aligned input is graph state (doc 013, "Bypass and
//! mute are graph state", stage M4): `Mix`, set with `Editor::set_mix`,
//! applied by the executor to any node as `out = y * m + x̂ * (1 - m)`.
//!
//! Every figure is analytic: the ends are the node's output or the aligned
//! input bit for bit, a blend is the formula written out here with the exact
//! `f32` expression the executor uses (a multiply and an add, never fused),
//! and a ramp is the doc's `w_prev + (w_new - w_prev) * ((i + 1) / frames)`.
//! A render is compared with another only as the doc prescribes: the executor
//! against the reference interpreter (`Pair`, on every block of every test
//! that uses it), serial against parallel.
//!
//! Mutations are recorded on each test (each run, each failed, then restored).

#![allow(
    clippy::needless_range_loop,
    reason = "a frame index reads several outputs and the signal at that frame"
)]

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{Kind, Pair, SpecBehaviour, TestNode, bits, prepare};
use tutti_graph::{
    CommitError, CrossfadeCurve, Cx, Edge, Editor, EventOut, ForkMode, ForkTarget, GraphInvalid,
    GraphSpec, InPort, Io, MixError, Node, NodeKey, NodeMix, NodeMode, NodeSpec, OutPort, ParamIn,
    PassError, Prepare, Shape, Source, Status, Topology, Transport, Unforkable,
};
use tutti_types::{ChannelLayout, Mix, Tail};

const BLOCK: usize = 64;
/// The most a block here is: what every `Pair` is prepared for.
const MAX_BLOCK: usize = 128;
/// `MODE_DECLICK`.
const FADE: usize = 256;
/// The latency of the latent node: its aligned input is its input this late.
const L: usize = 480;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn at(node: u64, port: u16) -> InPort {
    InPort {
        node: key(node),
        port,
    }
}

fn out(node: u64, port: u16) -> Source {
    Source::Node(OutPort {
        node: key(node),
        port,
    })
}

fn spec_of(kind: &Kind) -> NodeSpec {
    let s = TestNode::new(kind.clone()).shape();
    NodeSpec::new("test", s.audio_in(), s.audio_out())
        .with_latency(s.latency.samples())
        .with_tail(s.tail)
}

/// A signal that is never zero and never repeats within a test.
fn wave(f: u64) -> f32 {
    ((f % 23) as f32 - 11.0) * 0.0625 + 0.03125 + (f / 23 % 5) as f32 * 0.5
}

/// The node under test: `3 * x` with no latency.
fn plain() -> Kind {
    Kind::Gain {
        gain: 3.0,
        width: 1,
    }
}

/// The node under test: `3 * x` (its output is *not* delayed) with a declared
/// latency of `L`, so its aligned input is `x(n - L)`.
fn latent() -> Kind {
    Kind::Spec {
        behaviour: SpecBehaviour::Gain(3.0),
        ins: 1,
        outs: 1,
        latency: L,
        tail: Tail::None,
    }
}

/// `y * m + x̂ * (1 - m)`: the exact expression the executor writes.
fn blend(y: f32, x_hat: f32, m: f32) -> f32 {
    y * m + x_hat * (1.0 - m)
}

/// `m[i]` of a block of `frames` that moves the mix from `from` to `to`: the
/// doc's ramp.
fn ramp(from: f32, to: f32, i: usize, frames: usize) -> f32 {
    from + (to - from) * ((i + 1) as f32 / frames as f32)
}

/// A `Pair` over one node (key 1) fed by global input 0, with its mix (and
/// mode) set before the first block: a unit starts steady in the spec's.
struct Rig {
    pair: Pair,
    frame: u64,
}

impl Rig {
    fn new(workers: usize, kind: &Kind, mix: Option<Mix>, mode: Option<NodeMode>) -> Self {
        let mut t = Topology::default();
        t.nodes.insert(key(1), spec_of(kind));
        t.inputs = ChannelLayout::MONO;
        t.connect(at(1, 0), Edge::Direct(Source::Global(0)));
        t.set_outputs(vec![out(1, 0)]);
        let mut spec = GraphSpec::new(t);
        if let Some(m) = mix {
            spec.set_mix(key(1), m);
        }
        if let Some(m) = mode {
            spec.set_mode(key(1), m);
        }
        let mut pair = Pair::with_workers(MAX_BLOCK, workers);
        pair.switch(
            &spec.validate().expect("valid"),
            &BTreeMap::from([(key(1), kind.clone())]),
        );
        Self { pair, frame: 0 }
    }

    fn mix(&mut self, mix: Mix) {
        self.pair.set_mix(key(1), mix);
    }

    fn mode(&mut self, mode: NodeMode) {
        self.pair.set_mode(key(1), mode);
    }

    /// Render `blocks` (each a block length) of the signal `x`, holding the
    /// executor to the reference at every block; the executor's output.
    fn run(&mut self, blocks: &[usize], x: &dyn Fn(u64) -> f32) -> Vec<f32> {
        let mut all = Vec::new();
        for &n in blocks {
            let input: Vec<f32> = (0..n as u64).map(|i| x(self.frame + i)).collect();
            let (a, b) = self.pair.block(n, &input);
            assert_eq!(
                bits(&a),
                bits(&b),
                "the executor and the reference diverge in the block at frame {} ({n} frames)",
                self.frame
            );
            all.extend(&a[0]);
            self.frame += n as u64;
        }
        all
    }

    fn frames(&mut self, frames: usize, x: &dyn Fn(u64) -> f32) -> Vec<f32> {
        assert_eq!(frames % BLOCK, 0);
        self.run(&vec![BLOCK; frames / BLOCK], x)
    }
}

/// Serial, and parallel on two and four participants.
const WORKERS: [usize; 3] = [1, 2, 4];

// ---------------------------------------------------------------------------
// The ends
// ---------------------------------------------------------------------------

/// **`Mix::WET` is the node's output, bit for bit**, including `-0.0`, a NaN
/// with a payload, a tiny normal and the infinities (`3 * x` is the figure
/// itself; a blend `y * 1 + x̂ * 0` would turn the infinities of `x̂` into
/// NaN and `-0.0` into `+0.0`), whether or not the node has latency and
/// whether the mix was set at the start or moved to the end. (No denormal:
/// nodes run with denormals flushed, and a copy is not a node.)
///
/// Mutation (run, failed): WET run through the blend (`needs_input` true at
/// WET, the early returns for `is_wet` in `mode_op` and `blend_active` and the
/// `w == 1.0` arm removed) -> `-0.0 * 3 * 1 + x̂ * 0` is `+0.0` and the
/// infinities read NaN.
#[test]
fn wet_is_the_nodes_output_bit_for_bit() {
    let special = [
        -0.0,
        f32::from_bits(0x7fc0_1234),
        1.0e-30,
        f32::INFINITY,
        -2.5,
        0.0,
        0.1,
        f32::NEG_INFINITY,
    ];
    let x = |f: u64| special[(f % special.len() as u64) as usize];
    for workers in WORKERS {
        for kind in [plain(), latent()] {
            // Set to WET explicitly (the canonical form drops the entry) and
            // moved to it from a blend and back to it from DRY.
            let mut rig = Rig::new(workers, &kind, Some(Mix::WET), None);
            let got = rig.frames(256, &x);
            for (n, &y) in got.iter().enumerate() {
                assert_eq!(y.to_bits(), (3.0 * x(n as u64)).to_bits(), "frame {n}");
            }
            for from in [0.5, 0.0] {
                let mut rig = Rig::new(workers, &kind, Some(Mix::new(from)), None);
                rig.frames(BLOCK, &x);
                rig.mix(Mix::WET);
                // One block of ramp, then steady WET.
                rig.frames(BLOCK, &x);
                let start = rig.frame as usize;
                let got = rig.frames(256, &x);
                for (i, &y) in got.iter().enumerate() {
                    let n = (start + i) as u64;
                    assert_eq!(y.to_bits(), (3.0 * x(n)).to_bits(), "frame {n}");
                }
            }
        }
    }
}

/// **`Mix::DRY` is the aligned input, bit for bit**: what steady Bypassed
/// writes (the input, delayed by the declared latency: for `L = 0` the input
/// itself), `-0.0`, NaN payloads, denormals and infinities included, because
/// a steady DRY copies and never multiplies. The node is still **called every
/// block** and never reset: DRY is not parked.
///
/// Mutation (each run, each failed): the static DRY arm of `blend_active`
/// removed (the general `o * w + x * wx` runs) -> `-0.0` reads `+0.0` and the
/// infinities NaN; parking the node at DRY (`is_parked` true for a steady
/// Active node at mix 0) -> `calls` falls short.
#[test]
fn dry_is_the_aligned_input_bit_for_bit_and_the_node_keeps_running() {
    let special = [
        -0.0,
        f32::from_bits(0x7fc0_1234),
        1.0e-40,
        f32::INFINITY,
        -2.5,
        0.0,
        0.1,
        f32::NEG_INFINITY,
    ];
    let x = |f: u64| special[(f % special.len() as u64) as usize];
    for workers in WORKERS {
        for (kind, delay) in [(plain(), 0usize), (latent(), L)] {
            let mut rig = Rig::new(workers, &kind, Some(Mix::DRY), None);
            let got = rig.frames(1024, &x);
            for (n, &y) in got.iter().enumerate() {
                let want = if n >= delay {
                    x((n - delay) as u64)
                } else {
                    0.0
                };
                assert_eq!(y.to_bits(), want.to_bits(), "frame {n}");
            }
        }
    }

    // Called every block, never reset, and un-DRY-ing needs no warm-up: the
    // first frame off DRY is already `y * m` plus `x̂ * (1 - m)`.
    let calls = Arc::new(AtomicUsize::new(0));
    let resets = Arc::new(AtomicUsize::new(0));
    let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(
        key(1),
        "probe",
        Unforkable(Counter {
            calls: Arc::clone(&calls),
            resets: Arc::clone(&resets),
        }),
    );
    ed.spec_mut()
        .topology
        .connect(at(1, 0), Edge::Direct(Source::Global(0)));
    ed.spec_mut().topology.set_outputs(vec![out(1, 0)]);
    ed.set_mix(key(1), Mix::DRY).expect("a node");
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let input: Vec<f32> = (0..BLOCK as u64).map(wave).collect();
    let mut o = vec![0.0f32; BLOCK];
    for _ in 0..6 {
        exec.process(BLOCK, &Transport::default(), &[&input], &mut [&mut o[..]]);
        assert_eq!(bits(&[o.clone()]), bits(std::slice::from_ref(&input)));
    }
    ed.set_mix(key(1), Mix::new(0.5)).expect("a node");
    // The ramp block, then a steady one: `y = 3 x`, `x̂ = x` (no latency).
    exec.process(BLOCK, &Transport::default(), &[&input], &mut [&mut o[..]]);
    exec.process(BLOCK, &Transport::default(), &[&input], &mut [&mut o[..]]);
    for i in 0..BLOCK {
        assert_eq!(
            o[i].to_bits(),
            blend(3.0 * input[i], input[i], 0.5).to_bits()
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 8, "called every block");
    assert_eq!(resets.load(Ordering::Relaxed), 0, "never reset");
}

/// `3 * x`, counting its calls and its resets.
struct Counter {
    calls: Arc<AtomicUsize>,
    resets: Arc<AtomicUsize>,
}

impl Node for Counter {
    fn shape(&self) -> Shape {
        Shape::through_discrete(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let n = io.frames();
        let (ins, mut outs) = io.split();
        for i in 0..n {
            outs.get(0)[i] = 3.0 * ins.get(0)[i];
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.resets.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// The blend
// ---------------------------------------------------------------------------

/// **A mix between the ends is `y * m + x̂ * (1 - m)`** on a latent node,
/// `x̂` being the input `L` frames late (silence before): the figure is the
/// exact expression, frame for frame, in blocks of every length.
///
/// Mutation (each run, each failed): the weights swapped (`o * (1 - m) + x *
/// m`) -> fails at the first frame; the aligned input taken undelayed (the
/// scratch fed from the input rather than the line's output) -> fails from
/// the first frame. (A swap of the two products' order is no mutation: the
/// sum is commutative.)
#[test]
fn a_mix_between_the_ends_blends_the_output_with_the_aligned_input() {
    for workers in WORKERS {
        for blocks in [vec![BLOCK; 24], vec![100; 16], vec![7; 220]] {
            for m in [0.25f32, 0.5, 0.875] {
                let mut rig = Rig::new(workers, &latent(), Some(Mix::new(m)), None);
                let got = rig.run(&blocks, &wave);
                for (n, &y) in got.iter().enumerate() {
                    let x_hat = if n >= L { wave((n - L) as u64) } else { 0.0 };
                    let want = blend(3.0 * wave(n as u64), x_hat, m);
                    assert_eq!(y.to_bits(), want.to_bits(), "frame {n}, mix {m}");
                }
            }
        }
    }
}

/// The blend on a node with no latency: the aligned input is the input itself
/// (kept beside the output, since the node works in place), and the first
/// block has no ramp.
///
/// Mutation (each run, each failed): `align` not copying the input to the
/// scratch when the node has no line (`(None, true)` arm without the copy) ->
/// `x̂` reads stale and the figure fails; a first block that ramps (the unit
/// built with `mix_to` at `0.0` against a cell at the mix) -> the first block
/// fails.
#[test]
fn the_first_block_has_no_ramp_and_a_plain_node_blends_with_its_input() {
    for workers in WORKERS {
        let mut rig = Rig::new(workers, &plain(), Some(Mix::new(0.25)), None);
        let got = rig.frames(256, &wave);
        for (n, &y) in got.iter().enumerate() {
            let want = blend(3.0 * wave(n as u64), wave(n as u64), 0.25);
            assert_eq!(y.to_bits(), want.to_bits(), "frame {n}");
        }
    }
}

/// **A move ramps over the block it lands on**, per the doc's formula, from
/// the mix the previous block was given to the new one, and holds still the
/// block after; a move on a block of another length ramps across that block.
///
/// Mutation (each run, each failed): `ramp_at` with `i as f32 / frames` (starts
/// at the old mix, never reaches the new) -> the ramp's first frame fails; the
/// ramp across `MAX_BLOCK` rather than the block's frames -> the 100-frame
/// block fails; `mix_from` not advanced by `snapshot` -> the block after the
/// ramp ramps again.
#[test]
fn a_move_ramps_over_one_block_per_the_formula() {
    for workers in WORKERS {
        let mut rig = Rig::new(workers, &plain(), Some(Mix::new(0.75)), None);
        let m_at = |rig: &mut Rig, frames: usize, from: f32, to: f32| {
            let start = rig.frame;
            let got = rig.run(&[frames], &wave);
            for (i, &y) in got.iter().enumerate() {
                let n = start + i as u64;
                let m = if from == to {
                    to
                } else {
                    ramp(from, to, i, frames)
                };
                let want = blend(3.0 * wave(n), wave(n), m);
                assert_eq!(y.to_bits(), want.to_bits(), "frame {n} ({from} -> {to})");
            }
        };
        m_at(&mut rig, BLOCK, 0.75, 0.75);
        rig.mix(Mix::new(0.25));
        m_at(&mut rig, BLOCK, 0.75, 0.25);
        m_at(&mut rig, BLOCK, 0.25, 0.25);
        rig.mix(Mix::new(0.5));
        m_at(&mut rig, 100, 0.25, 0.5);
        m_at(&mut rig, 100, 0.5, 0.5);
        // Two moves in one block: the last wins, from what the previous block
        // was given.
        rig.mix(Mix::DRY);
        rig.mix(Mix::new(0.125));
        m_at(&mut rig, BLOCK, 0.5, 0.125);
    }
}

// ---------------------------------------------------------------------------
// The mix and the mode
// ---------------------------------------------------------------------------

/// **The mode wins.** A Bypassed node is its aligned input and a Muted one is
/// `+0.0` whatever the mix, once the fade is over (the mix enters a fade only
/// as the weights of the Active end).
///
/// Mutation (run, failed): steady Muted blending (`blend_active` for the
/// Muted arm) -> the muted output is not silence. (The Bypassed half is
/// structural: a parked node never reads the mix. Its fade is
/// `a_fade_runs_against_the_weights_of_the_mix`'s.)
#[test]
fn the_mode_wins_over_the_mix() {
    for workers in WORKERS {
        for m in [0.0f32, 0.3, 0.5, 1.0] {
            let mut rig = Rig::new(
                workers,
                &latent(),
                Some(Mix::new(m)),
                Some(NodeMode::Bypassed),
            );
            let got = rig.frames(1024, &wave);
            for (n, &y) in got.iter().enumerate() {
                let want = if n >= L { wave((n - L) as u64) } else { 0.0 };
                assert_eq!(y.to_bits(), want.to_bits(), "bypassed, mix {m}, frame {n}");
            }
            let mut rig = Rig::new(workers, &plain(), Some(Mix::new(m)), Some(NodeMode::Muted));
            let got = rig.frames(256, &wave);
            assert!(got.iter().all(|y| y.to_bits() == 0), "muted, mix {m}");
        }
    }
}

/// **A toggle to Active fades to the mix's weights**, not to `(1, 0)`: from
/// Muted `(0, 0)` to Active at `m` the weights are `(0 + (m - 0) * g(k),
/// 0 + ((1 - m) - 0) * g(k))` with `g` the equal-amplitude gain of frame `k`,
/// and after the fade the steady blend. And Active with a mix, toggled to
/// Bypassed, fades from `(m, 1 - m)` to `(0, 1)`.
///
/// Mutation (each run, each failed): the fade targeting `(1, 0)` for Active
/// (`heading.weights(1.0)` in the ramp) -> the fade frames fail; the unit's
/// steady weights kept as `(1, 0)` rather than `(m, 1 - m)` (`step` writing
/// `heading.weights(1.0)`) -> the fade to Bypassed starts from the wrong
/// weights and fails.
#[test]
fn a_fade_runs_against_the_weights_of_the_mix() {
    const M: f32 = 0.25;
    for workers in WORKERS {
        let mut rig = Rig::new(workers, &plain(), Some(Mix::new(M)), Some(NodeMode::Muted));
        rig.frames(BLOCK, &wave);
        rig.mode(NodeMode::Active);
        let start = rig.frame;
        let got = rig.frames(FADE + 128, &wave);
        for (i, &y) in got.iter().enumerate() {
            let n = start + i as u64;
            let want = if i < FADE {
                let g = CrossfadeCurve::EqualAmplitude.gains(i, FADE).0;
                let (wy, wx) = (0.0 + (M - 0.0) * g, 0.0 + ((1.0 - M) - 0.0) * g);
                (3.0 * wave(n)) * wy + wave(n) * wx
            } else {
                blend(3.0 * wave(n), wave(n), M)
            };
            assert_eq!(y.to_bits(), want.to_bits(), "frame {n} (toggle + {i})");
        }

        // Active at M, toggled to Bypassed: `(M, 1 - M)` to `(0, 1)`.
        rig.mode(NodeMode::Bypassed);
        let start = rig.frame;
        let got = rig.frames(FADE + 128, &wave);
        for (i, &y) in got.iter().enumerate() {
            let n = start + i as u64;
            let want = if i < FADE {
                let g = CrossfadeCurve::EqualAmplitude.gains(i, FADE).0;
                let (wy, wx) = (M + (0.0 - M) * g, (1.0 - M) + (1.0 - (1.0 - M)) * g);
                (3.0 * wave(n)) * wy + wave(n) * wx
            } else {
                wave(n)
            };
            assert_eq!(y.to_bits(), want.to_bits(), "frame {n} (toggle + {i})");
        }
    }
}

/// A mix that moves while a fade runs does not jump: the fade's target
/// follows the mix frame by frame. Pinned at the one frame the move's block
/// starts on: the output there is the fade's blend at the ramp's first mix,
/// not at the new mix.
///
/// Mutation (run, failed): the fade's target taken at the block's final mix
/// (`self.mix.to` for every frame) -> the moving block fails.
#[test]
fn a_mix_moving_during_a_fade_follows_frame_by_frame() {
    for workers in WORKERS {
        let mut rig = Rig::new(
            workers,
            &plain(),
            Some(Mix::new(0.5)),
            Some(NodeMode::Muted),
        );
        rig.frames(BLOCK, &wave);
        rig.mode(NodeMode::Active);
        let fade_start = rig.frame;
        rig.frames(BLOCK, &wave);
        rig.mix(Mix::new(1.0));
        let start = rig.frame;
        let got = rig.frames(BLOCK, &wave);
        for (i, &y) in got.iter().enumerate() {
            let n = start + i as u64;
            let k = (n - fade_start) as usize;
            let g = CrossfadeCurve::EqualAmplitude.gains(k, FADE).0;
            let m = ramp(0.5, 1.0, i, BLOCK);
            let (wy, wx) = (0.0 + (m - 0.0) * g, 0.0 + ((1.0 - m) - 0.0) * g);
            let want = (3.0 * wave(n)) * wy + wave(n) * wx;
            assert_eq!(y.to_bits(), want.to_bits(), "frame {n}");
        }
    }
}

// ---------------------------------------------------------------------------
// Determinism, the plan, forks, the spec
// ---------------------------------------------------------------------------

/// Calls a function on the frame it runs: a control thread's `set_mix`
/// landing in the middle of a block, staged deterministically.
struct Hook {
    run: Box<dyn FnMut() + Send>,
}

impl Node for Hook {
    fn shape(&self) -> Shape {
        Shape::source_discrete(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        (self.run)();
        io.output(0).fill(1.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **The mix snapshot is per block**: a `set_mix` that lands while a block runs
/// (here from an upstream node's `process`, before the node's own op) reaches
/// no op of that block, and the ramp is the next block's. Serial and
/// parallel.
///
/// Mutation (run, failed): the mix read from the cell at the node's op
/// (`is_plain` and `mix_run` reading `self.cell.load_mix()`) -> the node's op
/// in the block already ramps.
#[test]
fn a_mix_stored_mid_block_lands_on_the_next_block() {
    for workers in WORKERS {
        let (ed, exec) = Editor::new(prepare(MAX_BLOCK));
        let ed = Arc::new(Mutex::new(ed));
        let mut exec = exec;
        if workers > 1 {
            exec.set_pool(Some(common::test_pool(workers)));
        }
        let blocks = Arc::new(AtomicUsize::new(0));
        {
            let mut e = ed.lock().unwrap();
            let (hook_ed, hook_blocks) = (Arc::clone(&ed), Arc::clone(&blocks));
            e.insert(
                key(1),
                "hook",
                Unforkable(Hook {
                    run: Box::new(move || {
                        // The third block: store while the block is running.
                        if hook_blocks.fetch_add(1, Ordering::Relaxed) == 2 {
                            hook_ed
                                .lock()
                                .unwrap()
                                .set_mix(key(2), Mix::new(0.5))
                                .expect("a node");
                        }
                    }),
                }),
            );
            e.insert(key(2), "gain", Unforkable(TestNode::new(plain())));
            e.spec_mut()
                .topology
                .connect(at(2, 0), Edge::Direct(out(1, 0)));
            e.spec_mut().topology.set_outputs(vec![out(2, 0)]);
            e.commit().expect("commits");
        }
        exec.apply_pending();
        ed.lock().unwrap().collect();
        let mut got = Vec::new();
        for _ in 0..8 {
            let mut o = vec![0.0f32; BLOCK];
            exec.process(BLOCK, &Transport::default(), &[], &mut [&mut o[..]]);
            got.extend(o);
        }
        // The hook's output is 1.0, so `y = 3`, `x̂ = 1`.
        assert!(
            got[..3 * BLOCK].iter().all(|&v| v == 3.0),
            "the block the store landed in is still WET"
        );
        for i in 0..BLOCK {
            let m = ramp(1.0, 0.5, i, BLOCK);
            assert_eq!(got[3 * BLOCK + i].to_bits(), blend(3.0, 1.0, m).to_bits());
        }
        for &v in &got[4 * BLOCK..] {
            assert_eq!(v.to_bits(), blend(3.0, 1.0, 0.5).to_bits());
        }
    }
}

/// `3 * x` on one channel: a clonable twin of `plain()`, for the forks.
#[derive(Clone)]
struct Triple;

impl Node for Triple {
    fn shape(&self) -> Shape {
        Shape::through_discrete(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let (ins, mut outs) = io.split();
        for i in 0..n {
            outs.get(0)[i] = 3.0 * ins.get(0)[i];
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// An editor and executor over one `Triple` node, fed by global input 0.
fn solo(mix: Option<Mix>) -> (Editor, tutti_graph::Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(key(1), "gain", tutti_graph::ForkByClone(Triple));
    ed.spec_mut()
        .topology
        .connect(at(1, 0), Edge::Direct(Source::Global(0)));
    ed.spec_mut().topology.set_outputs(vec![out(1, 0)]);
    if let Some(m) = mix {
        ed.set_mix(key(1), m).expect("a node");
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec)
}

/// **`set_mix` does not change the plan**: the `Arc` the editor and the
/// executor hold is the same one after it, and a commit that has nothing else
/// to send sends nothing; a mix edited in the spec directly lands at the next
/// commit, still not a new plan.
///
/// Mutation (each run, each failed): `same_but_ranges` comparing `mixes` -> the
/// commit after an edit of the spec's mixes compiles and sends a new plan;
/// `sync_ranges` not writing the mix cells -> the mix edited in the spec never
/// reaches the unit.
#[test]
fn set_mix_does_not_change_the_plan() {
    let (mut ed, mut exec) = solo(None);
    let (ed_plan, exec_plan) = (
        Arc::clone(ed.base().expect("planned")),
        Arc::clone(exec.plan().expect("running")),
    );
    ed.set_mix(key(1), Mix::new(0.25)).expect("a node");
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(Arc::ptr_eq(&ed_plan, ed.base().expect("planned")));
    assert!(Arc::ptr_eq(&exec_plan, exec.plan().expect("running")));
    assert_eq!(ed.mix(key(1)), Mix::new(0.25));
    assert_eq!(ed.mix(key(9)), Mix::WET, "absent is WET");
    // Back to WET: the spec holds no entry for it.
    ed.set_mix(key(1), Mix::WET).expect("a node");
    assert!(ed.spec().mixes.is_empty());
    ed.spec_mut().set_mix(key(1), Mix::DRY);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(Arc::ptr_eq(&ed_plan, ed.base().expect("planned")));
    let input: Vec<f32> = (0..BLOCK as u64).map(wave).collect();
    let mut o = vec![0.0f32; BLOCK];
    for _ in 0..3 {
        exec.process(BLOCK, &Transport::default(), &[&input], &mut [&mut o[..]]);
    }
    assert_eq!(
        bits(&[o.clone()]),
        bits(std::slice::from_ref(&input)),
        "DRY"
    );
}

/// `set_mix` refuses a key with no node and a mix off `0..=1` (NaN, negative,
/// above one), a spec with a mix for a missing node or a bad mix is invalid,
/// and removing the node takes its mix with it. A refusal changes nothing.
///
/// Mutation (each run, each failed): drop the `contains_key` check in
/// `set_mix`; drop the `is_sound` check there; drop the `MixWithoutNode` /
/// `BadMix` pushes in `validate`; drop `spec.mixes.remove` in `Editor::remove`.
#[test]
fn mixes_name_nodes_and_stay_in_range() {
    let (mut ed, _exec) = solo(Some(Mix::new(0.5)));
    assert_eq!(
        ed.set_mix(key(7), Mix::DRY),
        Err(MixError::NoSuchNode { node: key(7) })
    );
    for bad in [f32::NAN, -0.01, 1.5, f32::INFINITY] {
        assert_eq!(ed.set_mix(key(1), Mix::new(bad)), Err(MixError::OutOfRange));
    }
    assert_eq!(ed.mix(key(1)), Mix::new(0.5), "a refusal changes nothing");

    let mut spec: GraphSpec = ed.spec().clone();
    spec.set_mix(key(7), Mix::DRY);
    assert_eq!(
        spec.validate().map(|_| ()),
        Err(vec![GraphInvalid::MixWithoutNode { node: key(7) }])
    );
    spec.mixes.remove(&key(7));
    spec.set_mix(key(1), Mix::new(2.0));
    let errs = spec.validate().map(|_| ()).expect_err("a bad mix");
    assert!(matches!(
        errs.as_slice(),
        [GraphInvalid::BadMix { node, .. }] if *node == key(1)
    ));
    spec.set_mix(key(1), Mix::new(0.5));
    let v = spec.validate().expect("valid");
    assert_eq!(v.mixes().get(&key(1)).map(|m| m.mix), Some(Mix::new(0.5)));

    ed.remove(key(1));
    assert!(ed.spec().mixes.is_empty());
}

/// **A fork copies the mixes and starts every unit at them**: the live graph,
/// moved to a mix and forked, renders its fork the blend at that mix from the
/// fork's first frame (steady, no ramp), and keeps its own.
///
/// Mutation (run, failed): `Editor::fork`'s spec built without `mixes` -> the
/// fork renders `3 * x`.
#[test]
fn a_fork_copies_the_mix() {
    let (mut ed, mut exec) = solo(None);
    let input: Vec<f32> = (0..BLOCK as u64).map(wave).collect();
    let mut o = vec![0.0f32; BLOCK];
    exec.process(BLOCK, &Transport::default(), &[&input], &mut [&mut o[..]]);
    ed.set_mix(key(1), Mix::new(0.5)).expect("a node");
    exec.process(BLOCK, &Transport::default(), &[&input], &mut [&mut o[..]]);

    let (fed, mut fexec) = ed
        .fork(ForkTarget::Master, ForkMode::Offline, prepare(MAX_BLOCK))
        .expect("forks");
    assert_eq!(fed.mix(key(1)), Mix::new(0.5));
    let mut f = vec![0.0f32; BLOCK];
    fexec.process(BLOCK, &Transport::default(), &[&input], &mut [&mut f[..]]);
    for i in 0..BLOCK {
        assert_eq!(
            f[i].to_bits(),
            blend(3.0 * input[i], input[i], 0.5).to_bits()
        );
    }
    exec.process(BLOCK, &Transport::default(), &[&input], &mut [&mut o[..]]);
    for i in 0..BLOCK {
        assert_eq!(
            o[i].to_bits(),
            blend(3.0 * input[i], input[i], 0.5).to_bits()
        );
    }
}

/// **Nothing hears a pass a node does not have.** A mono node with a stereo
/// output, built from widths, names no placement: by index its input would be
/// heard on the left alone. It commits and runs while nothing reads its dry
/// path; `set_mix` short of WET is refused, and so is a commit of a spec that
/// mixes it, bypasses it or drives its mix from a lane, each with the reason.
/// `Mix::WET` and a node at one width are accepted.
///
/// Mutation (each run, each failed): `set_mix` not asking `check_pass` → the
/// half mix is accepted; `check_passes` not called in `commit` → the spec's
/// mix commits; its `heard` dropping the mode, or the lane's source → that
/// commit is accepted.
#[test]
fn a_mix_of_a_node_with_no_pass_is_refused() {
    let (mut ed, _exec) = Editor::new(prepare(MAX_BLOCK));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(
        key(1),
        "split",
        Unforkable(TestNode::new(Kind::Spec {
            behaviour: SpecBehaviour::Fan,
            ins: 1,
            outs: 2,
            latency: 0,
            tail: Tail::None,
        })),
    );
    ed.insert(
        key(2),
        "driver",
        Unforkable(TestNode::new(Kind::MixDriver {
            period: 64,
            latency: 0,
        })),
    );
    ed.spec_mut()
        .topology
        .connect(at(1, 0), Edge::Direct(Source::Global(0)));
    ed.spec_mut()
        .topology
        .set_outputs(vec![out(1, 0), out(1, 1)]);
    ed.commit().expect("nothing hears its pass yet");

    let no_pass = PassError::Unplaced {
        inputs: 1,
        outputs: 2,
    };
    assert_eq!(
        ed.set_mix(key(1), Mix::new(0.5)),
        Err(MixError::Unplaced {
            node: key(1),
            error: no_pass
        })
    );
    assert_eq!(ed.mix(key(1)), Mix::WET, "a refusal changes nothing");
    assert_eq!(ed.set_mix(key(1), Mix::WET), Ok(()));

    let refused = Err(CommitError::Unplaced {
        node: key(1),
        error: no_pass,
    });
    ed.spec_mut().set_mix(key(1), Mix::new(0.5));
    assert_eq!(ed.commit(), refused, "a mix in the spec");
    ed.spec_mut().set_mix(key(1), Mix::WET);

    ed.spec_mut().set_mode(key(1), NodeMode::Bypassed);
    assert_eq!(ed.commit(), refused, "a bypass in the spec");
    ed.spec_mut().set_mode(key(1), NodeMode::Active);

    let mix = ParamIn {
        node: key(1),
        param: NodeMix::ADDR,
    };
    let lane = EventOut {
        node: key(2),
        port: 0,
    };
    ed.spec_mut().connect_param(mix, lane);
    assert_eq!(ed.commit(), refused, "a lane on the mix");
    ed.spec_mut().disconnect_param(mix, lane);

    ed.commit().expect("nothing hears its pass again");
}
