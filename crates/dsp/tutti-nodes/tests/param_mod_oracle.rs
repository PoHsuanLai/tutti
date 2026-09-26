//! The graph's compiler-owned param modulation (design doc 013 item 6)
//! against the sub-graph it replaced: `AtomicSourceNode → ParamSumNode ←
//! ParamShaperNode × N`.
//!
//! The replacement is exact by construction — `ParamModShaping::shaping`
//! bakes `tutti_mod::shape` into a table with `ParamShaperNode`'s own bake
//! and lookup, and the fused step sums in source order from `-0.0` (as
//! `f32`'s `Sum`) and clamps once, as `ParamSumNode` folded — and these
//! tests hold it to that, bit for bit, for every curve `tutti_mod` shapes,
//! both polarities and several depths, and through a rendered graph.
//!
//! **The oracle.** The old nodes were built in these tests and compared
//! against sample by sample, bit for bit, while they still existed (the
//! commit that added this file, "test(tutti-nodes): pin the graph's param
//! step against the chain it replaces"); they were then deleted, and what
//! each test produced — the old nodes' output, since the two agreed on every
//! bit — is pinned as a digest. Everything here is `+`, `*`, `sqrt` and a
//! table read: correctly rounded IEEE operations, no libm, so the digests
//! are portable across targets. The shaping is also held to
//! `tutti_mod::shape` itself, within the table's interpolation error.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tutti_graph::{
    Cx, Editor, Io, Node, ParamFrom, ParamIn, ParamInput, ParamRange, Prepare, Shape, Status,
    Transport, Unforkable, PARAM_DECLICK,
};
use tutti_mod::{CurveType, Polarity};
use tutti_nodes::ParamModShaping;
use tutti_types::graph::{OutPort, Source};
use tutti_types::{ChannelLayout, Depth, NodeKey, SampleRate, Samples, UnitParam};

/// FNV-1a over `f32` bit patterns.
fn digest(values: impl IntoIterator<Item = f32>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for v in values {
        for b in v.to_bits().to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Inputs across and past the modulator's range: the table's own points,
/// the points between them, both clamps, and both zeros.
fn xs() -> Vec<f32> {
    let mut v: Vec<f32> = (0..=2_500).map(|i| i as f32 / 1_000.0 - 1.25).collect();
    v.extend((0..256).map(|i| (i as f32 / 255.0) * 2.0 - 1.0));
    v.extend([-0.0, 0.0, f32::MIN_POSITIVE, -f32::MIN_POSITIVE]);
    v
}

/// Every curve `tutti_mod::curve_apply` shapes, and two it degrades to
/// linear (which must stay linear).
const CURVES: [CurveType; 6] = [
    CurveType::Linear,
    CurveType::Exponential,
    CurveType::Logarithmic,
    CurveType::SCurve,
    CurveType::Stepped,
    CurveType::QuadIn,
];
const DEPTHS: [f32; 4] = [0.0, 0.3, 1.0, -0.7];

/// The shaping the graph applies per source equals `ParamShaperNode`'s
/// output, bit for bit (the digest), for each curve, polarity and depth; and
/// it agrees with `tutti_mod::shape` — the function the control-rate path
/// applies to the same route — within the table's interpolation error.
///
/// Mutation (run, against the oracle and after): bake the table at
/// `x_i = i / 256 · 2 − 1` (one point off) → fails. Read it at
/// `(x + 1) / 2 · 254` → fails.
#[test]
fn each_curve_shapes_as_the_old_shaper_did() {
    let mut all = Vec::new();
    for curve in CURVES {
        for polarity in [Polarity::Bipolar, Polarity::Unipolar] {
            for depth in DEPTHS {
                let s = ParamModShaping {
                    depth: Depth(depth),
                    polarity,
                    curve,
                };
                let new = s.shaping();
                for x in xs() {
                    let got = new.apply(x);
                    let want = tutti_mod::shape(x.clamp(-1.0, 1.0), Depth(depth), polarity, curve);
                    // The log curve's slope is unbounded at 0, where linear
                    // interpolation between table points is loosest.
                    assert!(
                        (got - want).abs() < 2e-2,
                        "{curve:?} {polarity:?} depth {depth} at {x}: {got} vs shape's {want}"
                    );
                    all.push(got);
                }
            }
        }
    }
    let d = digest(all);
    assert_eq!(d, SHAPE_DIGEST, "the shaping moved: digest {d:#018x}");
}

const SHAPE_DIGEST: u64 = 0x02d6_272e_ed35_0943;

/// Declares `Drive` with base `base`, and writes what it reads for it.
struct Echo {
    base: Arc<AtomicU32>,
}

impl Node for Echo {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_params(&[UnitParam::Drive])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match io.param(0) {
            ParamInput::Base => io
                .output(0)
                .fill(f32::from_bits(self.base.load(Ordering::Relaxed))),
            ParamInput::Frames(v) => io.output(0).copy_from_slice(v),
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (port == 0).then(|| f32::from_bits(self.base.load(Ordering::Relaxed)))
    }
}

/// A modulator in `[-1, 1]` that visits the table's points and the space
/// between them.
struct Wave(u64);

fn wave(seed: u64, frame: u64) -> f32 {
    ((frame.wrapping_mul(seed) % 2_001) as f32 / 1_000.0) - 1.0
}

impl Node for Wave {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for (i, o) in io.output(0).iter_mut().enumerate() {
            *o = wave(self.0, start + i as u64);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// The modulators' seeds, in source order.
const SEEDS: [u64; 3] = [7_919, 104_729, 1_299_709];

/// Through a rendered graph, three modulators on one param sum on the base
/// and clamp once exactly as `base → ParamSumNode ← ParamShaperNode × 3`
/// did, frame for frame (the digest), once the connection's declick is over.
///
/// Mutation (run, against the oracle and after): sum the offsets in reverse
/// source order in `ParamState::port` → fails (float addition is not
/// associative over three terms; over two it commutes, which is why there
/// are three). Clamp before adding the base → fails.
#[test]
fn the_graph_sums_and_clamps_as_the_old_chain_did() {
    let (base, min, max) = (2.0f32, 1.5, 2.75);
    let shapings = [
        ParamModShaping {
            depth: Depth(0.6),
            polarity: Polarity::Bipolar,
            curve: CurveType::Exponential,
        },
        ParamModShaping {
            depth: Depth(0.9),
            polarity: Polarity::Unipolar,
            curve: CurveType::SCurve,
        },
        ParamModShaping {
            depth: Depth(-0.35),
            polarity: Polarity::Bipolar,
            curve: CurveType::Logarithmic,
        },
    ];
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
    let cell = Arc::new(AtomicU32::new(base.to_bits()));
    ed.insert(
        NodeKey(1),
        "echo",
        Unforkable(Echo {
            base: Arc::clone(&cell),
        }),
    );
    let at = ParamIn {
        node: NodeKey(1),
        param: UnitParam::Drive,
    };
    // Keys 2, 3, 4: source order is the seeds' order.
    for (i, (&seed, s)) in SEEDS.iter().zip(&shapings).enumerate() {
        let key = NodeKey(2 + i as u64);
        ed.insert(key, "wave", Unforkable(Wave(seed)));
        ed.spec_mut().connect_param(
            at,
            ParamFrom::Audio(OutPort { node: key, port: 0 }),
            s.shaping(),
        );
    }
    ed.spec_mut().set_param_range(at, ParamRange::new(min, max));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    ed.commit().expect("commits");

    let frames = PARAM_DECLICK.get() + 4_096;
    let mut out = Vec::with_capacity(frames);
    while out.len() < frames {
        let mut b = [0.0f32; 64];
        exec.process(64, &Transport::default(), &[], &mut [&mut b[..]]);
        out.extend_from_slice(&b);
    }
    let settled = &out[PARAM_DECLICK.get()..frames];

    assert!(
        settled.contains(&min) && settled.contains(&max),
        "the clamp was exercised at both ends"
    );
    let d = digest(settled.iter().copied());
    assert_eq!(d, CHAIN_DIGEST, "the fused step moved: digest {d:#018x}");
}

const CHAIN_DIGEST: u64 = 0x02e6_d191_48c4_4e39;
