use super::*;

fn kernel(cutoff: f64) -> WindowedSinc {
    WindowedSinc::new(cutoff, 32.0, CosineWindow::BLACKMAN).unwrap()
}

/// The centre is `cutoff` (sinc and window are 1 there), and at cutoff 1
/// every other integer offset is a sinc zero crossing, so the kernel is
/// an exact interpolator: it reads samples back unchanged.
#[test]
fn the_centre_is_the_cutoff_and_integers_are_zero_crossings() {
    let k = kernel(1.0);
    assert!((k.weight(0.0) - 1.0).abs() < 1e-7);
    for m in 1..32 {
        assert!(k.weight(f64::from(m)).abs() < 1e-7, "offset {m}");
    }
    assert!((kernel(0.95).weight(0.0) - 0.95).abs() < 1e-7);
}

/// Unity DC gain: the kernel's samples at any fractional offset sum to 1
/// (cutoff 1 and 0.95 alike), so a constant keeps its level. The
/// truncation to 32 crossings leaves the sum within a few 1e-4.
#[test]
fn the_samples_at_any_offset_sum_to_one() {
    for cutoff in [1.0, 0.95] {
        let k = kernel(cutoff);
        let reach = k.half_width().ceil() as i64 + 1;
        for frac in [0.0, 0.25, 0.5, 0.731] {
            let sum: f64 = (-reach..=reach).map(|m| k.weight(frac - m as f64)).sum();
            assert!(
                (sum - 1.0).abs() < 5e-4,
                "cutoff {cutoff}, frac {frac}: {sum}"
            );
        }
    }
}

/// The kernel is even, and zero at and beyond its half-width, where the
/// Blackman window ends.
#[test]
fn it_is_even_and_ends_at_the_half_width() {
    let k = kernel(0.95);
    for t in [0.3, 1.7, 5.25, 20.0] {
        assert_eq!(k.weight(t), k.weight(-t));
    }
    let h = k.half_width();
    assert!(k.weight(h).abs() < 1e-7, "window end: {}", k.weight(h));
    assert_eq!(k.weight(h * 1.001), 0.0);
    assert_eq!(k.weight(-h * 1.001), 0.0);
}

/// A kernel needs a finite positive cutoff and crossing count.
#[test]
fn a_bad_shape_is_refused() {
    let w = CosineWindow::BLACKMAN;
    for (c, n) in [
        (0.0, 8.0),
        (-1.0, 8.0),
        (f64::NAN, 8.0),
        (1.0, 0.0),
        (1.0, f64::INFINITY),
    ] {
        assert!(WindowedSinc::new(c, n, w).is_none(), "{c} {n}");
    }
}
