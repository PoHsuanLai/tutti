use super::*;

/// The cutoff fit lands on the values rubato documents for it: 0.894 at 128
/// taps and 0.947 at 256, each within 0.001. Worked by hand from the fit's
/// constants, `1 / (1 + k1/n + k2/n^2 + k3/n^3)` at n = 128 is
/// `1 / (1 + 0.10738 + 0.00743 + 0.00284) = 0.8947`, so a slip in any of the
/// three coefficients moves it off the documented figure.
///
/// Mutation (run): `k1` 13.745_203 -> 12.0 → 128 taps gives 0.9058.
#[test]
fn the_cutoff_fit_matches_its_documented_values() {
    assert!((cutoff(128) - 0.894).abs() < 0.001, "{}", cutoff(128));
    assert!((cutoff(256) - 0.947).abs() < 0.001, "{}", cutoff(256));
}

/// The window is the squared four-term Blackman-Harris, from its closed
/// form `w(x) = (a - b cos(2 pi x/n) + c cos(4 pi x/n) - d cos(6 pi x/n))^2`
/// with `a, b, c, d = 0.35875, 0.48829, 0.14128, 0.01168`. At n = 64: the
/// peak at x = 32 is `(a + b + c + d)^2 = 1`; the start is
/// `(a - b + c - d)^2 = 0.00006^2 = 3.6e-9`; a quarter of the way in, the
/// cosines at 2 pi x/n = pi/2 give `(a - c)^2 = 0.21747^2 = 0.047293`; and it is
/// symmetric about the peak.
///
/// Mutations (run): `w * w` -> `w` gives 0.21747 at the quarter point and
/// 6e-5 at the start; zeroing `d` gives a peak of 0.98832^2 = 0.9768.
#[test]
fn the_window_is_the_squared_blackman_harris() {
    let n = 64;
    let w = window(n);
    assert!((w[n / 2] - 1.0).abs() < 1e-5, "peak {}", w[n / 2]);
    assert!(w[0] < 1e-6, "start {}", w[0]);
    assert!((w[n / 4] - 0.047_293).abs() < 1e-5, "quarter {}", w[n / 4]);
    let (peak, _) = w
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .unwrap();
    assert_eq!(peak, n / 2);
    for (x, (head, tail)) in w[1..].iter().zip(w[1..].iter().rev()).enumerate() {
        assert!((head - tail).abs() < 1e-6, "asymmetric at {}", x + 1);
    }
    // The same closed form, in f64, at every tap.
    for (x, &got) in w.iter().enumerate() {
        let t = 2.0 * std::f64::consts::PI * x as f64 / n as f64;
        let bh =
            0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos() - 0.01168 * (3.0 * t).cos();
        assert!((got as f64 - bh * bh).abs() < 1e-6, "tap {x}");
    }
}

/// The window sets the filter's response: a 256-tap, 0.9-cutoff filter is
/// 46 dB down at 0.93 of Nyquist (the squared window's wide transition; an
/// unsquared one is already at -105 dB there) and below -130 dB from 0.96
/// to Nyquist (the four-term window's sidelobes; a three-term one is at
/// -113 dB). Read as the magnitude of the taps' DFT in f64.
///
/// Mutations (run): `w * w` -> `w` reads -105 dB at 0.93 and fails the
/// first bound; `d` -> 0 reads -113 dB at 0.96 and fails the second.
#[test]
fn the_filter_has_the_squared_windows_transition_and_stopband() {
    let n = 256;
    let taps = sinc_filter(n, 0.9);
    let gain_db = |rel: f64| {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &t) in taps.iter().enumerate() {
            let w = std::f64::consts::PI * rel * (i as f64 - (n / 2) as f64);
            re += t as f64 * w.cos();
            im += t as f64 * w.sin();
        }
        20.0 * re.hypot(im).log10()
    };
    let edge = gain_db(0.93);
    assert!((-60.0..-35.0).contains(&edge), "0.93 Nyquist: {edge:.1} dB");
    for rel in [0.96, 0.97, 0.98, 0.99, 1.0] {
        let db = gain_db(rel);
        assert!(db < -130.0, "{rel} Nyquist: {db:.1} dB");
    }
}

/// The filter sums to 1 (DC gain 1), is symmetric about tap `n / 2`, and
/// peaks there.
///
/// Mutation (run): drop the `/= sum` normalisation → the sum is ~1/cutoff.
#[test]
fn the_sinc_has_unity_dc_gain_and_peaks_at_its_centre() {
    let taps = sinc_filter(441, 0.9);
    let sum: f32 = taps.iter().sum();
    assert!((sum - 1.0).abs() < 1e-5, "sum {sum}");
    let peak = taps
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .unwrap()
        .0;
    assert_eq!(peak, 441 / 2);
}

/// The block sizes are the rate pair's repeating unit times a count, so the
/// ratio is exact: 44.1 -> 48 k is 147 -> 160 per unit, and 1024 in 2
/// sub-chunks asks for 512 -> 4 units = 588 -> 640.
///
/// Mutation (run): derive `fft_size_out` from `source_hz` → 588 -> 588.
#[test]
fn block_sizes_are_whole_units_of_the_rate_pair() {
    let r = FftResampler::new(44_100, 48_000, 1024, 2, 1).unwrap();
    assert_eq!((r.fft_size_in, r.fft_size_out), (588, 640));
    assert_eq!(r.output_delay(), 320);
    let r = FftResampler::new(48_000, 44_100, 1024, 2, 1).unwrap();
    assert_eq!((r.fft_size_in, r.fft_size_out), (640, 588));
}

/// The block cap admits every real pair and refuses a runaway one. The
/// pairs are the common rates both ways, the coprime ones at the top of the
/// range (192000 <-> 44101, 44100 <-> 48001), and the largest preset-sized
/// block (8000 -> 192000 at a 32768-frame chunk in one piece, 786432
/// frames); 1 -> 192000 would plan an FFT of about 98 M points and is an
/// error, not an 800 MB allocation.
///
/// Mutations (run): `MAX_BLOCK` 1 << 20 -> 1 << 17 refuses the
/// large-chunk pairs (8000 -> 44100 at 32768 in one piece first); dropping
/// the `.filter` on the block size lets 1 -> 192000 through, which plans the
/// 98 M-point FFT (about 40 s) and fails the last assertion.
#[test]
fn the_block_cap_admits_real_pairs_and_refuses_a_runaway_one() {
    let rates = [
        8_000, 11_025, 22_050, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 44_101, 48_001,
    ];
    let ok = |a: usize, b: usize, chunk: usize, sub: usize| {
        let r = FftResampler::new(a, b, chunk, sub, 1);
        assert!(r.is_ok(), "{a} -> {b} at {chunk}/{sub}: {:?}", r.err());
    };
    for &a in &rates {
        for &b in &rates {
            ok(a, b, 1024, 2);
        }
    }
    for (a, b) in [(8_000, 192_000), (192_000, 44_101), (44_101, 192_000)] {
        for (chunk, sub) in [(512, 1), (4096, 8), (32_768, 32), (32_768, 1)] {
            ok(a, b, chunk, sub);
        }
    }
    let err = FftResampler::new(1, 192_000, 1024, 2, 1).err().unwrap();
    assert!(err.to_string().contains("block"), "{err}");
}

/// The block size is computed with checked arithmetic. A product that wraps
/// to 0 (`2^20` units of `2^44` frames is `2^64`) would slip under the cap if
/// it were wrapping.
///
/// Mutation (run): `checked_mul` -> `wrapping_mul` builds a zero-length
/// block, which panics in `RealFft` instead of returning the error.
#[cfg(target_pointer_width = "64")]
#[test]
fn an_overflowing_block_size_is_an_error() {
    assert!(FftResampler::new(1, 1 << 44, 1 << 20, 1, 1).is_err());
}

/// A zero rate is refused, not divided by.
#[test]
fn a_zero_rate_is_an_error() {
    assert!(FftResampler::new(0, 48_000, 1024, 2, 1).is_err());
    assert!(FftResampler::new(44_100, 0, 1024, 2, 1).is_err());
}
