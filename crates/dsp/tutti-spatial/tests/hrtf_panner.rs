//! Direct tests of HRIR selection and interpolation in the binaural panner.
//!
//! `src/hrtf/panner.rs` has no unit tests of its own: the node tests in
//! `src/hrtf/node.rs` only prove the frame bridge emits audio. This file
//! drives [`HrtfBinauralNode`] with a synthetic HRIR sphere whose left/right
//! impulse responses encode a **known** interaural time difference and level
//! difference as a function of azimuth, then recovers the interpolated IR by
//! impulsing the node.
//!
//! # The sphere
//!
//! An octagonal bipyramid (8 equatorial vertices every 45°, plus the poles)
//! whose vertex positions match `direction_from_degrees` in `panner.rs`:
//! azimuth 0 = front (−z), +90 = left (−x), −90 = right (+x). Each equatorial
//! vertex stores:
//!
//! - a unit-ish delta whose **right-ear delay** is `round(k · sin(az))`
//!   samples (positive = source on the left, so the right ear is the far ear)
//! - left/right gains `1 ± ILD · sin(az)` (near ear louder)
//! - a `cos(az)` marker tap so **front ≠ back** — `sin(az)` alone cannot tell
//!   0° from 180°, and a wrap bug that clamps 359° to 180° would otherwise
//!   look like a hit
//!
//! Poles store a median-plane IR (equal L/R, no delay, no marker).
//!
//! # How the IR is read
//!
//! Output lags one HRTF frame (`INTERPOLATION_STEPS * BLOCK_LEN` in
//! `panner.rs`). `reset` seats the de-zipper on the commanded bearing but
//! rewinds `prev_dir` to front, so the first processed frame still cross-fades
//! from centre; two silent frames are drained before the impulse. The node
//! averages its stereo input; `[1, 1]` is the conventional impulse and, with
//! this sphere, recovers the encoded IR at unity gain (FFT normalisation
//! cancels the ½-fold).

use approx::assert_abs_diff_eq;
use tutti_core::{AudioUnit, Azimuth, Elevation, Radians, SampleRate, Samples};
use tutti_spatial::HrtfBinauralNode;

const SAMPLE_RATE: u32 = 44_100;
const IR_LEN: usize = 64;
/// `INTERPOLATION_STEPS * BLOCK_LEN` in `panner.rs`. Output lags by one frame.
const FRAME_LEN: usize = 4 * 128;
/// Max |ITD| in samples, at azimuth ±90°.
const ITD_K: f32 = 6.0;
const ILD: f32 = 0.5;
/// Tap distinct from the ITD taps (`0..=6`) holding `cos(az)` so front ≠ back.
const MARKER: usize = 16;

fn push_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn push_f32(b: &mut Vec<u8>, v: f32) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// Match `direction_from_degrees` in `panner.rs`: 0 = front/−z, +90 = left/−x.
fn cartesian(azimuth: Azimuth, elevation: Elevation) -> [f32; 3] {
    let az = Radians::from(azimuth);
    let el = Radians::from(elevation);
    let cos_el = el.cos();
    [-az.sin() * cos_el, el.sin(), -az.cos() * cos_el]
}

/// Encode one vertex IR. `None` is a pole (median plane, no ITD/ILD/marker).
fn encode_hrir(azimuth: Option<Azimuth>) -> (Vec<f32>, Vec<f32>) {
    let mut left = vec![0.0f32; IR_LEN];
    let mut right = vec![0.0f32; IR_LEN];
    let Some(azimuth) = azimuth else {
        left[0] = 1.0;
        right[0] = 1.0;
        return (left, right);
    };
    let az = azimuth.wrap();
    let sin_az = Radians::from(az).sin();
    let cos_az = Radians::from(az).cos();
    let itd = (ITD_K * sin_az).round() as i32;
    let left_gain = 1.0 + ILD * sin_az;
    let right_gain = 1.0 - ILD * sin_az;
    if itd >= 0 {
        left[0] = left_gain;
        right[itd as usize] = right_gain;
    } else {
        left[(-itd) as usize] = left_gain;
        right[0] = right_gain;
    }
    left[MARKER] = cos_az;
    (left, right)
}

fn equator_azimuths() -> [Azimuth; 8] {
    [
        Azimuth::FRONT,
        Azimuth(45.0),
        Azimuth(90.0),
        Azimuth(135.0),
        Azimuth(180.0),
        Azimuth(-135.0),
        Azimuth(-90.0),
        Azimuth(-45.0),
    ]
}

/// Format-valid HRIR sphere: 8 equatorial vertices + poles, IRs from [`encode_hrir`].
fn synthetic_itd_ild_sphere() -> Vec<u8> {
    let equator = equator_azimuths();
    let mut verts: Vec<[f32; 3]> = equator
        .iter()
        .map(|&az| cartesian(az, Elevation::LEVEL))
        .collect();
    verts.push(cartesian(Azimuth::FRONT, Elevation::UP));
    verts.push(cartesian(Azimuth::FRONT, Elevation::DOWN));

    let up = 8u32;
    let down = 9u32;
    let mut faces = Vec::new();
    for i in 0..8u32 {
        let j = (i + 1) % 8;
        faces.push([up, i, j]);
        faces.push([down, j, i]);
    }

    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    push_u32(&mut b, SAMPLE_RATE);
    push_u32(&mut b, IR_LEN as u32);
    push_u32(&mut b, verts.len() as u32);
    push_u32(&mut b, (faces.len() * 3) as u32);
    for f in faces {
        for idx in f {
            push_u32(&mut b, idx);
        }
    }
    for (i, v) in verts.iter().enumerate() {
        push_f32(&mut b, v[0]);
        push_f32(&mut b, v[1]);
        push_f32(&mut b, v[2]);
        let (left, right) = if i < 8 {
            encode_hrir(Some(equator[i]))
        } else {
            encode_hrir(None)
        };
        for s in left {
            push_f32(&mut b, s);
        }
        for s in right {
            push_f32(&mut b, s);
        }
    }
    b
}

fn make_node() -> HrtfBinauralNode {
    let bytes = synthetic_itd_ild_sphere();
    HrtfBinauralNode::new(&bytes, SampleRate(f64::from(SAMPLE_RATE)))
        .expect("synthetic sphere parses")
}

/// Recover the stereo IR at a settled bearing. Reuses `node` so a sweep does
/// not rebuild the sphere on every step.
fn ir_at(
    node: &mut HrtfBinauralNode,
    azimuth: Azimuth,
    elevation: Elevation,
) -> (Vec<f32>, Vec<f32>) {
    node.set_position(azimuth, elevation);
    node.reset();
    let mut out = [0.0f32; 2];
    // Two silent frames: the first still cross-fades from front (`prev_dir`
    // resets to forward); the second is the settled direction.
    for _ in 0..(FRAME_LEN * 2) {
        node.tick(&[0.0, 0.0], &mut out);
    }
    node.tick(&[1.0, 1.0], &mut out);
    for _ in 1..FRAME_LEN {
        node.tick(&[0.0, 0.0], &mut out);
    }
    // Last tick rendered the impulse frame and returned IR[0].
    let mut left = Vec::with_capacity(IR_LEN);
    let mut right = Vec::with_capacity(IR_LEN);
    left.push(out[0]);
    right.push(out[1]);
    for _ in 1..IR_LEN {
        node.tick(&[0.0, 0.0], &mut out);
        left.push(out[0]);
        right.push(out[1]);
    }
    (left, right)
}

fn peak_index(ir: &[f32]) -> Samples {
    Samples(
        ir.iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .expect("IR is non-empty"),
    )
}

fn peak_value(ir: &[f32]) -> f32 {
    ir.iter().copied().fold(0.0f32, |a, s| a.max(s.abs()))
}

fn energy(left: &[f32], right: &[f32]) -> f32 {
    left.iter().chain(right).map(|s| s * s).sum()
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max)
}

fn assert_ir_eq(got: &[f32], expected: &[f32], label: &str) {
    assert_eq!(got.len(), expected.len(), "{label}: length");
    for (i, (g, e)) in got.iter().zip(expected).enumerate() {
        assert!(
            g.is_finite() && e.is_finite(),
            "{label}[{i}]: non-finite {g} vs {e}"
        );
        assert_abs_diff_eq!(*g, *e, epsilon = 1e-6);
    }
}

fn assert_between(mid: &[f32], a: &[f32], b: &[f32], ear: &str) {
    for (i, ((m, na), nb)) in mid.iter().zip(a).zip(b).enumerate() {
        let lo = na.min(*nb) - 1e-6;
        let hi = na.max(*nb) + 1e-6;
        assert!(
            *m >= lo && *m <= hi,
            "{ear}[{i}] = {m} is not between neighbours {na} and {nb}"
        );
    }
}

/// At a measured equatorial vertex the interpolated IR equals that vertex's
/// encoded IR within 1e-6.
///
/// Mutation: snap azimuth to a 90° grid in `direction_from_degrees` → fails at 45° (rounds to 90°, left peak 1.5 not 1.354).
#[test]
fn measured_vertex_ir_matches_the_encoded_hrir() {
    let mut node = make_node();
    for az in equator_azimuths() {
        let (left, right) = ir_at(&mut node, az, Elevation::LEVEL);
        let (exp_l, exp_r) = encode_hrir(Some(az));
        assert_ir_eq(&left, &exp_l, &format!("left @ {}deg", az.get()));
        assert_ir_eq(&right, &exp_r, &format!("right @ {}deg", az.get()));
    }
}

/// Midway between two measured points the IR is a blend of the neighbours,
/// not a snap to either one: every sample lies in the neighbour min/max, and
/// both neighbour taps are present.
///
/// Mutation: snap azimuth to a 45° grid in `direction_from_degrees` → fails at 22.5° (rounds to 45°, drops the t=0 right-ear tap).
#[test]
fn midway_azimuth_interpolates_between_neighbours() {
    let mut node = make_node();
    let (l0, r0) = ir_at(&mut node, Azimuth::FRONT, Elevation::LEVEL);
    let (l45, r45) = ir_at(&mut node, Azimuth(45.0), Elevation::LEVEL);
    let (lmid, rmid) = ir_at(&mut node, Azimuth(22.5), Elevation::LEVEL);

    assert_between(&lmid, &l0, &l45, "left");
    assert_between(&rmid, &r0, &r45, "right");

    // Front contributes a right-ear tap at t=0; 45° contributes one at t=4
    // (`round(k·sin(45°))` = 4). A snap to either vertex keeps only one.
    assert!(
        rmid[0].abs() > 0.2 && rmid[4].abs() > 0.2,
        "22.5deg right ear should carry both neighbour taps, got t0={} t4={}",
        rmid[0],
        rmid[4]
    );
}

/// A source at +90° (left, this crate's convention) reaches the left ear
/// earlier than the right by the encoded sample count; at −90° (right) the
/// opposite. `Azimuth` has no `Ord` — the sign lives in which ear peaks first,
/// not in a comparison of bearings.
///
/// Mutation: flip the sign of `x` in `direction_from_degrees` (`-az.sin()` → `az.sin()`) → +90° delays the left ear (peak t=6).
#[test]
fn itd_sign_follows_the_near_ear() {
    let mut node = make_node();
    let expected = Samples((ITD_K).round() as usize);

    let (l_left, r_left) = ir_at(&mut node, Azimuth(90.0), Elevation::LEVEL);
    let left_at_left = peak_index(&l_left);
    let right_at_left = peak_index(&r_left);
    assert_eq!(
        left_at_left,
        Samples(0),
        "+90deg (left): near (left) ear should peak at t=0, peaked at {left_at_left}"
    );
    assert_eq!(
        right_at_left, expected,
        "+90deg (left): far (right) ear should be delayed by {expected}, peaked at {right_at_left}"
    );

    let (l_right, r_right) = ir_at(&mut node, Azimuth(-90.0), Elevation::LEVEL);
    let left_at_right = peak_index(&l_right);
    let right_at_right = peak_index(&r_right);
    assert_eq!(
        right_at_right,
        Samples(0),
        "-90deg (right): near (right) ear should peak at t=0, peaked at {right_at_right}"
    );
    assert_eq!(
        left_at_right, expected,
        "-90deg (right): far (left) ear should be delayed by {expected}, peaked at {left_at_right}"
    );
}

/// The near ear is louder by the encoded factor `(1+ILD)/(1−ILD)`.
///
/// Mutation: flip the sign of `x` in `direction_from_degrees` → at +90° the far ear is louder (L/R ratio 1/3 not 3).
#[test]
fn ild_sign_makes_the_near_ear_louder() {
    let mut node = make_node();
    let encoded_ratio = (1.0 + ILD) / (1.0 - ILD);

    let (l_left, r_left) = ir_at(&mut node, Azimuth(90.0), Elevation::LEVEL);
    let ratio_left = peak_value(&l_left) / peak_value(&r_left);
    assert_abs_diff_eq!(ratio_left, encoded_ratio, epsilon = 1e-5);
    assert!(
        peak_value(&l_left) > peak_value(&r_left),
        "+90deg (left): left ear should be louder, L={} R={}",
        peak_value(&l_left),
        peak_value(&r_left)
    );

    let (l_right, r_right) = ir_at(&mut node, Azimuth(-90.0), Elevation::LEVEL);
    let ratio_right = peak_value(&r_right) / peak_value(&l_right);
    assert_abs_diff_eq!(ratio_right, encoded_ratio, epsilon = 1e-5);
    assert!(
        peak_value(&r_right) > peak_value(&l_right),
        "-90deg (right): right ear should be louder, L={} R={}",
        peak_value(&l_right),
        peak_value(&r_right)
    );
}

/// Azimuths 359° and −1° are the same bearing after wrap; 359° and 1° sit 2°
/// apart across 0°/360° and must both render as *front* (marker ≈ +1), never
/// as back (marker ≈ −1). Poles are well-defined: finite samples, finite
/// energy, no NaN, for any azimuth at ±90° elevation.
///
/// Mutation: fold azimuth with `rem_euclid(180)` in `direction_from_degrees` → 359° maps to 179° (back, marker −0.993) while 1° stays front.
#[test]
fn wrap_seam_is_continuous_and_poles_are_finite() {
    let mut node = make_node();

    let (l_359, r_359) = ir_at(&mut node, Azimuth(359.0), Elevation::LEVEL);
    let (l_m1, r_m1) = ir_at(&mut node, Azimuth(-1.0), Elevation::LEVEL);
    let (l_1, r_1) = ir_at(&mut node, Azimuth(1.0), Elevation::LEVEL);
    let (l_180, r_180) = ir_at(&mut node, Azimuth(180.0), Elevation::LEVEL);

    assert_ir_eq(&l_359, &l_m1, "left 359 vs -1 (wrap identity)");
    assert_ir_eq(&r_359, &r_m1, "right 359 vs -1 (wrap identity)");

    // Both sides of 0° are front: the cos(az) marker is +1, not the back's −1.
    assert!(
        l_359[MARKER] > 0.9 && l_1[MARKER] > 0.9,
        "359deg marker {} and 1deg marker {} should both be front (≈+1), not back (≈-1)",
        l_359[MARKER],
        l_1[MARKER]
    );
    let seam = max_abs_diff(&l_359, &l_1).max(max_abs_diff(&r_359, &r_1));
    let to_back = max_abs_diff(&l_359, &l_180).max(max_abs_diff(&r_359, &r_180));
    assert!(
        seam < 0.15 && seam < to_back * 0.25,
        "359deg vs 1deg differ by {seam} (should be a 2° front step); \
         359deg vs 180deg differ by {to_back}. A wrap that clamps 359→180 \
         would put 359 on the back of the sphere."
    );

    for (az, el, name) in [
        (Azimuth::FRONT, Elevation::UP, "up/front"),
        (Azimuth(90.0), Elevation::UP, "up/left"),
        (Azimuth(359.0), Elevation::UP, "up/359"),
        (Azimuth::FRONT, Elevation::DOWN, "down/front"),
        (Azimuth(-90.0), Elevation::DOWN, "down/right"),
        (Azimuth(180.0), Elevation::DOWN, "down/back"),
    ] {
        let (l, r) = ir_at(&mut node, az, el);
        assert!(
            l.iter().chain(&r).all(|s| s.is_finite()),
            "pole {name} produced a non-finite sample"
        );
        let e = energy(&l, &r);
        assert!(
            e.is_finite() && e > 0.5,
            "pole {name} energy {e} should be finite and audible"
        );
    }
}

/// Sweeping azimuth in 1° steps: no sample-wise jump larger than a quarter of
/// the largest adjacent-vertex difference (catches nearest-neighbour snaps),
/// and L+R energy stays finite inside `(1, 4]` — encoded energy is
/// `2 + 2·(ILD·sin(az))² + cos²(az)` ∈ `[2.25, 3]` on the equator, `2` at the
/// poles; the bound leaves room for interpolation without accepting silence
/// or a doubled IR.
///
/// Mutation: snap azimuth to a 45° grid in `direction_from_degrees` → 1° step to 23° jumps by the full neighbour difference (1.0, bound 0.25).
#[test]
fn azimuth_sweep_is_continuous_and_energy_bounded() {
    let mut node = make_node();

    let mut neighbour_diff = 0.0f32;
    let equator = equator_azimuths();
    for i in 0..equator.len() {
        let a = ir_at(&mut node, equator[i], Elevation::LEVEL);
        let b = ir_at(
            &mut node,
            equator[(i + 1) % equator.len()],
            Elevation::LEVEL,
        );
        neighbour_diff = neighbour_diff
            .max(max_abs_diff(&a.0, &b.0))
            .max(max_abs_diff(&a.1, &b.1));
    }
    assert!(
        neighbour_diff > 0.2,
        "adjacent vertices should differ; got {neighbour_diff}"
    );

    let jump_bound = 0.25 * neighbour_diff;
    let mut worst_jump = 0.0f32;
    let mut worst_jump_at = 0i32;
    let mut max_energy = 0.0f32;
    let mut min_energy = f32::MAX;
    let mut prev = ir_at(&mut node, Azimuth::FRONT, Elevation::LEVEL);

    for deg in 1..=360 {
        // Integer degrees, then wrap via Azimuth — never compare bearings.
        let cur = ir_at(&mut node, Azimuth(deg as f32), Elevation::LEVEL);
        let jump = max_abs_diff(&prev.0, &cur.0).max(max_abs_diff(&prev.1, &cur.1));
        if jump > worst_jump {
            worst_jump = jump;
            worst_jump_at = deg;
        }
        assert!(
            jump <= jump_bound,
            "1deg step to {deg}deg jumped {jump}, bound is {jump_bound} \
             (¼ of neighbour difference {neighbour_diff}). Nearest-neighbour \
             selection jumps by the full neighbour difference at the midpoint."
        );

        let e = energy(&cur.0, &cur.1);
        assert!(
            e.is_finite() && (1.0..4.0).contains(&e),
            "az {deg}deg: L+R energy {e} is outside (1, 4]"
        );
        max_energy = max_energy.max(e);
        min_energy = min_energy.min(e);
        assert!(
            cur.0.iter().chain(&cur.1).all(|s| s.is_finite()),
            "az {deg}deg produced a non-finite sample"
        );
        prev = cur;
    }

    println!(
        "worst 1deg jump: {worst_jump:.4} at {worst_jump_at}deg \
         (bound {jump_bound:.4}, neighbour {neighbour_diff:.4}); \
         energy [{min_energy:.3}, {max_energy:.3}]"
    );
}
