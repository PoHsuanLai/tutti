//! Pulkki's properties of vector base amplitude panning, held to the
//! layouts' own figures: the tuple sets are hand-derived from the method (see
//! `select.rs`) and every other case is a property of the published method,
//! not a comparison with another implementation.

use super::*;
use tutti_types::{Azimuth, Elevation, Spread};

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
        .map(|&(a, e)| (Azimuth(a), Elevation(e)))
        .collect();
    SpeakerLayout::new(&p).unwrap()
}

fn presets() -> Vec<(&'static str, SpeakerLayout, &'static [Position])> {
    vec![
        ("stereo", SpeakerLayout::stereo().unwrap(), &STEREO),
        ("quad", SpeakerLayout::quad().unwrap(), &QUAD),
        ("5.1", SpeakerLayout::surround_5_1().unwrap(), &FIVE),
        ("7.1", SpeakerLayout::surround_7_1().unwrap(), &SEVEN),
        ("7.1.4", SpeakerLayout::atmos_7_1_4().unwrap(), &ATMOS),
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
    super::math::direction(f64::from(azimuth), f64::from(elevation))
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
///
/// Mutations (each run, each fails): the pair's long-way-round gap test dropped
/// (stereo lists `[0, 1]` again); the crossing rule of `accepted_edges` removed
/// (the square lists all four of its triangles).
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
///
/// Mutation (run, fails): the solve's `norm` fixed at `1.0` (the tuple's raw
/// solution, whose power is 1 only on a speaker).
#[test]
fn the_sum_of_squared_gains_is_one() {
    for (name, layout, _) in presets() {
        for (a, e) in covered(name) {
            let sum: f64 = raw(&layout, a, e).iter().map(|g| g * g).sum();
            assert!((sum - 1.0).abs() < 1e-12, "{name} at ({a}, {e}): {sum}");
        }
    }
}

/// The gains are never negative, in or out of the coverage (the clamp), and
/// the corrected solve's are unit power everywhere.
///
/// Mutation (run, fails): the clamp `.max(0.0)` removed (outside the coverage
/// a gain is negative).
#[test]
fn gains_are_not_negative() {
    for (name, layout, _) in presets() {
        for a in (-180..180).step_by(3) {
            for e in (-90..=90).step_by(5) {
                let (a, e) = (f64::from(a) + 0.37, f64::from(e));
                for g in raw(&layout, a, e) {
                    assert!(g >= 0.0, "{name} at ({a}, {e}): {g}");
                }
                let all = layout.gains(Azimuth(a as f32), Elevation(e as f32), Spread::POINT);
                assert!(all.iter().all(|g| *g >= 0.0), "{name} at ({a}, {e})");
            }
        }
    }
}

/// At most two gains are non-zero for a horizontal layout and three for one
/// with height: a source is placed by one tuple.
///
/// Mutation (run, fails): every tuple's gains written out (the loop's
/// `best` replaced by an accumulation), so neighbours' gains survive.
#[test]
fn a_source_uses_at_most_a_tuple() {
    for (name, layout, _) in presets() {
        let most = match layout.mode() {
            PanningMode::Horizontal => 2,
            PanningMode::Surround => 3,
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
///
/// Mutation (run, fails): the second and third columns of a triplet's matrix
/// swapped in `SpeakerLayout::new` (Atmos's heights land on the wrong
/// speakers).
#[test]
fn a_source_at_a_speaker_puts_all_its_gain_there() {
    for (name, layout, positions) in presets() {
        for (s, &(a, e)) in positions.iter().enumerate() {
            let gains = layout.gains(Azimuth(a), Elevation(e), Spread::POINT);
            for (other, g) in gains.iter().enumerate().take(positions.len()) {
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
///
/// Mutations (each run, each fails): the crossing rule removed from
/// `accepted_edges` (the overhead square keeps both diagonals, and the tuple
/// that wins flips between two triangulations along the line where their
/// smallest gains meet).
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
///
/// Mutation (run, fails): the `beyond > EPS` rejection removed from `facets`
/// (a triangle across the overhead cap, with speakers behind its plane,
/// overlaps its neighbours and the gain jumps at their shared edge).
#[test]
fn a_shared_edge_has_the_same_gains_on_both_sides() {
    let layout = SpeakerLayout::atmos_7_1_4().unwrap();
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
        let normal = super::math::cross(a, b);
        let along = super::math::cross(normal, mid);
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
///
/// Mutation (run, fails): the `beyond > EPS` rejection removed from `facets`
/// (the large triangle `[0, 1, 2]` joins the three, and its gains, positive
/// across the cap, overlap theirs).
#[test]
fn a_triangle_with_a_speaker_inside_is_split() {
    let cap = layout(&[(0.0, 20.0), (120.0, 20.0), (-120.0, 20.0), (0.0, 90.0)]);
    assert_eq!(
        tuples_of(&cap),
        vec![vec![0, 1, 3], vec![0, 2, 3], vec![1, 2, 3]]
    );
}

/// A layout the method cannot tile is refused with the reason.
///
/// Mutations (each run, each fails): the duplicate-corner rule removed from
/// `facets` (the layout builds, with two identical tuples); the
/// coplanar-with-listener volume test removed from `facets` and from
/// `inverse3` (the great-circle triple is a tuple).
#[test]
fn a_layout_that_cannot_be_tiled_is_refused() {
    let make = |p: &[Position]| {
        let p: Vec<_> = p.iter().map(|&(a, e)| (Azimuth(a), Elevation(e))).collect();
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
