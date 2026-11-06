//! Signal helpers for the unit tests.

/// `len` samples of a unit sine at `hz`, starting at phase 0.
pub(crate) fn sine(hz: f64, rate: f64, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| libm::sin(2.0 * core::f64::consts::PI * hz * i as f64 / rate) as f32)
        .collect()
}

/// The complex amplitude of the `hz` component of `x`, as `(re, im)` of
/// `2/N * sum(x[n] * e^(-j w n))` (a unit sine at phase 0 reads `(0, -1)`).
///
/// Exact (no leakage) when `x` spans a whole number of periods of `hz`.
pub(crate) fn lock_in(x: &[f32], hz: f64, rate: f64) -> (f64, f64) {
    let w = 2.0 * core::f64::consts::PI * hz / rate;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
        let a = w * n as f64;
        re += f64::from(v) * libm::cos(a);
        im -= f64::from(v) * libm::sin(a);
    }
    let k = 2.0 / x.len() as f64;
    (re * k, im * k)
}

/// The amplitude of [`lock_in`].
pub(crate) fn amplitude_at(x: &[f32], hz: f64, rate: f64) -> f64 {
    let (re, im) = lock_in(x, hz, rate);
    re.hypot(im)
}
