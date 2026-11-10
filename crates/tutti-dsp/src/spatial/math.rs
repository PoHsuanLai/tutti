//! The small `f64` vector and matrix arithmetic of the solve.
//!
//! Plain arrays, with the operations in the order the 2x2 and 3x3 inverse and
//! the matrix-vector product are conventionally written (cofactors from cross
//! products, products summed left to right), so the gains are the same bits on
//! every platform: nothing here is fused or reassociated, and the only
//! transcendental is `libm`'s.

/// A direction or a column of a matrix.
pub(super) type Vec3 = [f64; 3];

/// A 3x3 matrix as its columns.
pub(super) type Mat3 = [Vec3; 3];

/// A 2x2 matrix as its columns.
pub(super) type Mat2 = [[f64; 2]; 2];

const DEG: f64 = core::f64::consts::PI / 180.0;

/// A determinant smaller than this has no inverse: the columns are, to that
/// figure, dependent.
const DETERMINANT_FLOOR: f64 = 1e-10;

/// A bearing in degrees wrapped into `-180..180` (`Azimuth::wrap` is `f32`).
pub(super) fn wrapped_deg(deg: f64) -> f64 {
    deg - 360.0 * ((deg + 180.0) / 360.0).floor()
}

/// The unit vector of a bearing and height in degrees: 0 degrees bearing is
/// `+y` (front), 90 is `+x` (left), height is `+z`.
///
/// The types stop here: this is the trigonometry, in `f64` degrees.
pub(super) fn direction(azimuth_deg: f64, elevation_deg: f64) -> Vec3 {
    let (azi_sin, azi_cos) = libm::sincos(azimuth_deg * DEG);
    let (ele_sin, ele_cos) = libm::sincos(elevation_deg * DEG);
    [ele_cos * azi_sin, ele_cos * azi_cos, ele_sin]
}

pub(super) fn dot(a: Vec3, b: Vec3) -> f64 {
    (a[0] * b[0]) + (a[1] * b[1]) + (a[2] * b[2])
}

pub(super) fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - b[1] * a[2],
        a[2] * b[0] - b[2] * a[0],
        a[0] * b[1] - b[0] * a[1],
    ]
}

pub(super) fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub(super) fn scale(a: Vec3, s: f64) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

/// The inverse of the matrix whose columns are `c`, or `None` when its
/// determinant is smaller than [`DETERMINANT_FLOOR`] (the columns are,
/// to that figure, coplanar).
///
/// The cofactor columns are `c1 x c2`, `c2 x c0` and `c0 x c1`, the
/// determinant is `c2 . (c0 x c1)`, and the inverse is those columns scaled
/// by `1 / det` and transposed.
pub(super) fn inverse3(c: Mat3) -> Option<Mat3> {
    let t0 = cross(c[1], c[2]);
    let t1 = cross(c[2], c[0]);
    let t2 = cross(c[0], c[1]);
    let det = dot(c[2], t2);
    if det.abs() < DETERMINANT_FLOOR {
        return None;
    }
    let inv_det = det.recip();
    let m = [scale(t0, inv_det), scale(t1, inv_det), scale(t2, inv_det)];
    Some([
        [m[0][0], m[1][0], m[2][0]],
        [m[0][1], m[1][1], m[2][1]],
        [m[0][2], m[1][2], m[2][2]],
    ])
}

/// `m * v`: the columns scaled by the components and summed left to right.
pub(super) fn mul3(m: Mat3, v: Vec3) -> Vec3 {
    let mut res = scale(m[0], v[0]);
    let b = scale(m[1], v[1]);
    res = [res[0] + b[0], res[1] + b[1], res[2] + b[2]];
    let c = scale(m[2], v[2]);
    [res[0] + c[0], res[1] + c[1], res[2] + c[2]]
}

/// The inverse of the 2x2 matrix whose columns are `x` and `y`, or `None`
/// when its determinant is smaller than [`DETERMINANT_FLOOR`].
pub(super) fn inverse2(x: [f64; 2], y: [f64; 2]) -> Option<Mat2> {
    let det = x[0] * y[1] - x[1] * y[0];
    if det.abs() < DETERMINANT_FLOOR {
        return None;
    }
    let inv_det = det.recip();
    Some([
        [y[1] * inv_det, x[1] * -inv_det],
        [y[0] * -inv_det, x[0] * inv_det],
    ])
}

/// `m * v` for a 2x2 matrix.
pub(super) fn mul2(m: Mat2, v: [f64; 2]) -> [f64; 2] {
    [
        (m[0][0] * v[0]) + (m[1][0] * v[1]),
        (m[0][1] * v[0]) + (m[1][1] * v[1]),
    ]
}
