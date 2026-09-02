//! Differential test: the VBAP energy invariant, swept — and the rear-arc
//! behaviour that sweep uncovered.
//!
//! # What the oracle is, and why it is independent
//!
//! This is the shape a subsystem with **no library oracle** gets instead of
//! one. The property is a scalar identity that can be stated exactly — the sum
//! of the squared speaker gains is 1.0 — so the oracle is that sentence, and
//! there is nothing to compare against and nothing to justify a tolerance
//! beyond f32 accumulation. Comparing against another VBAP crate would be
//! weaker, not stronger: the panner already *is* `vbap` 0.1.1 underneath, so a
//! second copy of it could not disagree.
//!
//! Nothing asserted the invariant before this file. The existing tests check
//! where the *bearing* lands, never how much energy comes out, so a
//! normalization that was skipped, applied twice, or clamped to zero passed all
//! of them.
//!
//! # This file characterized a bug; it now pins the fix
//!
//! The first version of this file measured the behaviour rather than judging
//! it, and what it measured was a stereo pair that held unity to 30°, decayed
//! (0.933 at 45°, 0.500 at 90°, 0.067 at 135°) and was **completely silent from
//! 150° through 210°**. Its own doc comment named the cause — upstream `vbap`
//! normalizes the tuple solution and then clamps negative gains to zero — and
//! said the maintainer's call was whether that was a property or a bug.
//!
//! It was a bug, and `VbapPanner::solve_gains` now fixes it in this crate's
//! layer rather than in the vendored dependency. The assertions below are
//! therefore the flipped versions of the originals: unity is asserted at
//! **spread 0 as well**, and the dead-arc characterization is replaced by the
//! hard-pan table it became.
//!
//! # Tolerance rationale
//!
//! `1e-3` on the sum of squares. The identity is exact in real arithmetic; the
//! error present is f32 accumulation over up to 12 speaker gains, each itself
//! the result of a `f64 → f32` narrowing. The measured worst deviation across
//! the whole sweep is 5.4e-7 — over three orders of magnitude inside the bound
//! — and the run prints it so the margin stays visible. The failures it exists
//! to catch (a missing normalization, a doubled one, a clamped-away arc) are
//! order-1, not order-1e-3.
//!
//! # How the gains are read
//!
//! Through the public [`VbapPannerNode`] surface, not the private panner: width
//! is set to 0, which routes `tick` through the mono fold, and a `[1.0, 1.0]`
//! frame folds to exactly 1.0. Each output channel then carries its speaker's
//! gain unscaled. LFE is not fed by the panner (`build_vbap_mix` sends it a
//! separate low-passed feed), so it reads 0 and contributes nothing to the sum
//! — which is correct: it is not one of the gains being normalized.

use core::f32::consts::FRAC_1_SQRT_2;

use tutti_core::AudioUnit;
use tutti_spatial::VbapPannerNode;

/// Steady-state per-channel gains at one bearing, read through `AudioUnit`.
///
/// # `reset` rather than a settling loop
///
/// The de-zipper is exponential: it *asymptotes* toward the commanded bearing
/// and never arrives, so a fixed number of frames leaves a residual whose size
/// depends on how far the previous reading was — making every measurement a
/// function of the sweep's iteration order. `AudioUnit::reset` drops the ramp
/// onto the commanded position instead (and touches nothing else; position,
/// spread and width are caller-set configuration and survive it), so each
/// reading is the exact steady state at that bearing alone.
///
/// # The leading `tick` this used to need is gone, and that is the fix
///
/// `reset` seeds the smoother from the **panner's** copy of the position, and
/// `set_position` writes the *node's* `SpatialTarget`. The two were joined only
/// by `sync_position`, which ran inside `tick`. So a bare
/// `set_position` → `reset` → `tick` seeded the ramp at the panner's *stale*
/// bearing and took one step from there: every azimuth read back as roughly
/// `[0.707, 0.707]`, a plausible-looking unity that is actually front-centre
/// everywhere and pans nowhere. This function used to carry an extra leading
/// `tick` purely to work around that.
///
/// `VbapPannerNode::reset` now calls `sync_position` itself, so the workaround
/// is deleted — and its absence is part of what this file asserts.
///
/// # Only the gain assertions catch that bug, and that is worth knowing
///
/// Mutation-testing this file found that reinstating the reset bug leaves the
/// two energy tests **passing**: front-centre is `[0.707, 0.707]`, which is unit
/// energy, so a sweep that only sums squares cannot tell "correctly panned" from
/// "not panned at all". The tests that fail are
/// [`stereo_hard_pans_outside_the_pair_and_mirrors_the_rear`] and
/// [`a_surrounding_layout_does_not_fold_its_rear_arc`], because those assert
/// *where* the energy went.
///
/// That is not a hole to be plugged by widening the energy tests — it is the
/// reason this file asserts gains as well as energy. An energy law is a real
/// property and worth pinning on its own, but it is exactly half of what a
/// panner promises, and the missing half is the one that fails silently.
fn gains_at(node: &mut VbapPannerNode, azimuth_deg: f32) -> Vec<f32> {
    node.set_position(azimuth_deg, 0.0f32);
    // Width 0 takes `process_stereo_into`'s mono-fold branch, so a [1, 1] frame
    // arrives at the panner as exactly 1.0 and the outputs are the raw gains.
    node.set_width(0.0f32);

    let n = node.num_channels();
    let mut out = vec![0.0f32; n];
    node.reset(); // seed the ramp at the commanded bearing
    node.tick(&[1.0, 1.0], &mut out); // read the steady state
    out
}

fn sum_of_squares(gains: &[f32]) -> f32 {
    gains.iter().map(|g| g * g).sum()
}

/// Every layout, every spread, every bearing: the sum of squares is 1.0.
///
/// The sweep is a cross-product rather than random sampling: the domain is
/// small enough to enumerate, and an enumeration is a proof where a sample is
/// only evidence. Five layouts × seven spreads (including **0**) × 24 bearings.
///
/// Spread 0 is in the grid, which is the change from the version of this test
/// that characterized the bug. It was excluded then because `apply_spread`
/// returns early there and the invariant was upstream `vbap`'s to keep — and
/// upstream did not keep it. `solve_gains` now establishes unit energy *before*
/// `apply_spread` ever runs, so the law holds at every spread including none.
///
/// Mutation: delete the renormalization in `solve_gains` → fails at stereo,
/// spread 0%, az 45° (0.933 against 1.0). Two mutations this test verifiably
/// does *not* catch, both by design: deleting the elevation retreat (it sweeps
/// the horizontal plane only —
/// [`every_layout_holds_unit_energy_off_the_horizontal_plane`] covers it), and
/// reinstating the reset-seeding bug (front-centre is unit energy too — see the
/// note on [`gains_at`]).
#[test]
fn every_layout_holds_unit_energy_across_the_sweep() {
    // (deviation, layout, spread%, azimuth, sum_sq) — the worst point seen,
    // reported at the end so one run characterizes the whole surface.
    let mut worst = (0.0f32, "", 0u32, 0i32, 1.0f32);

    for (name, mut node) in [
        ("stereo", VbapPannerNode::stereo().unwrap()),
        ("quad", VbapPannerNode::quad().unwrap()),
        ("5.1", VbapPannerNode::surround_5_1().unwrap()),
        ("7.1", VbapPannerNode::surround_7_1().unwrap()),
        ("atmos", VbapPannerNode::atmos_7_1_4().unwrap()),
    ] {
        for spread_pct in [0u32, 1, 25, 50, 75, 99, 100] {
            node.set_spread(spread_pct as f32 / 100.0);
            for az in (0..360).step_by(15) {
                let gains = gains_at(&mut node, az as f32);
                let sum_sq = sum_of_squares(&gains);

                if (sum_sq - 1.0).abs() > worst.0 {
                    worst = ((sum_sq - 1.0).abs(), name, spread_pct, az, sum_sq);
                }
                assert!(
                    (sum_sq - 1.0).abs() < 1e-3,
                    "{name} @ spread {spread_pct}% az {az}deg: sum-of-squares is \
                     {sum_sq}, not 1.0 (gains {gains:?})"
                );
            }
        }
    }

    let (dev, wname, wspread, waz, wsum) = worst;
    println!(
        "worst energy deviation: {wname} @ spread {wspread}% az {waz}deg \
         -> sum_sq {wsum:.6} (dev {dev:.3e})"
    );
}

/// The law holds off the horizontal plane too, including where the array has no
/// speaker at all.
///
/// This is the half of the fix the azimuth sweep cannot reach. Two regions have
/// no geometry to solve against, and upstream returned an all-zero gain vector
/// for both — silence, not a fade:
///
/// - **Below an Atmos bed.** 7.1.4 has eleven speakers and none beneath the
///   listener, so everything under about -20° elevation was dead (measured:
///   every bearing at -70° and below, 31 of 36 at -60°).
/// - **The exact poles of a 2D ring.** quad / 5.1 / 7.1 solve in the horizontal
///   plane, and at ±90° elevation the direction vector's horizontal projection
///   is the zero vector, so every gain came out zero.
///
/// `solve_gains` retreats the elevation toward the plane until the array can
/// answer, which covers both. Renormalizing alone cannot: upstream's clamp had
/// already destroyed the sign information, and scaling zeros leaves zeros.
///
/// Mutation: delete the elevation-retreat loop in `solve_gains` (solve once) →
/// fails at atmos, elevation -90°, and at quad/5.1/7.1 elevation ±90°.
#[test]
fn every_layout_holds_unit_energy_off_the_horizontal_plane() {
    for (name, mut node) in [
        ("stereo", VbapPannerNode::stereo().unwrap()),
        ("quad", VbapPannerNode::quad().unwrap()),
        ("5.1", VbapPannerNode::surround_5_1().unwrap()),
        ("7.1", VbapPannerNode::surround_7_1().unwrap()),
        ("atmos", VbapPannerNode::atmos_7_1_4().unwrap()),
    ] {
        node.set_spread(0.0f32);
        node.set_width(0.0f32);

        let n = node.num_channels();
        let mut out = vec![0.0f32; n];

        for el in [-90i32, -60, -30, 0, 30, 60, 90] {
            for az in (0..360).step_by(30) {
                node.set_position(az as f32, el as f32);
                node.reset();
                node.tick(&[1.0, 1.0], &mut out);

                let sum_sq = sum_of_squares(&out);
                assert!(
                    (sum_sq - 1.0).abs() < 1e-3,
                    "{name} @ az {az}deg el {el}deg: sum-of-squares is {sum_sq}, \
                     not 1.0 (gains {out:?}). A direction the array cannot render \
                     must collapse onto the nearest one it can, at full energy — \
                     never fade to silence."
                );
            }
        }
    }
}

/// The stereo law, stated as exact gains rather than as energy alone.
///
/// This replaces the dead-arc characterization the first version of this file
/// carried. Those numbers (unity to 30°, 0.933 at 45°, 0.500 at 90°, 0.067 at
/// 135°, zero from 150° to 210°) were real measurements of a real bug; they are
/// kept in the doc comment as the "before" column and asserted nowhere.
///
/// # The law
///
/// A stereo pair sits at ±30° and has **no rear speaker**, so it has no
/// front/back axis: there is no amplitude difference between two speakers both
/// in front of you that says "behind". `solve_gains` therefore mirrors a rear
/// bearing about the lateral axis (135° → 45°, 180° → 0°) and then hard-pans
/// anything still outside the pair, at full energy.
///
/// Read as a sweep, that is: pan smoothly from centre out to the ±30° speaker,
/// hold a hard pan across the side, and come back through the mirrored front
/// arc to centre again at 180°. Every bearing is audible and every bearing is
/// unit energy. The alternative — VBAP's textbook fade — is what produced the
/// silent arc, and a caller automating a pan through 180° heard the source
/// vanish.
///
/// # Why 180° is front-centre and not something else
///
/// It is the mirror of 0°, and on a pair with no depth cue that is the honest
/// answer: directly behind and directly ahead differ by exactly the information
/// this array does not carry. The alternative of leaving 180° silent is the bug
/// this fixes; the alternative of picking one speaker arbitrarily would make
/// the sweep discontinuous at the seam.
///
/// Mutation: normalize before clamping (i.e. drop `solve_gains` and call
/// upstream directly) → fails at 90°, energy 0.500 against 1.0. Fold without
/// renormalizing → also fails at 90°, same figure. Renormalize without folding
/// → fails at 180°, energy 0.000. Reinstate the reset-seeding bug (drop
/// `sync_position` from `VbapPannerNode::reset`) → fails at 30°, reading
/// front-centre; this test and
/// [`a_surrounding_layout_does_not_fold_its_rear_arc`] are the only two here
/// that catch it.
#[test]
fn stereo_hard_pans_outside_the_pair_and_mirrors_the_rear() {
    let mut stereo = VbapPannerNode::stereo().unwrap();
    stereo.set_spread(0.0f32);

    // (azimuth, left gain, right gain). Measured, not derived.
    for (az, left, right) in [
        (0.0f32, FRAC_1_SQRT_2, FRAC_1_SQRT_2), // dead ahead, equal power
        (15.0, 0.939_071, 0.343_724),           // between centre and the L speaker
        (30.0, 1.000_000, 0.000_000),           // on the L speaker: hard pan begins
        (45.0, 1.000_000, 0.000_000),           // outside the pair, held at full energy
        (90.0, 1.000_000, 0.000_000),           // hard left — was 0.707 (half energy)
        (135.0, 1.000_000, 0.000_000),          // mirrors to 45 — was 0.259 (nearly gone)
        (165.0, 0.939_071, 0.343_724),          // mirrors to 15 — was SILENT
        (180.0, FRAC_1_SQRT_2, FRAC_1_SQRT_2),  // mirrors to 0 — was SILENT
        (195.0, 0.343_724, 0.939_071),          // mirrors to -15 — was SILENT
        (225.0, 0.000_000, 1.000_000),          // the mirrored side, hard right
        (270.0, 0.000_000, 1.000_000),          // hard right
        (315.0, 0.000_000, 1.000_000),
        (345.0, 0.343_724, 0.939_071), // back toward centre the other way
    ] {
        let got = gains_at(&mut stereo, az);
        assert!(
            (got[0] - left).abs() < 1e-3 && (got[1] - right).abs() < 1e-3,
            "stereo at {az}deg: gains {got:?}, expected [{left:.6}, {right:.6}]. \
             The pan law has changed — see this test's doc comment and re-measure \
             rather than widening the bound."
        );
    }
}

/// The fold applies to a front-only array and to nothing else.
///
/// A surrounding layout has a real speaker behind the listener, so mirroring a
/// rear bearing into the front hemisphere would not be a graceful fallback — it
/// would move the source to the wrong side of the room. The gate in
/// `solve_gains` is derived from the speaker positions rather than from the
/// channel count, and this is what checks it did not invert.
///
/// Asserted as "the rear speakers carry the energy at a rear bearing", which is
/// the observable the fold would break, rather than as exact gains: the point is
/// the hemisphere, not the taper.
///
/// Mutation: drop the `has_rear_speakers` gate so every layout folds → fails on
/// quad at 180°, where the energy moves to the front pair. Reinstate the
/// reset-seeding bug → also fails, at 180°, because the reading is front-centre
/// rather than the commanded bearing at all.
#[test]
fn a_surrounding_layout_does_not_fold_its_rear_arc() {
    // Quad speaker order is FL, FR, RL, RR (vbap's QUAD preset), and
    // `speaker_channel_map` is the identity for a layout with no LFE.
    let mut quad = VbapPannerNode::quad().unwrap();
    quad.set_spread(0.0f32);

    let front = gains_at(&mut quad, 0.0);
    assert!(
        front[0] + front[1] > 0.9 && front[2] + front[3] < 0.1,
        "quad at 0deg should be in the front pair, got {front:?}"
    );

    let rear = gains_at(&mut quad, 180.0);
    assert!(
        rear[2] + rear[3] > 0.9 && rear[0] + rear[1] < 0.1,
        "quad at 180deg should be in the REAR pair, got {rear:?}. A fold that \
         ignored the layout's rear speakers would put it in the front pair."
    );

    let rear_left = gains_at(&mut quad, 135.0);
    assert!(
        rear_left[2] > 0.9,
        "quad at 135deg should be on the rear-left speaker, got {rear_left:?}"
    );
}

/// Spread is applied on the stereo-width path, not silently dropped.
///
/// `process_stereo_into`'s width>0 branch used to call the upstream solver
/// directly and never reach `apply_spread`, so a spread set on a node fed any
/// non-zero width did nothing at all — a parameter the inspector shows, the
/// document saves and the engine ignores. Width and spread are orthogonal
/// controls (width separates two virtual sources, spread smears each across the
/// speaker field), so one must not suppress the other.
///
/// Checked on 5.1, where full spread has somewhere to spread *to*: it must
/// light up more speakers than a point source at the same bearing does.
///
/// Mutation: delete the two `apply_spread` calls from `process_stereo_into`'s
/// width branch → fails, because both readings light the same speakers.
#[test]
fn spread_reaches_the_stereo_width_path() {
    let mut node = VbapPannerNode::surround_5_1().unwrap();
    node.set_width(1.0f32); // the width>0 branch, which used to skip spread
    node.set_position(0.0f32, 0.0f32);

    let mut out = vec![0.0f32; node.num_channels()];

    node.set_spread(0.0f32);
    node.reset();
    node.tick(&[1.0, 1.0], &mut out);
    let point_lit = out.iter().filter(|g| g.abs() > 0.05).count();

    node.set_spread(1.0f32);
    node.reset();
    node.tick(&[1.0, 1.0], &mut out);
    let diffuse_lit = out.iter().filter(|g| g.abs() > 0.05).count();

    assert!(
        diffuse_lit > point_lit,
        "full spread lit {diffuse_lit} speakers and a point source lit \
         {point_lit} — spread is being dropped on the width>0 path"
    );
}
