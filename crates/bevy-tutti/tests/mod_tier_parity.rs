//! The two tiers agree: audio-rate modulation sounds like the frame-rate path.
//!
//! The matrix delivers a native-param route either per frame (an `AtomicTarget`
//! mirroring into the node's atomic) or per sample (`ParamShaperNode →
//! ParamSumNode → node.param_port`). A route can switch tiers, so the two must
//! compute the same value — a divergence would be heard as a level or timbre
//! jump on a change that is supposed to be inaudible.
//!
//! Both tests here are **numeric**: they compare a node's `tick` against the
//! `tutti_mod` function the frame-rate accumulator uses, with no `App` and no
//! reconciler. What the *reconciler* emits is `mod_audio_rate_reconcile.rs`'s
//! subject; this file is only about the arithmetic at each end.
//!
//! This file used to also hold two hand-built graph-shape specs, written when
//! nothing connected a `ModRoute` to the per-sample tier. That reconciler now
//! exists, and `mod_audio_rate_reconcile.rs` asserts the same edges on a chain
//! it actually emitted, so the hand-built pair was deleted rather than left
//! claiming coverage of a graph nothing built.

#![cfg(feature = "modulation")]

use bevy_ecs::prelude::*;

use bevy_tutti::modulation::ModRoute;
use tutti_core::dsp::AudioUnit as _;
use tutti_mod::{shape, CurveType, Polarity};
use tutti_nodes::{ParamShaperNode, ParamSumNode};
use tutti_types::{Depth, ParamAddr, UnitParam};

/// A shaper built from a route's own fields shapes like the route does.
///
/// Two claims in one, and the second is what can fail at runtime:
///
/// - Every input `ParamShaperNode::new` takes is a field already on `ModRoute`,
///   so the translation needs no new authoring vocabulary. (That half is a
///   compile-time fact, and the reconciler's own source now depends on it.)
/// - The node's per-sample output matches `tutti_mod::shape` — the exact
///   function the frame-rate accumulator applies to the same route. The shaper
///   bakes depth, polarity and curve into a LUT, so this is a real comparison
///   between an interpolated table and the closed form, not a tautology.
///
/// `Exponential` rather than `Linear`: a linear curve would agree with almost
/// any LUT, so the curved case is the one that discriminates.
#[test]
fn a_shaper_built_from_a_route_agrees_with_the_routes_control_rate_shaping() {
    let mut world = World::new();
    let (src, dst) = (world.spawn_empty().id(), world.spawn_empty().id());

    let route = ModRoute::new(src, dst, ParamAddr::Unit(UnitParam::Drive))
        .with_depth(Depth(0.5))
        .with_polarity(Polarity::Unipolar)
        .with_curve(CurveType::Exponential);

    // The shaper is built straight from the route's fields.
    let shaper = ParamShaperNode::new(route.depth, route.polarity, route.curve);

    // ...and it agrees with the control-rate shaping of the same route, which is
    // what keeps a route's sound stable if delivery ever switches tiers.
    for x in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
        let mut got = [0.0f32; 1];
        let mut s = shaper.clone();
        s.tick(&[x], &mut got);
        let want = shape(x, route.depth, route.polarity, route.curve);
        assert!(
            (got[0] - want).abs() < 1e-3,
            "audio-rate shaping diverged from the route's control-rate shaping \
             at {x}: {} vs {want}",
            got[0]
        );
    }
}

/// The arithmetic the chain performs, verified against the same `fold` the
/// frame-rate accumulator uses. Two full-scale sources at depth 0.25 land the
/// same offset whichever tier evaluates them.
#[test]
fn the_chain_sums_to_what_the_frame_rate_path_would() {
    let depth = Depth(0.25);
    let (base, min, max) = (5.0f32, 0.0f32, 10.0f32);

    // Audio-rate: two shapers into a sum.
    let mut sum = ParamSumNode::new(2, min, max);
    let shaped = shape(1.0, depth, Polarity::Bipolar, CurveType::Linear);
    let mut out = [0.0f32; 1];
    sum.tick(&[base, shaped, shaped], &mut out);

    // Frame-rate: the same offsets folded by tutti-mod.
    let want = tutti_mod::fold(base, [shaped, shaped].into_iter(), min, max);

    assert!(
        (out[0] - want).abs() < 1e-6,
        "audio-rate sum {} disagrees with the control-rate fold {want}",
        out[0]
    );
}
