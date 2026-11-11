use super::*;

const ALL: [CosineWindow; 5] = [
    CosineWindow::HANN,
    CosineWindow::HAMMING,
    CosineWindow::BLACKMAN,
    CosineWindow::BLACKMAN_HARRIS,
    CosineWindow::RECTANGULAR,
];

#[test]
fn periodic_hann_starts_at_zero_and_peaks_at_the_centre() {
    let w = CosineWindow::HANN.coefficients(8);
    assert_eq!(w.len(), 8);
    assert!(w[0].abs() < 1e-6, "first point is zero");
    assert!((w[4] - 1.0).abs() < 1e-6, "centre reaches unity");
    // Periodic: the last point is NOT zero, which is what distinguishes it
    // from the symmetric form and what makes it tile.
    assert!(w[7] > 0.0);
}

/// **The COLA rule, checked in both directions.**
///
/// Flat at the derived overlap *and* rippling at half of it. The second half
/// is what makes the rule tight rather than merely safe: a `cola_overlap`
/// that returned 16 for everything would pass the first assertion and
/// needlessly refuse grids that reconstruct perfectly well.
#[test]
fn the_cola_rule_is_tight() {
    for w in ALL {
        let overlap = w.cola_overlap();
        let flat = cola_ripple(&w, 64, overlap);
        assert!(
            flat < 1e-4,
            "{:?}-term window is not flat at its own {overlap}x: {flat:.2e}",
            w.terms()
        );

        if overlap > 1 {
            let looser = cola_ripple(&w, 64, overlap / 2);
            assert!(
                looser > 1e-3,
                "{:?}-term window claims it needs {overlap}x, but {}x is \
                 already flat ({looser:.2e}) — the rule is too conservative",
                w.terms(),
                overlap / 2
            );
        }
    }
}

/// **`coherent_gain` is `a₀`, and `a₀` is the measured mean.**
///
/// Two sizes, because the claim is also that it does not depend on `n` —
/// which is the entire justification for it being a closed form that takes
/// no length.
#[test]
fn coherent_gain_is_the_measured_mean_at_any_size() {
    for w in ALL {
        for size in [64usize, 1024] {
            let measured = w.coefficients(size).iter().sum::<f32>() / size as f32;
            let claimed = w.coherent_gain();
            assert!(
                (measured - claimed).abs() < 1e-6,
                "{:?}-term at n={size}: claims {claimed}, measures {measured}",
                w.terms()
            );
        }
    }
}

/// The trait's *default* implementations must agree with the closed forms
/// that override them.
///
/// Without this the overrides could drift from the maths they claim to
/// shortcut, and nothing would notice — the defaults are the only
/// independent statement of what the two functionals mean.
#[test]
fn the_closed_forms_agree_with_numeric_integration() {
    /// A window with no closed forms, so the trait defaults run.
    struct Numeric(CosineWindow);
    impl Window for Numeric {
        fn at(&self, t: f32) -> f32 {
            self.0.at(t)
        }
    }

    for w in ALL {
        let numeric = Numeric(w);
        assert!(
            (numeric.coherent_gain() - w.coherent_gain()).abs() < 1e-4,
            "{:?}-term: integrated {} vs closed form {}",
            w.terms(),
            numeric.coherent_gain(),
            w.coherent_gain()
        );
        assert_eq!(
            numeric.cola_overlap(),
            w.cola_overlap(),
            "{:?}-term: searched overlap disagrees with the derived one",
            w.terms()
        );
    }
}

/// Every window peaks at or below unity and never goes negative — one that
/// overshot would amplify the frame it was meant to taper.
///
/// Blackman-Harris is the one that could plausibly dip: it has the most
/// subtracting terms.
#[test]
fn no_window_overshoots_or_goes_negative() {
    for w in ALL {
        for &v in w.coefficients(128).iter() {
            assert!(
                (-1e-6..=1.0 + 1e-6).contains(&v),
                "{:?}-term produced {v}, outside [0, 1]",
                w.terms()
            );
        }
    }
}

/// `centred` is `at` shifted half a period, in `f64`: the same window,
/// peak at 0, zero at `±0.5` for the zero-ended ones.
#[test]
fn centred_is_the_periodic_window_shifted_half_a_period() {
    for w in [
        CosineWindow::HANN,
        CosineWindow::HAMMING,
        CosineWindow::BLACKMAN,
    ] {
        for i in 0..=40 {
            let x = f64::from(i) / 40.0 - 0.5;
            let periodic = f64::from(w.at((x + 0.5) as f32));
            assert!((w.centred(x) - periodic).abs() < 1e-6, "{w:?} at {x}");
        }
    }
    assert!(CosineWindow::BLACKMAN.centred(0.5).abs() < 1e-7);
    assert!(CosineWindow::BLACKMAN.centred(-0.5).abs() < 1e-7);
    assert!((CosineWindow::BLACKMAN.centred(0.0) - 1.0).abs() < 1e-7);
}

/// `RECTANGULAR` is exactly the identity, so a caller holding pre-windowed
/// samples can say so rather than passing a window that almost does nothing.
#[test]
fn rectangular_is_the_identity() {
    assert_eq!(CosineWindow::RECTANGULAR.coefficients(5), vec![1.0; 5]);
    assert_eq!(CosineWindow::RECTANGULAR.coherent_gain(), 1.0);
}

#[test]
fn degenerate_sizes_do_not_divide_by_zero() {
    for w in ALL {
        assert!(w.coefficients(0).is_empty(), "{:?}-term at 0", w.terms());
        assert_eq!(w.coefficients(1).len(), 1, "{:?}-term at 1", w.terms());
        assert!(
            w.coefficients(1)[0].is_finite(),
            "{:?}-term at size 1 is not finite",
            w.terms()
        );
    }
}

/// Equality is by coefficients, not by slice address — otherwise two
/// identical windows built at different call sites would compare unequal,
/// and `StftGeometry`'s `PartialEq` would report a grid as changed when
/// nothing about it had.
#[test]
fn windows_compare_by_their_coefficients() {
    assert_eq!(CosineWindow::new(&[0.5, 0.5]), CosineWindow::HANN);
    assert_ne!(CosineWindow::HANN, CosineWindow::HAMMING);
}
