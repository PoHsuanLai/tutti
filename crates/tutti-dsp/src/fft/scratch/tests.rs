use super::*;
use crate::{CosineWindow, Window};

#[test]
fn forward_finds_a_tone_in_the_right_bin() {
    let size = 1024;
    let bin = 64usize;
    let input: Vec<f32> = (0..size)
        .map(|i| (2.0 * core::f32::consts::PI * bin as f32 * i as f32 / size as f32).sin())
        .collect();
    let window = vec![1.0f32; size]; // rectangular, so the peak is sharp

    let mut fft = FftScratch::new();
    let mut out = vec![Complex::default(); size / 2 + 1];
    fft.forward(&input, &window, &mut out);

    let peak = out
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.norm().partial_cmp(&b.norm()).unwrap())
        .map(|(i, _)| i)
        .unwrap();
    assert_eq!(peak, bin);
}

/// Forward then inverse is the identity, up to float precision.
#[test]
fn forward_and_inverse_round_trip() {
    let size = 256;
    let input: Vec<f32> = (0..size).map(|i| (i as f32 / 7.0).sin() * 0.5).collect();
    let window = vec![1.0f32; size];

    let mut fft = FftScratch::new();
    let mut bins = vec![Complex::default(); size / 2 + 1];
    fft.forward(&input, &window, &mut bins);

    let mut out = vec![0.0f32; size];
    fft.inverse(&bins, &mut out);

    for (i, (a, b)) in input.iter().zip(&out).enumerate() {
        assert!((a - b).abs() < 1e-4, "sample {i}: {a} != {b}");
    }
}

/// The imaginary parts of DC (and Nyquist, for an even size) are ignored.
#[test]
fn inverse_ignores_imaginary_dc_and_nyquist() {
    for size in [64usize, 63] {
        let input: Vec<f32> = (0..size).map(|i| (i as f32 / 5.0).sin() + 0.25).collect();
        let window = vec![1.0f32; size];
        let mut fft = FftScratch::new();
        let mut bins = vec![Complex::default(); size / 2 + 1];
        fft.forward(&input, &window, &mut bins);
        bins[0].im = 0.0;
        if size % 2 == 0 {
            bins.last_mut().unwrap().im = 0.0;
        }
        let mut clean = vec![0.0f32; size];
        fft.inverse(&bins, &mut clean);

        bins[0].im = 3.0;
        if size % 2 == 0 {
            bins.last_mut().unwrap().im = -2.0;
        }
        let mut dirty = vec![0.0f32; size];
        fft.inverse(&bins, &mut dirty);
        assert_eq!(clean, dirty, "size {size}");
    }
}

/// The property that makes "pure in its inputs" true despite the `&mut`.
#[test]
fn reused_scratch_gives_identical_results() {
    let size = 512;
    let a: Vec<f32> = (0..size).map(|i| (i as f32 / 11.0).sin()).collect();
    let b: Vec<f32> = (0..size).map(|i| (i as f32 / 3.0).cos()).collect();
    let window = CosineWindow::HANN.coefficients(size);

    let mut shared = FftScratch::new();
    let mut first = vec![Complex::default(); size / 2 + 1];
    shared.forward(&a, &window, &mut first);
    // Run something else through the same scratch, then repeat.
    let mut other = vec![Complex::default(); size / 2 + 1];
    shared.forward(&b, &window, &mut other);
    let mut again = vec![Complex::default(); size / 2 + 1];
    shared.forward(&a, &window, &mut again);

    assert_eq!(first, again);

    // And a fresh scratch agrees with the reused one.
    let mut fresh = FftScratch::new();
    let mut clean = vec![Complex::default(); size / 2 + 1];
    fresh.forward(&a, &window, &mut clean);
    assert_eq!(first, clean);
}

/// Alternating sizes must not return a stale plan.
#[test]
fn switching_sizes_replans() {
    let mut fft = FftScratch::new();
    for size in [64usize, 128, 64, 256, 64] {
        let input: Vec<f32> = (0..size).map(|i| i as f32 / size as f32).collect();
        let window = vec![1.0f32; size];
        let mut out = vec![Complex::default(); size / 2 + 1];
        fft.forward(&input, &window, &mut out);

        let mut back = vec![0.0f32; size];
        fft.inverse(&out, &mut back);
        for (a, b) in input.iter().zip(&back) {
            assert!((a - b).abs() < 1e-4, "round trip failed at size {size}");
        }
    }
}

fn tone(size: usize, bin: f32, amp: f32, phase: f32) -> Vec<f32> {
    (0..size)
        .map(|i| amp * (core::f32::consts::TAU * bin * i as f32 / size as f32 + phase).sin())
        .collect()
}

fn spectrum(input: &[f32]) -> Vec<Complex> {
    let size = input.len();
    let mut out = vec![Complex::default(); size / 2 + 1];
    FftScratch::new().forward(input, &vec![1.0; size], &mut out);
    out
}

/// A rectangular-window sine of amplitude `a` on bin `k` has magnitude
/// `a * N / 2` there and (to f32 rounding) nothing elsewhere.
#[test]
fn a_sine_has_magnitude_half_n_times_amplitude_on_its_bin() {
    let (size, bin, amp) = (2048usize, 100usize, 0.75f32);
    let bins = spectrum(&tone(size, bin as f32, amp, 0.3));
    let expected = amp * size as f32 / 2.0;
    assert!(
        (bins[bin].norm() - expected).abs() < expected * 1e-4,
        "peak {} vs {expected}",
        bins[bin].norm()
    );
    for (k, b) in bins.iter().enumerate().filter(|(k, _)| *k != bin) {
        assert!(b.norm() < expected * 1e-3, "leak {} at bin {k}", b.norm());
    }
}

/// DC is the sum of the samples and Nyquist the alternating sum; both are
/// real for real input.
#[test]
fn dc_and_nyquist_bins_are_the_sum_and_the_alternating_sum() {
    let size = 64usize;
    let input: Vec<f32> = (0..size)
        .map(|i| ((i * 7 % 13) as f32 - 6.0) / 8.0)
        .collect();
    let bins = spectrum(&input);
    let dc: f32 = input.iter().sum();
    let nyq: f32 = input
        .iter()
        .enumerate()
        .map(|(i, x)| if i % 2 == 0 { *x } else { -*x })
        .sum();
    assert!((bins[0].re - dc).abs() < 1e-4 && bins[0].im == 0.0);
    assert!((bins[size / 2].re - nyq).abs() < 1e-4 && bins[size / 2].im == 0.0);
}

/// Parseval: `sum x^2 == (|X0|^2 + |X_{N/2}|^2 + 2 sum_{0<k<N/2} |Xk|^2) / N`
/// for even `N`, and with no Nyquist term for odd `N`. Both parities run.
#[test]
fn parseval_holds_for_even_and_odd_sizes() {
    for size in [256usize, 255, 1000, 7] {
        let input: Vec<f32> = (0..size)
            .map(|i| (i as f32 * 0.37).sin() + 0.5 * (i as f32 * 1.9).cos())
            .collect();
        let bins = spectrum(&input);
        let time: f32 = input.iter().map(|x| x * x).sum();
        let last = bins.len() - 1;
        let mut freq = bins[0].norm_sqr();
        for (k, b) in bins.iter().enumerate().skip(1) {
            let mirrored = !(size % 2 == 0 && k == last);
            freq += b.norm_sqr() * if mirrored { 2.0 } else { 1.0 };
        }
        freq /= size as f32;
        assert!(
            (time - freq).abs() < time * 1e-4,
            "size {size}: {time} vs {freq}"
        );
    }
}

/// An odd size round-trips and finds its tone, so the odd-length plan is
/// the one doing the work.
#[test]
fn odd_sizes_round_trip_and_find_the_tone() {
    let size = 301usize;
    let input = tone(size, 17.0, 1.0, 0.0);
    let bins = spectrum(&input);
    let peak = (0..bins.len())
        .max_by(|&a, &b| bins[a].norm().total_cmp(&bins[b].norm()))
        .unwrap();
    assert_eq!(peak, 17);
    let mut back = vec![0.0f32; size];
    FftScratch::new().inverse(&bins, &mut back);
    for (a, b) in input.iter().zip(&back) {
        assert!((a - b).abs() < 1e-4);
    }
}

/// Spectral centroid of two equal tones at bins 40 and 120 is bin 80.
#[test]
fn the_centroid_of_two_equal_tones_is_their_midpoint() {
    let size = 1024usize;
    let a = tone(size, 40.0, 1.0, 0.0);
    let b = tone(size, 120.0, 1.0, 0.0);
    let input: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
    let bins = spectrum(&input);
    let (num, den) = bins
        .iter()
        .enumerate()
        .fold((0.0f32, 0.0f32), |(n, d), (k, x)| {
            (n + k as f32 * x.norm(), d + x.norm())
        });
    assert!((num / den - 80.0).abs() < 0.05, "centroid {}", num / den);
}
