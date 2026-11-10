//! Which speakers a source can be placed between.
//!
//! This is Pulkki's method (V. Pulkki, "Virtual sound source positioning using
//! vector base amplitude panning", JAES 45(6), 1997, and his 1999 extension to
//! speaker triplets), stated from the paper and written from it:
//!
//! - **Two dimensions.** Speakers on the horizontal plane are sorted by
//!   bearing and each pair of neighbours is a tuple, because the source lies
//!   between two speakers and only the two on either side of it are active. A
//!   pair that spans half the circle or more cannot place a source between its
//!   speakers (the listener is on or outside its chord), and a pair closer than
//!   a few degrees is a single point to the ear and a near-singular matrix, so
//!   neither is a tuple.
//! - **Three dimensions.** The speakers' directions are points on a sphere and
//!   the tuples are a triangulation of it by spherical triangles "as small as
//!   possible" with **no speaker inside any triangle** and **no two triangles
//!   overlapping**: the facets of the convex hull of the speaker directions.
//!   A triple is a facet exactly when no other speaker lies beyond its plane.
//!   (Speakers are on the sphere, so none can lie inside a triangle of three
//!   others on the plane: a speaker "inside" a spherical triangle is beyond the
//!   plane of its corners. One at a corner's own position is a duplicate, and a
//!   triangle with a duplicate corner is no tuple.) Where four or more speakers are coplanar
//!   the facet is a polygon and more than one triangulation tiles it; the one
//!   taken is the greedy one Pulkki's method describes, shorter connections
//!   first, a longer one that would cross an accepted one dropped, and a tie
//!   going to the lower speaker indices.
//!
//! A triple whose three directions are within a hair of coplanar with the
//! listener (a facet lying across the origin, such as three speakers on the
//! horizontal plane) spans no volume and has no inverse, so it is not a tuple;
//! a layout with no speaker below the horizon therefore has no tuple covering
//! the directions below it, and `SpeakerLayout::gains_into` retreats a source there
//! to the horizon.

use super::math::{Vec3, cross, direction, dot, sub, wrapped_deg};

/// A pair closer than this (degrees of bearing) is one point to the ear.
const MIN_PAIR_GAP: f64 = 5.0;

/// A pair this far apart (degrees of bearing) or more leaves the listener on
/// or beside its chord.
const MAX_PAIR_GAP: f64 = 175.0;

/// A triple whose direction matrix has a determinant below this spans no
/// volume.
const MIN_VOLUME: f64 = 1e-3;

/// Distances below this are zero: three speakers on a plane, a point on an
/// edge, two arcs sharing a point.
const EPS: f64 = 1e-9;

/// A direction whose dot product with a corner exceeds `1 - ` this is the
/// corner: a second speaker at the same place.
const COINCIDENT: f64 = 1e-12;

/// Edge lengths are compared as their cosines times this, rounded, so that
/// equal lengths tie whatever the last bit of the dot product.
const SHORTNESS_QUANTUM: f64 = 1e9;

/// The pairs of a horizontal layout, as indices into `azimuths` (degrees),
/// ordered from the lowest bearing.
pub(super) fn pairs(azimuths: &[f64]) -> Vec<[u16; 2]> {
    let n = azimuths.len();
    let bearing = |i: usize| wrapped_deg(azimuths[i]);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| bearing(a).total_cmp(&bearing(b)));
    let mut out = Vec::with_capacity(n);
    for at in 0..n {
        let (a, b) = (order[at], order[(at + 1) % n]);
        let mut gap = bearing(b) - bearing(a);
        if at + 1 == n {
            gap += 360.0;
        }
        if (MIN_PAIR_GAP..MAX_PAIR_GAP).contains(&gap) {
            out.push([a as u16, b as u16]);
        }
    }
    out
}

/// The triplets of a layout with height, as sorted index triples.
pub(super) fn triplets(positions: &[(f64, f64)]) -> Vec<[u16; 3]> {
    let dirs: Vec<Vec3> = positions
        .iter()
        .map(|&(azimuth, elevation)| direction(azimuth, elevation))
        .collect();
    let facets = facets(&dirs);
    let kept = accepted_edges(&dirs, &facets);
    facets
        .into_iter()
        .filter(|&[i, j, k]| {
            [(i, j), (i, k), (j, k)]
                .iter()
                .all(|edge| kept.contains(edge))
        })
        .collect()
}

/// The triples no speaker lies beyond: the facets of the convex hull that face
/// away from the listener, in index order.
fn facets(dirs: &[Vec3]) -> Vec<[u16; 3]> {
    let n = dirs.len();
    let mut out = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            'triple: for k in (j + 1)..n {
                let (a, b, c) = (dirs[i], dirs[j], dirs[k]);
                let volume = dot(a, cross(b, c));
                if volume.abs() < MIN_VOLUME {
                    continue;
                }
                // The plane's normal, pointing away from the listener.
                let raw = cross(sub(b, a), sub(c, a));
                let length = dot(raw, raw).sqrt();
                let sign = volume.signum();
                let normal = [
                    sign * raw[0] / length,
                    sign * raw[1] / length,
                    sign * raw[2] / length,
                ];
                let offset = dot(normal, a);
                for (m, &p) in dirs.iter().enumerate() {
                    if m == i || m == j || m == k {
                        continue;
                    }
                    let beyond = dot(normal, p) - offset;
                    if beyond > EPS {
                        continue 'triple;
                    }
                    // A second speaker at a corner's place: the layout has a
                    // duplicate, and no triangle of it is a tuple.
                    if [a, b, c]
                        .iter()
                        .any(|&corner| dot(corner, p) > 1.0 - COINCIDENT)
                    {
                        continue 'triple;
                    }
                }
                out.push([i as u16, j as u16, k as u16]);
            }
        }
    }
    out
}

/// The connections of the facets, shortest first, that cross no shorter one.
///
/// Facets of a convex hull never cross except inside a coplanar polygon, where
/// the shorter diagonal is the one Pulkki's method keeps. Returned as
/// `(low, high)` index pairs.
fn accepted_edges(dirs: &[Vec3], facets: &[[u16; 3]]) -> Vec<(u16, u16)> {
    let mut edges: Vec<(u16, u16)> = facets
        .iter()
        .flat_map(|&[i, j, k]| [(i, j), (i, k), (j, k)])
        .collect();
    edges.sort_unstable();
    edges.dedup();
    // Shortest first: the larger the cosine the shorter the arc. Quantised, so
    // that two edges of equal length (a symmetric layout) compare as equal and
    // the lower indices decide, whatever the last bit of their dot products.
    let shortness = |&(i, j): &(u16, u16)| {
        -(dot(dirs[usize::from(i)], dirs[usize::from(j)]) * SHORTNESS_QUANTUM).round() as i64
    };
    edges.sort_by_key(|e| (shortness(e), *e));
    let mut kept: Vec<(u16, u16)> = Vec::with_capacity(edges.len());
    for &(i, j) in &edges {
        let crossed = kept.iter().any(|&(c, d)| {
            c != i
                && c != j
                && d != i
                && d != j
                && arcs_cross(
                    dirs[usize::from(i)],
                    dirs[usize::from(j)],
                    dirs[usize::from(c)],
                    dirs[usize::from(d)],
                )
        });
        if !crossed {
            kept.push((i, j));
        }
    }
    kept.sort_unstable();
    kept
}

/// Whether the shorter great-circle arcs `ab` and `cd` cross in a point other
/// than an end.
fn arcs_cross(a: Vec3, b: Vec3, c: Vec3, d: Vec3) -> bool {
    let side = |normal: Vec3, p: Vec3| {
        let s = dot(normal, p);
        if s > EPS {
            1
        } else if s < -EPS {
            -1
        } else {
            0
        }
    };
    let (n_ab, n_cd) = (unit(cross(a, b)), unit(cross(c, d)));
    // Each arc's ends are on opposite sides of the other's great circle...
    if side(n_ab, c) * side(n_ab, d) != -1 || side(n_cd, a) * side(n_cd, b) != -1 {
        return false;
    }
    // ...and the circles' crossing nearer both arcs (not its antipode) is the
    // same one: an arc lies in the half-space of its midpoint.
    let p = cross(n_ab, n_cd);
    let mid = |x: Vec3, y: Vec3| [x[0] + y[0], x[1] + y[1], x[2] + y[2]];
    dot(p, mid(a, b)) * dot(p, mid(c, d)) > 0.0
}

fn unit(v: Vec3) -> Vec3 {
    let length = dot(v, v).sqrt();
    if length > 0.0 {
        [v[0] / length, v[1] / length, v[2] / length]
    } else {
        v
    }
}
