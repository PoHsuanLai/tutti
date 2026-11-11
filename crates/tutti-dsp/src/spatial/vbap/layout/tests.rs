//! Pulkki's properties of vector base amplitude panning, held to the
//! layouts' own figures: the tuple sets are hand-derived from the method (see
//! `select.rs`) and every other case is a property of the published method,
//! not a comparison with another implementation.

use super::*;
use tutti_types::{Azimuth, Direction, Elevation, Spread};

type Position = (f32, f32);

const STEREO: [Position; 2] = [(30.0, 0.0), (-30.0, 0.0)];
const QUAD: [Position; 4] = [(45.0, 0.0), (-45.0, 0.0), (135.0, 0.0), (-135.0, 0.0)];
const FIVE: [Position; 5] = [
    (30.0, 0.0),
    (-30.0, 0.0),
    (0.0, 0.0),
    (110.0, 0.0),
    (-110.0, 0.0),
];
const SEVEN: [Position; 7] = [
    (30.0, 0.0),
    (-30.0, 0.0),
    (0.0, 0.0),
    (90.0, 0.0),
    (-90.0, 0.0),
    (150.0, 0.0),
    (-150.0, 0.0),
];
const ATMOS: [Position; 11] = [
    (30.0, 0.0),
    (-30.0, 0.0),
    (0.0, 0.0),
    (90.0, 0.0),
    (-90.0, 0.0),
    (150.0, 0.0),
    (-150.0, 0.0),
    (45.0, 45.0),
    (-45.0, 45.0),
    (135.0, 45.0),
    (-135.0, 45.0),
];

fn layout(positions: &[Position]) -> SpeakerLayout {
    let p: Vec<_> = positions
        .iter()
        .map(|&(a, e)| Direction::new(Azimuth(a), Elevation(e)))
        .collect();
    SpeakerLayout::new(&p).unwrap()
}

fn presets() -> Vec<(&'static str, SpeakerLayout, &'static [Position])> {
    vec![
        ("stereo", SpeakerLayout::stereo(), &STEREO),
        ("quad", SpeakerLayout::quad(), &QUAD),
        ("5.1", SpeakerLayout::surround_5_1(), &FIVE),
        ("7.1", SpeakerLayout::surround_7_1(), &SEVEN),
        ("7.1.4", SpeakerLayout::atmos_7_1_4(), &ATMOS),
    ]
}

fn tuples_of(layout: &SpeakerLayout) -> Vec<Vec<u16>> {
    layout
        .tuples()
        .iter()
        .map(|t| t.speakers().to_vec())
        .collect()
}

/// The solve's own gains (clamped, unit power), in `f64`, for a direction in
/// degrees.
fn raw(layout: &SpeakerLayout, azimuth: f64, elevation: f64) -> Vec<f64> {
    let mut gains = vec![0.0; layout.speaker_count()];
    layout.tuple_gains(azimuth, elevation, &mut gains);
    gains
}

/// Bearing and height in degrees of a vector.
fn degrees(v: [f64; 3]) -> (f64, f64) {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    (
        v[0].atan2(v[1]).to_degrees(),
        (v[2] / length).asin().to_degrees(),
    )
}

fn direction(azimuth: f32, elevation: f32) -> [f64; 3] {
    super::super::math::direction(f64::from(azimuth), f64::from(elevation))
}

/// The directions each layout covers with tuples: the whole circle for a
/// horizontal layout (stereo: between its speakers), the upper hemisphere
/// for Atmos (no speaker sits below the horizon).
fn covered(name: &str) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    match name {
        "stereo" => out.extend((-30..30).map(|a| (f64::from(a) + 0.13, 0.0))),
        "7.1.4" => {
            for a in -180..180 {
                for e in 0..=90 {
                    out.push((f64::from(a) + 0.37, f64::from(e)));
                }
            }
        }
        _ => out.extend((-180..180).map(|a| (f64::from(a) + 0.37, 0.0))),
    }
    out
}

/// The tuple sets are the method's, derived by hand: a horizontal layout's
/// neighbours by bearing (stereo's pair is listed once, the long way round
/// being 300 degrees), and Atmos 7.1.4's triangulation of the upper
/// hemisphere. Its speakers are 0 L, 1 R, 2 C, 3 Lss, 4 Rss, 5 Lrs, 6 Rrs,
/// 7 Ltf, 8 Rtf, 9 Ltr, 10 Rtr. The overhead four are one coplanar square
/// (7, 9, 10, 8) and the rear wall (5, 6, 10, 9) another, so two of the
/// three-speaker combinations in each tile it and the shortest diagonal decides,
/// the two diagonals of each being equal and the lower indices winning:
/// 7-10 and 5-10. The front wall is (0,2,7), (2,7,8) and (1,2,8), and each
/// side wall three triangles. Thirteen triangles, none overlapping, none with a
/// speaker inside.
#[test]
fn the_tuple_sets_of_the_presets_are_the_hand_derived_ones() {
    let want: [(&str, Vec<Vec<u16>>); 5] = [
        ("stereo", vec![vec![1, 0]]),
        ("quad", vec![vec![3, 1], vec![1, 0], vec![0, 2], vec![2, 3]]),
        (
            "5.1",
            vec![vec![4, 1], vec![1, 2], vec![2, 0], vec![0, 3], vec![3, 4]],
        ),
        (
            "7.1",
            vec![
                vec![6, 4],
                vec![4, 1],
                vec![1, 2],
                vec![2, 0],
                vec![0, 3],
                vec![3, 5],
                vec![5, 6],
            ],
        ),
        (
            "7.1.4",
            vec![
                vec![0, 2, 7],
                vec![0, 3, 7],
                vec![1, 2, 8],
                vec![1, 4, 8],
                vec![2, 7, 8],
                vec![3, 5, 9],
                vec![3, 7, 9],
                vec![4, 6, 10],
                vec![4, 8, 10],
                vec![5, 6, 10],
                vec![5, 9, 10],
                vec![7, 8, 10],
                vec![7, 9, 10],
            ],
        ),
    ];
    for ((name, layout, _), (want_name, want)) in presets().iter().zip(want) {
        assert_eq!(*name, want_name);
        assert_eq!(tuples_of(layout), want, "{name}");
    }
}

/// Pulkki's normalisation: inside the coverage of its tuples the gains'
/// squares sum to 1.
#[test]
fn the_sum_of_squared_gains_is_one() {
    for (name, layout, _) in presets() {
        for (a, e) in covered(name) {
            let sum: f64 = raw(&layout, a, e).iter().map(|g| g * g).sum();
            assert!((sum - 1.0).abs() < 1e-12, "{name} at ({a}, {e}): {sum}");
        }
    }
}

/// The corrected solve, `gains_into`, is unit power at every bearing and
/// height for every preset, at every spread (Pulkki: the sum of the squared
/// gains is 1, so a source's loudness does not depend on where it is). The
/// grid is an enumeration, not a sample: five presets, seven spreads (0 and
/// the ends included, where the diffusion blend returns early or is total),
/// 24 bearings across the seam, and heights from straight down to straight up,
/// which reaches the regions the raw solve answers with silence (under the
/// Atmos bed, the poles of a horizontal ring) and the rear of a front-only
/// pair. The bound is `f32` accumulation over at most 12 gains, each narrowed
/// from `f64`; the failures it is here for (a missing or doubled
/// normalisation, a clamped-away arc) are order 1.
#[test]
fn the_corrected_gains_are_unit_power_everywhere() {
    for (name, layout, _) in presets() {
        let mut gains = vec![0.0; layout.speaker_count()];
        for spread in [0.0, 0.01, 0.25, 0.5, 0.75, 0.99, 1.0] {
            for az in (0..360).step_by(15) {
                for el in (-90..=90).step_by(30) {
                    layout.gains_into(
                        Azimuth(az as f32),
                        Elevation(el as f32),
                        Spread::new_clamped(spread),
                        &mut gains,
                    );
                    let sum: f32 = gains.iter().map(|g| g * g).sum();
                    assert!(
                        (sum - 1.0).abs() < 1e-3,
                        "{name} at ({az}, {el}), spread {spread}: power {sum}, gains {gains:?}"
                    );
                }
            }
        }
    }
}

/// A front-only pair has no depth cue, so `gains_into` mirrors a rear bearing
/// into the front and hard-pans what is still outside the pair, at full
/// energy: dead ahead and dead behind are the same equal-power centre, 30
/// degrees and everything beyond it on that side is the one speaker, and a
/// pan through 180 degrees never goes silent. The figures are the pan law
/// at 15 degrees (`0.939071`, `0.343724`) and the equal-power centre.
#[test]
fn a_front_only_pair_mirrors_the_rear_and_hard_pans_outside_it() {
    let layout = SpeakerLayout::stereo();
    let centre = core::f32::consts::FRAC_1_SQRT_2;
    // (azimuth, left, right)
    for (az, left, right) in [
        (0.0f32, centre, centre),
        (15.0, 0.939_071, 0.343_724),
        (30.0, 1.0, 0.0),
        (45.0, 1.0, 0.0),
        (90.0, 1.0, 0.0),
        (135.0, 1.0, 0.0),
        (165.0, 0.939_071, 0.343_724),
        (180.0, centre, centre),
        (195.0, 0.343_724, 0.939_071),
        (225.0, 0.0, 1.0),
        (270.0, 0.0, 1.0),
        (315.0, 0.0, 1.0),
        (345.0, 0.343_724, 0.939_071),
    ] {
        let mut got = [0.0f32; 2];
        layout.gains_into(Azimuth(az), Elevation(0.0), Spread::POINT, &mut got);
        assert!(
            (got[0] - left).abs() < 1e-3 && (got[1] - right).abs() < 1e-3,
            "stereo at {az}: {got:?}, want [{left}, {right}]"
        );
    }
}

/// The fold is for a front-only array and nothing else: quad has speakers
/// behind the listener, so a rear bearing lights the rear pair and a front
/// bearing the front pair (speakers FL, FR, RL, RR).
#[test]
fn a_surrounding_layout_does_not_fold_its_rear_arc() {
    let layout = SpeakerLayout::quad();
    let at = |az: f32| {
        let mut g = [0.0f32; 4];
        layout.gains_into(Azimuth(az), Elevation(0.0), Spread::POINT, &mut g);
        g
    };
    let front = at(0.0);
    assert!(
        front[0] + front[1] > 0.9 && front[2] + front[3] < 0.1,
        "{front:?}"
    );
    let rear = at(180.0);
    assert!(
        rear[2] + rear[3] > 0.9 && rear[0] + rear[1] < 0.1,
        "{rear:?}"
    );
    let rear_left = at(135.0);
    assert!(rear_left[2] > 0.9, "{rear_left:?}");
}

/// The gains are never negative, in or out of the coverage (the clamp), and
/// the corrected solve's are unit power everywhere.
#[test]
fn gains_are_not_negative() {
    for (name, layout, _) in presets() {
        for a in (-180..180).step_by(3) {
            for e in (-90..=90).step_by(5) {
                let (a, e) = (f64::from(a) + 0.37, f64::from(e));
                for g in raw(&layout, a, e) {
                    assert!(g >= 0.0, "{name} at ({a}, {e}): {g}");
                }
                let mut all = vec![0.0; layout.speaker_count()];
                layout.gains_into(
                    Azimuth(a as f32),
                    Elevation(e as f32),
                    Spread::POINT,
                    &mut all,
                );
                assert!(all.iter().all(|g| *g >= 0.0), "{name} at ({a}, {e})");
            }
        }
    }
}

/// At most two gains are non-zero for a horizontal layout and three for one
/// with height: a source is placed by one tuple.
#[test]
fn a_source_uses_at_most_a_tuple() {
    for (name, layout, _) in presets() {
        let most = match layout.mode() {
            PanningMode::Horizontal => 2,
            PanningMode::Spherical => 3,
        };
        for a in -180..180 {
            for e in (-90..=90).step_by(5) {
                let (a, e) = (f64::from(a) + 0.37, f64::from(e));
                let lit = raw(&layout, a, e).iter().filter(|g| **g > 0.0).count();
                assert!(lit <= most, "{name} at ({a}, {e}): {lit} gains");
            }
        }
    }
}

/// A source at a speaker puts all its power there, through the corrected
/// solve too.
#[test]
fn a_source_at_a_speaker_puts_all_its_gain_there() {
    for (name, layout, positions) in presets() {
        for (s, &(a, e)) in positions.iter().enumerate() {
            let mut gains = vec![0.0; layout.speaker_count()];
            layout.gains_into(Azimuth(a), Elevation(e), Spread::POINT, &mut gains);
            for (other, g) in gains.iter().enumerate() {
                let want = if other == s { 1.0 } else { 0.0 };
                assert!(
                    (g - want).abs() < 1e-6,
                    "{name}: source at speaker {s}, gain {other} is {g}"
                );
            }
        }
    }
}

/// The gains are continuous: no tuple change moves a gain by a step. A walk
/// of 0.05 degrees over the coverage (along bearing at several heights and
/// along height at several bearings) changes no gain by more than 0.012, a
/// slope of 14 per radian that the steepest tuple (a thin triangle near the
/// pole) stays under and a jump between overlapping tuples (0.1 and up) does
/// not.
#[test]
fn gains_are_continuous_across_every_tuple_edge() {
    const STEP: f64 = 0.05;
    for (name, layout, _) in presets() {
        let mut worst = 0.0f64;
        let mut walk = |points: &mut dyn Iterator<Item = (f64, f64)>| {
            let mut last: Option<Vec<f64>> = None;
            for (a, e) in points {
                let g = raw(&layout, a, e);
                if let Some(last) = &last {
                    let step = last
                        .iter()
                        .zip(&g)
                        .map(|(x, y)| (x - y).abs())
                        .fold(0.0, f64::max);
                    worst = worst.max(step);
                }
                last = Some(g);
            }
        };
        let steps = (360.0 / STEP) as i32;
        let ceiling = if name == "7.1.4" { 80 } else { 0 };
        let covered_by = |e: f64| name != "7.1.4" || e >= 0.0;
        for e in (0..=ceiling).step_by(10).map(f64::from) {
            if covered_by(e) && name != "stereo" {
                walk(&mut (0..steps).map(|i| (-180.0 + f64::from(i) * STEP + 0.0137, e)));
            }
        }
        if name == "stereo" {
            walk(&mut (-600..=600).map(|i| (f64::from(i) * STEP / 10.0, 0.0)));
        }
        if name == "7.1.4" {
            for a in (-180..180).step_by(7).map(|a| f64::from(a) + 0.37) {
                walk(&mut (0..1600).map(|i| (a, f64::from(i) * STEP)));
            }
        }
        assert!(worst < 0.012, "{name}: a gain moved {worst} in one step");
    }
}

/// Straddling an edge shared by two triangles by a millionth of a radian, the
/// gains agree: the tuples share the edge and its two speakers' gains.
#[test]
fn a_shared_edge_has_the_same_gains_on_both_sides() {
    let layout = SpeakerLayout::atmos_7_1_4();
    let mut edges: Vec<((u16, u16), usize)> = Vec::new();
    for t in layout.tuples() {
        let s = t.speakers();
        for (i, j) in [(0, 1), (0, 2), (1, 2)] {
            let e = (s[i].min(s[j]), s[i].max(s[j]));
            match edges.iter_mut().find(|(k, _)| *k == e) {
                Some((_, n)) => *n += 1,
                None => edges.push((e, 1)),
            }
        }
    }
    let shared: Vec<_> = edges.iter().filter(|(_, n)| *n == 2).collect();
    assert!(shared.len() >= 10, "only {} shared edges", shared.len());
    for &&((i, j), _) in &shared {
        let (pi, pj) = (ATMOS[usize::from(i)], ATMOS[usize::from(j)]);
        let (a, b) = (direction(pi.0, pi.1), direction(pj.0, pj.1));
        let mid = [a[0] + b[0], a[1] + b[1], a[2] + b[2]];
        let normal = super::super::math::cross(a, b);
        let along = super::super::math::cross(normal, mid);
        let length = (along[0] * along[0] + along[1] * along[1] + along[2] * along[2]).sqrt();
        let eps = 1e-6;
        let side = |s: f64| {
            let p = [
                mid[0] + s * eps * along[0] / length,
                mid[1] + s * eps * along[1] / length,
                mid[2] + s * eps * along[2] / length,
            ];
            let (az, el) = degrees(p);
            raw(&layout, az, el)
        };
        for (x, y) in side(1.0).iter().zip(side(-1.0)) {
            assert!((x - y).abs() < 1e-4, "edge {i}-{j}: {x} against {y}");
        }
    }
}

/// A speaker inside a spherical triangle makes it two smaller ones: a layout
/// whose speakers are one triangle's corners with another at the zenith has
/// the three triangles around the zenith and not the large one over it. (A
/// speaker on the sphere is never inside a planar triangle of three others, so
/// "inside" is "beyond the corners' plane".)
#[test]
fn a_triangle_with_a_speaker_inside_is_split() {
    let cap = layout(&[(0.0, 20.0), (120.0, 20.0), (-120.0, 20.0), (0.0, 90.0)]);
    assert_eq!(
        tuples_of(&cap),
        vec![vec![0, 1, 3], vec![0, 2, 3], vec![1, 2, 3]]
    );
}

/// A layout the method cannot tile is refused with the reason.
#[test]
fn a_layout_that_cannot_be_tiled_is_refused() {
    let make = |p: &[Position]| {
        let p: Vec<_> = p
            .iter()
            .map(|&(a, e)| Direction::new(Azimuth(a), Elevation(e)))
            .collect();
        SpeakerLayout::new(&p).unwrap_err()
    };
    assert_eq!(
        make(&[(0.0, 0.0)]),
        SpeakerGeometryError::InsufficientSpeakers {
            provided: 1,
            required: 2
        }
    );
    assert_eq!(
        make(&[(0.0, 0.0), (90.0, 30.0)]),
        SpeakerGeometryError::InsufficientSpeakers {
            provided: 2,
            required: 3
        }
    );
    assert_eq!(
        make(&[(30.0, 0.0), (30.0, 0.0)]),
        SpeakerGeometryError::NoValidTuples
    );
    // Two speakers in one place: every triangle has the other beyond a corner.
    assert_eq!(
        make(&[(0.0, 20.0), (120.0, 20.0), (-120.0, 20.0), (0.0, 20.0)]),
        SpeakerGeometryError::NoValidTuples
    );
    // One great circle through the listener.
    assert_eq!(
        make(&[(0.0, 0.0), (180.0, 0.0), (0.0, 90.0)]),
        SpeakerGeometryError::NoValidTuples
    );
    assert_eq!(
        make(&[(f32::NAN, 0.0), (30.0, 0.0)]),
        SpeakerGeometryError::NonFinitePosition
    );
    let many = vec![(0.0, 0.0); MAX_SPEAKERS + 1];
    assert_eq!(
        make(&many),
        SpeakerGeometryError::TooManySpeakers {
            provided: MAX_SPEAKERS + 1,
            max: MAX_SPEAKERS
        }
    );
}
